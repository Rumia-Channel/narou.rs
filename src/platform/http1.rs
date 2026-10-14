//! Minimal HTTP/1.1 client codec for the Worker's raw-socket transport.
//!
//! The Worker fetches sites through the Fetch API first; when that path is
//! refused outright (403 from an edge filter) it retries over `connect()`
//! sockets, which leave from a different egress prefix. Sockets carry raw
//! TCP bytes, so this module supplies the request encoder and the streaming
//! response parser that transport needs.
//!
//! Pure byte-oriented logic with no `worker`, `web_sys`, or `tokio`
//! dependencies so it can be unit-tested on the host.

use super::http::{HttpMethod, is_safe_header_name, is_safe_header_value};
use crate::error::{NarouError, Result};

/// Upper bound for a response head (status line + headers).
///
/// Bodies are capped by the caller-provided limit (`MAX_WORKER_HTTP_BODY` on
/// the Worker). The head is metadata — a site sending 16 MiB of headers is
/// broken, not large — so a small fixed cap guards the buffer before the
/// framing is known. 64 KiB covers pathological `Set-Cookie` floods.
pub const MAX_HTTP1_HEAD: usize = 64 * 1024;

/// Everything the raw socket needs about a request URL: where to connect and
/// what to put on the wire.
#[derive(Debug, Clone)]
pub struct SocketTarget {
    /// Hostname for `connect()` (IPv6 literal without brackets).
    pub host: String,
    /// `Host` header value (bracketed IPv6, explicit port kept).
    pub authority: String,
    /// TCP port (explicit or scheme default).
    pub port: u16,
    /// Whether the scheme requires TLS (`secure_transport = On`).
    pub tls: bool,
    /// Origin-form request target (`path?query`, `/` when empty).
    pub target: String,
}

impl SocketTarget {
    pub fn parse(url: &str) -> Result<Self> {
        let parsed =
            url::Url::parse(url).map_err(|e| NarouError::Http(format!("invalid URL: {e}")))?;
        let (tls, default_port) = match parsed.scheme() {
            "https" => (true, 443),
            "http" => (false, 80),
            scheme => {
                return Err(NarouError::Http(format!(
                    "unsupported URL scheme for socket transport: {scheme}"
                )));
            }
        };
        let Some(host) = parsed.host_str() else {
            return Err(NarouError::Http("URL host is missing".to_string()));
        };
        // `host_str` keeps IPv6 brackets (`[::1]`); `connect()` wants the bare
        // address while the Host header wants them back on.
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let host_name = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.clone()
        };
        let authority = match parsed.port() {
            Some(port) => format!("{host_name}:{port}"),
            None => host_name,
        };
        let mut target = parsed.path().to_string();
        if target.is_empty() {
            target.push('/');
        }
        if let Some(query) = parsed.query() {
            target.push('?');
            target.push_str(query);
        }
        Ok(Self {
            host,
            authority,
            port: parsed.port().unwrap_or(default_port),
            tls,
            target,
        })
    }

}

/// Headers the transport controls; duplicates from the caller would produce
/// an ambiguous request.
fn is_transport_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("host")
        || name.eq_ignore_ascii_case("connection")
        || name.eq_ignore_ascii_case("accept-encoding")
        || name.eq_ignore_ascii_case("content-length")
        || name.eq_ignore_ascii_case("transfer-encoding")
        || name.eq_ignore_ascii_case("expect")
        || name.eq_ignore_ascii_case("upgrade")
}

/// Serialize the request head with origin-form target, `Host`, caller headers,
/// an optional body length and `Connection: close`.
///
/// `Connection: close` makes the server's EOF the body terminator when no
/// length is declared. No `Accept-Encoding` is emitted: the socket path has
/// no decompressor, and omitting the header is the only portable way to ask
/// for an identity body. Header names and values are checked with the shared
/// rules (`platform::http` — RFC token names bounded to
/// `MAX_HEADER_NAME_BYTES`, no control bytes in values); the
/// transport-controlled names above are dropped as well.
pub fn encode_request(target: &SocketTarget, method: HttpMethod, headers: &[(String, String)], body_length: Option<usize>) -> Vec<u8> {
    use std::io::Write;
    let mut request = Vec::with_capacity(256 + target.authority.len() + target.target.len());
    request.extend_from_slice(match method {
        HttpMethod::Get => b"GET",
        HttpMethod::Post => b"POST",
        HttpMethod::Put => b"PUT",
        HttpMethod::Delete => b"DELETE",
    });
    request.push(b' ');
    request.extend_from_slice(target.target.as_bytes());
    request.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    request.extend_from_slice(target.authority.as_bytes());
    request.extend_from_slice(b"\r\n");
    for (name, value) in headers {
        if !is_safe_header_name(name) || !is_safe_header_value(value) || is_transport_header(name) {
            continue;
        }
        request.extend_from_slice(name.as_bytes());
        request.extend_from_slice(b": ");
        request.extend_from_slice(value.as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    if let Some(length) = body_length {
        write!(&mut request, "Content-Length: {length}\r\n").expect("writing to a Vec is infallible");
    }
    request.extend_from_slice(b"Connection: close\r\n\r\n");
    request
}

/// How the response body is delimited once the head has been parsed.
#[derive(Debug)]
enum BodyFraming {
    /// `Content-Length` bytes remain.
    Fixed(usize),
    /// `Transfer-Encoding: chunked`.
    Chunked,
    /// No length information — read until the peer closes the connection.
    UntilClose,
}

#[derive(Debug)]
enum Stage {
    Head,
    FixedBody {
        remaining: usize,
    },
    ChunkSize,
    ChunkData {
        remaining: usize,
    },
    /// The CRLF after a chunk's data.
    ChunkEnd,
    /// Trailer fields after the zero chunk.
    Trailers,
    Done,
}

/// Streaming HTTP/1.1 response parser. Feed socket bytes with [`feed`];
/// when it returns `true` the response is complete and [`into_response`]
/// yields it. On EOF call [`finish`].
///
/// [`feed`]: Self::feed
/// [`into_response`]: Self::into_response
/// [`finish`]: Self::finish
pub struct Http1ResponseDecoder {
    buf: Vec<u8>,
    status: u16,
    headers: Vec<(String, String)>,
    framing: BodyFraming,
    stage: Stage,
    body: Vec<u8>,
    body_limit: usize,
}

impl Http1ResponseDecoder {
    /// `body_limit` is the shared response-size cap
    /// (`MAX_WORKER_HTTP_BODY` on the Worker); exceeding it fails loudly
    /// instead of buffering forever.
    pub fn new(body_limit: usize) -> Self {
        Self {
            buf: Vec::new(),
            status: 0,
            headers: Vec::new(),
            framing: BodyFraming::UntilClose,
            stage: Stage::Head,
            body: Vec::new(),
            body_limit,
        }
    }

    /// Consume bytes; `true` means the response is fully parsed.
    ///
    /// Head / trailer blocks are bounded by [`MAX_HTTP1_HEAD`]; the body by
    /// `body_limit`. Bytes arriving after the response is complete (a next
    /// request's data, or a server sending past `Content-Length`) are dropped.
    pub fn feed(&mut self, mut input: &[u8]) -> Result<bool> {
        loop {
            match self.stage {
                Stage::Head => {
                    if !self.buffer_metadata(&mut input)? {
                        return Ok(false);
                    }
                    let delimiter_len = if self.buf.ends_with(b"\r\n\r\n") { 4 } else { 2 };
                    let (status, headers) = parse_head(&self.buf[..self.buf.len() - delimiter_len])?;
                    self.buf.clear();
                    self.status = status;
                    self.headers = headers;
                    if (100..200).contains(&status) && status != 101 {
                        continue;
                    }
                    if matches!(status, 101 | 204 | 304) {
                        self.stage = Stage::Done;
                        continue;
                    }
                    self.framing = self.framing_for()?;
                    self.stage = match self.framing {
                        BodyFraming::Fixed(0) => Stage::Done,
                        BodyFraming::Fixed(remaining) => {
                            if remaining > self.body_limit {
                                return Err(NarouError::Http(
                                    "socket response body exceeds limit".to_string(),
                                ));
                            }
                            Stage::FixedBody { remaining }
                        }
                        BodyFraming::Chunked => Stage::ChunkSize,
                        BodyFraming::UntilClose => Stage::FixedBody { remaining: usize::MAX },
                    };
                }
                Stage::FixedBody { remaining } => {
                    let take = remaining.min(input.len());
                    self.append_body(&input[..take])?;
                    input = &input[take..];
                    let remaining = remaining - take;
                    if remaining == 0 {
                        self.stage = Stage::Done;
                    } else {
                        self.stage = Stage::FixedBody { remaining };
                        return Ok(false);
                    }
                }
                Stage::ChunkSize => {
                    if !self.buffer_metadata(&mut input)? {
                        return Ok(false);
                    }
                    let size = parse_chunk_size(&self.buf[..self.buf.len() - 2])?;
                    self.buf.clear();
                    if size > self.body_limit.saturating_sub(self.body.len()) {
                        return Err(NarouError::Http(
                            "socket response body exceeds limit".to_string(),
                        ));
                    }
                    self.stage = if size == 0 {
                        Stage::Trailers
                    } else {
                        Stage::ChunkData { remaining: size }
                    };
                }
                Stage::ChunkData { remaining } => {
                    let take = remaining.min(input.len());
                    self.append_body(&input[..take])?;
                    input = &input[take..];
                    let remaining = remaining - take;
                    self.stage = if remaining == 0 {
                        Stage::ChunkEnd
                    } else {
                        Stage::ChunkData { remaining }
                    };
                    if take == 0 {
                        return Ok(false);
                    }
                }
                Stage::ChunkEnd => {
                    let take = (2 - self.buf.len()).min(input.len());
                    self.buf.extend_from_slice(&input[..take]);
                    input = &input[take..];
                    if self.buf.len() < 2 {
                        return Ok(false);
                    }
                    if self.buf != b"\r\n" {
                        return Err(NarouError::Http(
                            "chunk data not followed by CRLF".to_string(),
                        ));
                    }
                    self.buf.clear();
                    self.stage = Stage::ChunkSize;
                }
                Stage::Trailers => {
                    if !self.buffer_metadata(&mut input)? {
                        return Ok(false);
                    }
                    self.buf.clear();
                    self.stage = Stage::Done;
                }
                Stage::Done => return Ok(true),
            }
        }
    }

    /// Buffer metadata only, stopping at its delimiter before any coalesced
    /// body bytes. The bounded buffer also covers split delimiters and the
    /// complete trailer section, rather than resetting the cap per line.
    fn buffer_metadata(&mut self, input: &mut &[u8]) -> Result<bool> {
        while let Some((&byte, rest)) = input.split_first() {
            if self.buf.len() == MAX_HTTP1_HEAD {
                return Err(NarouError::Http(
                    "socket response metadata exceeds limit".to_string(),
                ));
            }
            self.buf.push(byte);
            *input = rest;
            let complete = match self.stage {
                Stage::Head => self.buf.ends_with(b"\r\n\r\n") || self.buf.ends_with(b"\n\n"),
                Stage::ChunkSize => self.buf.ends_with(b"\r\n"),
                Stage::Trailers => self.buf == b"\r\n" || self.buf.ends_with(b"\r\n\r\n"),
                _ => unreachable!("only metadata stages buffer metadata"),
            };
            if complete {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn append_body(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.len() > self.body_limit.saturating_sub(self.body.len()) {
            return Err(NarouError::Http(
                "socket response body exceeds limit".to_string(),
            ));
        }
        self.body.extend_from_slice(bytes);
        Ok(())
    }

    /// Complete the response once `feed` reported `true`.
    pub fn into_response(self) -> Result<crate::platform::http::HttpResponse> {
        if !matches!(self.stage, Stage::Done) {
            return Err(NarouError::Http(
                "socket response is incomplete".to_string(),
            ));
        }
        Ok(crate::platform::http::HttpResponse {
            status: self.status,
            headers: self.headers,
            body: self.body,
        })
    }

    /// Resolve the response at connection EOF.
    ///
    /// A `Content-Length`/`chunked` body cut short is an explicit error — a
    /// truncated section must not be stored as complete. A `Connection:
    /// close` body ends at EOF by definition.
    pub fn finish(self) -> Result<crate::platform::http::HttpResponse> {
        match self.stage {
            Stage::Done => self.into_response(),
            // EOF mid-head: nothing parseable was received.
            Stage::Head => Err(NarouError::Http(
                "socket response ended before headers completed".to_string(),
            )),
            Stage::FixedBody { .. } if matches!(self.framing, BodyFraming::UntilClose) => {
                // until-close bodies are complete at EOF whatever remains.
                Ok(crate::platform::http::HttpResponse {
                    status: self.status,
                    headers: self.headers,
                    body: self.body,
                })
            }
            _ => Err(NarouError::Http(
                "socket response body truncated by connection close".to_string(),
            )),
        }
    }

    fn framing_for(&self) -> Result<BodyFraming> {
        let chunked = self.headers.iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
            .any(|(_, value)| value.split(',').any(|part| part.trim().eq_ignore_ascii_case("chunked")));
        if chunked {
            return Ok(BodyFraming::Chunked);
        }
        let mut length = None;
        for (_, value) in self.headers.iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        {
            let value = value.trim();
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(NarouError::Http("invalid socket Content-Length".into()));
            }
            let parsed = value.parse::<usize>()
                .map_err(|_| NarouError::Http("invalid socket Content-Length".into()))?;
            if length.is_some_and(|previous| previous != parsed) {
                return Err(NarouError::Http("conflicting socket Content-Length".into()));
            }
            length = Some(parsed);
        }
        Ok(length.map(BodyFraming::Fixed).unwrap_or(BodyFraming::UntilClose))
    }
}


fn parse_head(head: &[u8]) -> Result<(u16, Vec<(String, String)>)> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split('\n');
    let status_line = lines
        .next()
        .ok_or_else(|| NarouError::Http("empty socket response".to_string()))?
        .trim_end_matches('\r');
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/") {
        return Err(NarouError::Http(format!(
            "invalid status line: {status_line}"
        )));
    }
    let status = parts
        .next()
        .and_then(|code| code.trim().parse::<u16>().ok())
        .ok_or_else(|| NarouError::Http(format!("invalid status line: {status_line}")))?;
    let mut headers = Vec::new();
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            headers.push((name.to_string(), value.trim().to_string()));
        } else if let Some((_, value)) = headers.last_mut() {
            // Obsolete folded line: append to the previous header's value.
            if !line.trim().is_empty() {
                value.push(' ');
                value.push_str(line.trim());
            }
        }
    }
    Ok((status, headers))
}

fn parse_chunk_size(line: &[u8]) -> Result<usize> {
    let text = std::str::from_utf8(line)
        .map_err(|_| NarouError::Http("chunk size is not ASCII".to_string()))?;
    let number = text.split(';').next().unwrap_or_default().trim();
    usize::from_str_radix(number, 16)
        .map_err(|_| NarouError::Http(format!("invalid chunk size: {text}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(url: &str) -> SocketTarget {
        SocketTarget::parse(url).unwrap()
    }

    fn decode(bytes: &[u8]) -> Result<crate::platform::http::HttpResponse> {
        let mut decoder = Http1ResponseDecoder::new(1024);
        if decoder.feed(bytes)? {
            return decoder.into_response();
        }
        decoder.finish()
    }

    /// Feed `bytes` one byte at a time to exercise every partial boundary.
    fn decode_bytewise(bytes: &[u8]) -> Result<crate::platform::http::HttpResponse> {
        let mut decoder = Http1ResponseDecoder::new(1024);
        for byte in bytes {
            if decoder.feed(&[*byte])? {
                return decoder.into_response();
            }
        }
        decoder.finish()
    }

    #[test]
    fn request_has_origin_form_and_close() {
        let bytes = encode_request(
            &target("https://example.jp:8443/path/to?q=1&r=2"),
            HttpMethod::Get,
            &[
                ("User-Agent".to_string(), "narou/1".to_string()),
                ("Cookie".to_string(), "a=b".to_string()),
            ],
            None,
        );
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("GET /path/to?q=1&r=2 HTTP/1.1\r\n"));
        assert!(text.contains("Host: example.jp:8443\r\n"));
        assert!(text.ends_with("Connection: close\r\n\r\n"));
    }

    #[test]
    fn request_drops_framing_and_injected_headers() {
        let long_name = format!("X-{}", "a".repeat(80));
        let bytes = encode_request(
            &target("https://example.jp/"),
            HttpMethod::Post,
            &[
                ("Host".to_string(), "evil.jp".to_string()),
                ("Connection".to_string(), "keep-alive".to_string()),
                ("Accept-Encoding".to_string(), "gzip".to_string()),
                ("Content-Length".to_string(), "5".to_string()),
                ("X-Bad".to_string(), "a\r\nInjected: 1".to_string()),
                ("Bad Name".to_string(), "value".to_string()),
                ("X-Nul".to_string(), "value\u{0}".to_string()),
                (long_name.clone(), "value".to_string()),
                ("X.Token".to_string(), "kept".to_string()),
            ],
            Some(2),
        );
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text.matches("Host:").count(), 1);
        assert!(text.contains("Host: example.jp\r\n"));
        assert!(!text.contains("evil.jp"));
        assert!(!text.contains("gzip"));
        assert!(!text.contains("Injected"));
        assert!(text.contains("Content-Length: 2\r\n"));
        assert!(!text.contains("Content-Length: 5\r\n"));
        assert!(!text.contains("keep-alive"));
        assert!(!text.contains("Bad Name"));
        assert!(!text.contains(&long_name));
        assert!(!text.contains("X-Nul"));
        assert!(text.contains("X.Token: kept\r\n"));
        assert!(text.ends_with("Connection: close\r\n\r\n"));
    }

    #[test]
    fn repeated_response_headers_survive_interim_heads() {
        let response = decode(
            b"HTTP/1.1 100 Continue\r\nX-Interim: 1\r\n\r\nHTTP/1.1 200 OK\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\nContent-Length: 2\r\n\r\nok",
        )
        .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.header_values("set-cookie"), vec!["a=1", "b=2"]);
        assert!(response.header("x-interim").is_none());
        assert_eq!(response.body, b"ok");
    }

    #[test]
    fn target_defaults_and_ipv6() {
        let https = target("https://example.jp/novel");
        assert_eq!(
            (https.host.as_str(), https.port, https.tls),
            ("example.jp", 443, true)
        );
        assert_eq!(https.target, "/novel");

        let bare = target("http://example.jp");
        assert_eq!(
            (bare.port, bare.tls, bare.target.as_str()),
            (80, false, "/")
        );

        let v6 = target("http://[2001:db8::1]:8080/x");
        assert_eq!(v6.host, "2001:db8::1");
        assert_eq!(v6.authority, "[2001:db8::1]:8080");

        assert!(SocketTarget::parse("ftp://example.jp/").is_err());
        assert!(SocketTarget::parse("not a url").is_err());
        assert!(SocketTarget::parse("https://").is_err());
    }


    #[test]
    fn content_length_response() {
        let response = decode(
            b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nContent-Type: text/plain\r\n\r\nhello world",
        )
        .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"hello world");
        assert_eq!(response.header("content-type"), Some("text/plain"));
    }

    #[test]
    fn content_length_with_pending_bytes_stops_cleanly() {
        // Two pipelined bytes after the body must not corrupt it.
        let mut decoder = Http1ResponseDecoder::new(1024);
        let done = decoder
            .feed(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabcZZ")
            .unwrap();
        assert!(done);
        let response = decoder.into_response().unwrap();
        assert_eq!(response.body, b"abc");
    }

    #[test]
    fn header_names_are_case_insensitive() {
        let response =
            decode(b"HTTP/1.0 302 Found\r\nLOCATION: /there\r\ncontent-LENGTH: 0\r\n\r\n").unwrap();
        assert_eq!(response.status, 302);
        assert_eq!(response.header("location"), Some("/there"));
        assert!(response.body.is_empty());
    }

    #[test]
    fn chunked_body_is_decoded() {
        let response = decode(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n5;ext=1\r\npedia\r\n0\r\nX-Trailer: ignored\r\n\r\n",
        )
        .unwrap();
        assert_eq!(response.body, b"Wikipedia");
    }

    #[test]
    fn chunked_bytewise() {
        let response = decode_bytewise(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked\r\n\r\na\r\n0123456789\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!(response.body, b"0123456789");
    }

    #[test]
    fn body_until_close() {
        let response =
            decode(b"HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\n\r\nbody without length")
                .unwrap();
        assert_eq!(response.body, b"body without length");
    }

    #[test]
    fn interim_response_is_skipped() {
        let response =
            decode(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"ok");
    }

    #[test]
    fn truncated_content_length_is_an_error() {
        let result = decode(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort");
        assert!(result.is_err());
    }

    #[test]
    fn truncated_chunked_is_an_error() {
        let result = decode(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nabc");
        assert!(result.is_err());
    }

    #[test]
    fn oversized_body_is_rejected() {
        let mut decoder = Http1ResponseDecoder::new(8);
        let result = decoder.feed(b"HTTP/1.1 200 OK\r\nContent-Length: 64\r\n\r\n0123456789abcdef");
        assert!(result.is_err());
    }

    #[test]
    fn oversized_head_is_rejected() {
        let mut decoder = Http1ResponseDecoder::new(1024);
        let huge = vec![b'x'; MAX_HTTP1_HEAD + 1];
        assert!(decoder.feed(&huge).is_err());
    }

    #[test]
    fn a_head_at_the_metadata_limit_accepts_coalesced_content() {
        let prefix = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nX-Large: ";
        let suffix = b"\r\n\r\n";
        let mut wire = prefix.to_vec();
        wire.extend(std::iter::repeat_n(
            b'x', MAX_HTTP1_HEAD - prefix.len() - suffix.len(),
        ));
        wire.extend_from_slice(suffix);
        wire.extend_from_slice(b"ok");
        let mut decoder = Http1ResponseDecoder::new(2);
        assert!(decoder.feed(&wire).unwrap());
        assert_eq!(decoder.into_response().unwrap().body, b"ok");
    }

    #[test]
    fn coalesced_head_and_large_body_are_consumed_without_a_metadata_failure() {
        let body = vec![b'x'; MAX_HTTP1_HEAD + 1024];
        let mut fixed = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        fixed.extend_from_slice(&body);
        let mut decoder = Http1ResponseDecoder::new(body.len());
        assert!(decoder.feed(&fixed).unwrap());
        assert_eq!(decoder.into_response().unwrap().body, body);

        let mut chunked = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n", body.len(),
        ).into_bytes();
        chunked.extend_from_slice(&body);
        chunked.extend_from_slice(b"\r\n0\r\nX-Trailer: fine\r\n\r\n");
        // Unrelated bytes after the trailer delimiter do not count as metadata.
        chunked.extend(std::iter::repeat_n(b'z', MAX_HTTP1_HEAD + 1));
        let mut decoder = Http1ResponseDecoder::new(body.len());
        assert!(decoder.feed(&chunked).unwrap());
        assert_eq!(decoder.into_response().unwrap().body, body);
    }

    #[test]
    fn completed_and_fragmented_oversized_metadata_is_rejected() {
        let mut head = b"HTTP/1.1 200 OK\r\nX-Large: ".to_vec();
        head.extend(std::iter::repeat_n(b'x', MAX_HTTP1_HEAD));
        head.extend_from_slice(b"\r\n\r\nbody");
        assert!(Http1ResponseDecoder::new(1024).feed(&head).is_err());

        let mut decoder = Http1ResponseDecoder::new(1024);
        decoder.feed(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n").unwrap();
        let line = b"X-Trailer: value\r\n";
        for _ in 0..MAX_HTTP1_HEAD / line.len() {
            assert!(!decoder.feed(line).unwrap());
        }
        assert!(decoder.feed(line).is_err());

        let mut chunk_size = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1;".to_vec();
        chunk_size.extend(std::iter::repeat_n(b'x', MAX_HTTP1_HEAD));
        chunk_size.extend_from_slice(b"\r\na\r\n0\r\n\r\n");
        assert!(Http1ResponseDecoder::new(1024).feed(&chunk_size).is_err());
    }

    #[test]
    fn malformed_content_length_never_falls_back_to_a_complete_close_body() {
        for bytes in [
            b"HTTP/1.1 200 OK\r\nContent-Length: garbage\r\n\r\nshort".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\nshort".as_slice(),
        ] {
            assert!(decode(bytes).is_err());
        }
        let mut decoder = Http1ResponseDecoder::new(MAX_HTTP1_HEAD * 2);
        let mut wire = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", MAX_HTTP1_HEAD + 2).into_bytes();
        wire.extend(std::iter::repeat_n(b'x', MAX_HTTP1_HEAD + 1));
        assert!(!decoder.feed(&wire).unwrap());
        assert!(decoder.finish().unwrap_err().to_string().contains("truncated"));
    }

    #[test]
    fn eof_before_headers_is_an_error() {
        let mut decoder = Http1ResponseDecoder::new(1024);
        decoder.feed(b"HTTP/1.1 200 OK\r\nPartial: ").unwrap();
        assert!(decoder.finish().is_err());
    }

    #[test]
    fn no_body_statuses_ignore_framing() {
        let response = decode(b"HTTP/1.1 304 Not Modified\r\n\r\n").unwrap();
        assert_eq!(response.status, 304);
        assert!(response.body.is_empty());
    }

    #[test]
    fn bad_chunk_separator_is_an_error() {
        let result =
            decode(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nabXX0\r\n\r\n");
        assert!(result.is_err());
    }

    #[test]
    fn invalid_status_line_is_an_error() {
        assert!(decode(b"garbage\r\n\r\nbody").is_err());
        assert!(decode(b"HTTP/1.1 xyz Bad\r\n\r\n").is_err());
    }
}

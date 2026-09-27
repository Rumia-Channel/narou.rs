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

    /// Resolve `Location` against this URL for the next redirect hop.
    pub fn join(&self, location: &str) -> Result<Self> {
        // Re-bracket IPv6 literals: the stored host is bare for `connect()`.
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let base = format!(
            "{}://{}:{}{}",
            if self.tls { "https" } else { "http" },
            host,
            self.port,
            self.target
        );
        let next = url::Url::parse(&base)
            .and_then(|base| base.join(location))
            .map_err(|e| NarouError::Http(format!("invalid redirect location: {e}")))?;
        Self::parse(next.as_str())
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

/// Serialize `GET <target> HTTP/1.1` with `Host`, caller headers and
/// `Connection: close`.
///
/// `Connection: close` makes the server's EOF the body terminator when no
/// length is declared. No `Accept-Encoding` is emitted: the socket path has
/// no decompressor, and omitting the header is the only portable way to ask
/// for an identity body. Headers that would corrupt the request line or the
/// framing (CR/LF inside a value, `Host`/`Connection`/…) are dropped.
pub fn encode_request(target: &SocketTarget, headers: &[(String, String)]) -> Vec<u8> {
    let mut request = Vec::with_capacity(256 + target.authority.len() + target.target.len());
    request.extend_from_slice(b"GET ");
    request.extend_from_slice(target.target.as_bytes());
    request.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    request.extend_from_slice(target.authority.as_bytes());
    request.extend_from_slice(b"\r\n");
    for (name, value) in headers {
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-!#$%&'*+.^_`|~".contains(&b))
            || value.contains(['\r', '\n'])
            || is_transport_header(name)
        {
            continue;
        }
        request.extend_from_slice(name.as_bytes());
        request.extend_from_slice(b": ");
        request.extend_from_slice(value.as_bytes());
        request.extend_from_slice(b"\r\n");
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
    pub fn feed(&mut self, chunk: &[u8]) -> Result<bool> {
        if matches!(self.stage, Stage::Done) {
            return Ok(true);
        }
        if matches!(self.stage, Stage::Head | Stage::Trailers | Stage::ChunkSize)
            && self.buf.len() + chunk.len() > MAX_HTTP1_HEAD
        {
            return Err(NarouError::Http(
                "socket response header exceeds limit".to_string(),
            ));
        }
        self.buf.extend_from_slice(chunk);
        loop {
            match self.stage {
                Stage::Head => {
                    let Some(head_end) = find_head_end(&self.buf) else {
                        return Ok(false);
                    };
                    let head: Vec<u8> = self.buf.drain(..head_end.at).collect();
                    self.buf.drain(..head_end.skip);
                    let (status, headers) = parse_head(&head)?;
                    self.status = status;
                    self.headers = headers;
                    if (100..200).contains(&status) && status != 101 {
                        // Interim response (100 Continue etc.): discard and
                        // wait for the real status line.
                        continue;
                    }
                    self.stage = match (self.framing_for(status), status) {
                        // 1xx (101), 204 and 304 have no body by definition.
                        (_, 101 | 204 | 304) => Stage::Done,
                        (BodyFraming::Fixed(0), _) => Stage::Done,
                        (BodyFraming::Fixed(remaining), _) => Stage::FixedBody { remaining },
                        (BodyFraming::Chunked, _) => Stage::ChunkSize,
                        (BodyFraming::UntilClose, _) => Stage::FixedBody {
                            // Reuse the take/drain loop with an unbounded
                            // remainder; `finish` treats UntilClose specially.
                            remaining: usize::MAX,
                        },
                    };
                    self.framing = self.framing_for(status);
                }
                Stage::FixedBody { remaining } => {
                    let take = remaining.min(self.buf.len());
                    if self.body.len() + take > self.body_limit {
                        return Err(NarouError::Http(
                            "socket response body exceeds limit".to_string(),
                        ));
                    }
                    self.body.extend(self.buf.drain(..take));
                    let remaining = remaining - take;
                    if remaining == 0 {
                        self.stage = Stage::Done;
                    } else {
                        self.stage = Stage::FixedBody { remaining };
                        return Ok(false);
                    }
                }
                Stage::ChunkSize => {
                    let Some(line_end) = find_crlf(&self.buf) else {
                        return Ok(false);
                    };
                    let line: Vec<u8> = self.buf.drain(..line_end + 2).collect();
                    let size = parse_chunk_size(&line[..line.len() - 2])?;
                    self.stage = if size == 0 {
                        Stage::Trailers
                    } else {
                        Stage::ChunkData { remaining: size }
                    };
                }
                Stage::ChunkData { remaining } => {
                    let take = remaining.min(self.buf.len());
                    if self.body.len() + take > self.body_limit {
                        return Err(NarouError::Http(
                            "socket response body exceeds limit".to_string(),
                        ));
                    }
                    self.body.extend(self.buf.drain(..take));
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
                    if self.buf.len() < 2 {
                        return Ok(false);
                    }
                    if &self.buf[..2] != b"\r\n" {
                        return Err(NarouError::Http(
                            "chunk data not followed by CRLF".to_string(),
                        ));
                    }
                    self.buf.drain(..2);
                    self.stage = Stage::ChunkSize;
                }
                Stage::Trailers => {
                    // Trailer section: field lines then an empty line; an
                    // empty section is just the bare CRLF after the zero chunk.
                    let mut consumed = 0;
                    let mut ended = false;
                    while let Some(at) = find_crlf(&self.buf[consumed..]) {
                        if at == 0 {
                            consumed += 2;
                            ended = true;
                            break;
                        }
                        consumed += at + 2;
                    }
                    self.buf.drain(..consumed);
                    if !ended {
                        return Ok(false);
                    }
                    self.stage = Stage::Done;
                }
                Stage::Done => {
                    self.buf.clear();
                    return Ok(true);
                }
            }
        }
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

    fn framing_for(&self, _status: u16) -> BodyFraming {
        let chunked = self
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
            .map(|(_, value)| value.to_ascii_lowercase())
            .is_some_and(|value| value.split(',').any(|part| part.trim() == "chunked"));
        if chunked {
            return BodyFraming::Chunked;
        }
        if let Some(length) = self
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        {
            return BodyFraming::Fixed(length);
        }
        BodyFraming::UntilClose
    }
}

struct HeadEnd {
    at: usize,
    skip: usize,
}

fn find_head_end(buf: &[u8]) -> Option<HeadEnd> {
    if let Some(at) = find_subslice(buf, b"\r\n\r\n") {
        return Some(HeadEnd { at, skip: 4 });
    }
    // Tolerate LF-only servers; parsed line-wise so lone \r is still trimmed.
    find_subslice(buf, b"\n\n").map(|at| HeadEnd { at, skip: 2 })
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    find_subslice(buf, b"\r\n")
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
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
            &[
                ("User-Agent".to_string(), "narou/1".to_string()),
                ("Cookie".to_string(), "a=b".to_string()),
            ],
        );
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("GET /path/to?q=1&r=2 HTTP/1.1\r\n"));
        assert!(text.contains("Host: example.jp:8443\r\n"));
        assert!(text.contains("User-Agent: narou/1\r\n"));
        assert!(text.contains("Cookie: a=b\r\n"));
        assert!(text.ends_with("Connection: close\r\n\r\n"));
    }

    #[test]
    fn request_drops_framing_and_injected_headers() {
        let bytes = encode_request(
            &target("https://example.jp/"),
            &[
                ("Host".to_string(), "evil.jp".to_string()),
                ("Connection".to_string(), "keep-alive".to_string()),
                ("Accept-Encoding".to_string(), "gzip".to_string()),
                ("Content-Length".to_string(), "5".to_string()),
                ("X-Bad".to_string(), "a\r\nInjected: 1".to_string()),
            ],
        );
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text.matches("Host:").count(), 1);
        assert!(text.contains("Host: example.jp\r\n"));
        assert!(!text.contains("evil.jp"));
        assert!(!text.contains("gzip"));
        assert!(!text.contains("Injected"));
        assert!(!text.contains("keep-alive"));
        assert!(text.ends_with("Connection: close\r\n\r\n"));
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
    fn target_join_resolves_relative_location() {
        let base = target("https://example.jp/a/b?x=1");
        let next = base.join("../c?y=2").unwrap();
        assert_eq!(next.target, "/c?y=2");
        assert_eq!(next.host, "example.jp");
        let absolute = base.join("https://other.jp/z").unwrap();
        assert_eq!(absolute.host, "other.jp");
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

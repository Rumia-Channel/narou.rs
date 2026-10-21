//! curl トランスポート。
//!
//! 旧 Node 版 `scripts/relay/transport.mjs` の `viaCurl` の移植。`-i` 付きの
//! 出力から応答ヘッドと本文を分け、1xx の中間応答だけを読み飛ばす。
//!
//! 接続先は [`Target`] の検証済みアドレスに固定する。URL のホストが名前なら
//! `--resolve host:port:address` を渡し (IP リテラルの URL は URL 自体が接続先
//! になるので追加しない)、転送ヘッダは argv に載せず `--header @-` で標準入力
//! から渡す。子プロセスの stderr は捨て、URL・ヘッダ・Cookie・資格情報を
//! ログやエラーへ含めない。

use std::ffi::OsString;
use std::net::IpAddr;
use std::process::Stdio;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, ChildStdout, Command};
use url::Url;

use crate::wire::{MAX_BODY_BYTES, MAX_HEAD_BYTES, MAX_TRANSPORT_OUTPUT};

use super::{
    CurlOptions, KillOnDrop, Target, TransportError, TransportErrorKind, TransportResponse,
};

/// 子プロセスの stdout を読む 1 回のバッファ。上限判定は累積バイト数で行う。
const READ_BUFFER_BYTES: usize = 64 * 1024;

/// curl 経由で取得する。
///
/// 引数・出力解釈は旧 Node 版 `viaCurl` と同じ: `-q -sS -i --compressed
/// --noproxy * --proto =http,https`、ホスト名なら `--resolve host:port:address`
/// で接続先を固定し、ヘッダは標準入力から `--header @-` で渡す。1xx は
/// 読み飛ばし、本文は上限つきで読む。
pub async fn via_curl(
    target: &Target,
    headers: &[(String, String)],
    options: &CurlOptions,
) -> Result<TransportResponse, TransportError> {
    let args = build_curl_args(target, options);
    let mut command = Command::new(&options.bin);
    command
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| TransportError::new(spawn_error_kind(error.kind())))?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let mut guard = KillOnDrop::new(child);
    let header_bytes = header_block(headers);

    // `KillOnDrop` が子プロセスを所有したまま、ヘッダの書き込み・stdout の
    // 読み出し・終了待ちをまとめて期限で囲む。期限切れでこの future が
    // drop されると子プロセスも kill される。
    let outcome = tokio::time::timeout(options.timeout, async {
        let ((), output) =
            tokio::join!(write_header_block(stdin, header_bytes), read_stdout(stdout));
        let output = output?;
        let status = guard
            .child_mut()
            .wait()
            .await
            .map_err(|_| TransportError::new(TransportErrorKind::Failed))?;
        if !status.success() {
            return Err(TransportError::new(curl_exit_kind(status.code())));
        }
        split_head(&output)
    })
    .await;

    match outcome {
        Ok(result) => result,
        Err(_) => {
            let _ = guard.child_mut().start_kill();
            Err(TransportError::new(TransportErrorKind::TimedOut))
        }
    }
}

/// curl に渡す引数を組み立てる (実行ファイル名は含まない)。
fn build_curl_args(target: &Target, options: &CurlOptions) -> Vec<OsString> {
    // Node: `String(Math.min(10, timeoutMs / 1000))` / `String(timeoutMs / 1000)`。
    let timeout_seconds = options.timeout.as_secs_f64();
    let mut args: Vec<OsString> = [
        "-q",
        "-sS",
        "-i",
        "--compressed",
        "--noproxy",
        "*",
        "--proto",
        "=http,https",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    args.push(OsString::from("--connect-timeout"));
    args.push(OsString::from(timeout_seconds.min(10.0).to_string()));
    args.push(OsString::from("--max-time"));
    args.push(OsString::from(timeout_seconds.to_string()));
    args.push(OsString::from("--max-filesize"));
    args.push(OsString::from(MAX_BODY_BYTES.to_string()));
    if let Some(cacert) = &options.cacert {
        args.push(OsString::from("--cacert"));
        args.push(cacert.as_os_str().to_owned());
    }
    let host = url_host(&target.url);
    if host.parse::<IpAddr>().is_err() {
        let port = target_port(target);
        let address = if target.address.is_ipv6() {
            format!("[{}]", target.address)
        } else {
            target.address.to_string()
        };
        args.push(OsString::from("--resolve"));
        args.push(OsString::from(format!("{host}:{port}:{address}")));
    }
    args.push(OsString::from("--header"));
    args.push(OsString::from("@-"));
    args.push(OsString::from("--"));
    args.push(OsString::from(target.url.as_str()));
    args
}

/// `headers` を `--header @-` が読む形 (`Name: value\r\n` の連続) にする。
///
/// 大文字小文字と順序はそのまま、値は UTF-8 バイト列として渡す。Node 版と
/// 同様、最後の 1 行も CRLF で終える。
fn header_block(headers: &[(String, String)]) -> Vec<u8> {
    let mut block = Vec::new();
    for (name, value) in headers {
        block.extend_from_slice(name.as_bytes());
        block.extend_from_slice(b": ");
        block.extend_from_slice(value.as_bytes());
        block.extend_from_slice(b"\r\n");
    }
    block
}

/// URL のホスト。IPv6 の `[]` を外す (Node の `.replace(/^\[|\]$/g, "")`)。
fn url_host(url: &Url) -> &str {
    let host = url.host_str().unwrap_or_default();
    let host = host.strip_prefix('[').unwrap_or(host);
    host.strip_suffix(']').unwrap_or(host)
}

/// 接続ポート。Node の `url.port || (https ? 443 : 80)` と同じ。
fn target_port(target: &Target) -> u16 {
    target
        .url
        .port_or_known_default()
        .unwrap_or(if target.url.scheme() == "https" {
            443
        } else {
            80
        })
}

/// 子プロセスを起動できなかったときの理由。Node の spawn error (`ENOENT` /
/// `EACCES`) と同じ写像。
fn spawn_error_kind(kind: std::io::ErrorKind) -> TransportErrorKind {
    match kind {
        std::io::ErrorKind::NotFound => TransportErrorKind::NotInstalled,
        std::io::ErrorKind::PermissionDenied => TransportErrorKind::NotExecutable,
        _ => TransportErrorKind::Failed,
    }
}

/// curl の終了コードを旧 Node 版 `curlFailure` と同じ理由に写す。
fn curl_exit_kind(code: Option<i32>) -> TransportErrorKind {
    match code {
        Some(51 | 58 | 60 | 77) => TransportErrorKind::TlsVerification,
        Some(28) => TransportErrorKind::TimedOut,
        Some(63) => TransportErrorKind::TooLarge,
        Some(18) => TransportErrorKind::Incomplete,
        _ => TransportErrorKind::Failed,
    }
}

/// ヘッダを標準入力へ書き、EOF を送る。Node 版と同じく書き込みエラーは無視する
/// (子プロセス側ですでに失敗している場合がある)。
async fn write_header_block(stdin: Option<ChildStdin>, block: Vec<u8>) {
    if let Some(mut stdin) = stdin {
        let _ = stdin.write_all(&block).await;
        let _ = stdin.shutdown().await;
    }
}

/// 子プロセスの stdout を上限つきで読む。
async fn read_stdout(stdout: Option<ChildStdout>) -> Result<Vec<u8>, TransportError> {
    let Some(mut stdout) = stdout else {
        return Ok(Vec::new());
    };
    let mut output = Vec::new();
    let mut buffer = vec![0u8; READ_BUFFER_BYTES];
    loop {
        let read = stdout
            .read(&mut buffer)
            .await
            .map_err(|_| TransportError::new(TransportErrorKind::Failed))?;
        if read == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(read) > MAX_TRANSPORT_OUTPUT {
            return Err(TransportError::new(TransportErrorKind::TooLarge));
        }
        output.extend_from_slice(&buffer[..read]);
    }
}

/// `-i` の出力を「応答ヘッド + 本文」に分解する。
///
/// 1xx のヘッドは読み飛ばし、最初の最終応答 (2xx..599) を採用する。本文は
/// そのまま残すため、本文が `HTTP/` で始まっても解釈し直さない。
fn split_head(buffer: &[u8]) -> Result<TransportResponse, TransportError> {
    let mut offset = 0;
    loop {
        let Some(relative_end) = find_terminator(&buffer[offset..]) else {
            return Err(incomplete());
        };
        let end = offset + relative_end;
        if end + 4 > MAX_HEAD_BYTES {
            return Err(incomplete());
        }
        let (status, headers) = parse_head_block(&String::from_utf8_lossy(&buffer[offset..end]))?;
        offset = end + 4;
        if (100..200).contains(&status) {
            continue;
        }
        if !(200..=599).contains(&status) {
            return Err(incomplete());
        }
        let body = &buffer[offset..];
        if body.len() > MAX_BODY_BYTES {
            return Err(too_large());
        }
        return Ok(build_response(status, &headers, body, "curl"));
    }
}

/// `\r\n\r\n` の位置を探す。
fn find_terminator(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

/// 1 個の応答ヘッド (ステータス行 + ヘッダ行) を解釈する。
fn parse_head_block(text: &str) -> Result<(u16, Vec<(String, String)>), TransportError> {
    let mut lines = text.split("\r\n");
    let status = parse_status_line(lines.next().unwrap_or_default()).ok_or_else(incomplete)?;
    let mut headers = Vec::new();
    for line in lines {
        if let Some(separator) = line.find(':')
            && separator > 0
        {
            headers.push((
                line[..separator].trim().to_string(),
                line[separator + 1..].trim().to_string(),
            ));
        }
    }
    Ok((status, headers))
}

/// ステータス行から 3 桁のステータスを取り出す (Node の `^HTTP\/[\d.]+ (\d{3})`)。
fn parse_status_line(line: &str) -> Option<u16> {
    let rest = line.strip_prefix("HTTP/")?;
    let version_len = rest
        .find(|character: char| !(character.is_ascii_digit() || character == '.'))
        .unwrap_or(rest.len());
    if version_len == 0 {
        return None;
    }
    let digits = rest[version_len..].strip_prefix(' ')?;
    let bytes = digits.as_bytes();
    if bytes.len() < 3 || !bytes[..3].iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(
        u16::from(bytes[0] - b'0') * 100
            + u16::from(bytes[1] - b'0') * 10
            + u16::from(bytes[2] - b'0'),
    )
}

/// 最初に現れた同名ヘッダの値 (大文字小文字を無視)。
fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// 応答ヘッドと本文から [`TransportResponse`] を組み立てる。
fn build_response(
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
    via: &'static str,
) -> TransportResponse {
    let content_type = header_value(headers, "content-type").unwrap_or_default();
    let location = header_value(headers, "location").filter(|value| !value.is_empty());
    let set_cookie = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
        .map(|(_, value)| value.clone())
        .collect();
    TransportResponse {
        status,
        content_type: content_type.to_string(),
        location: location.map(str::to_string),
        set_cookie,
        body: body.to_vec(),
        via,
    }
}

fn incomplete() -> TransportError {
    TransportError::new(TransportErrorKind::Incomplete)
}

fn too_large() -> TransportError {
    TransportError::new(TransportErrorKind::TooLarge)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    fn target(url: &str, address: &str) -> Target {
        Target {
            url: Url::parse(url).expect("target URL"),
            address: address.parse().expect("target address"),
        }
    }

    fn options(timeout: Duration) -> CurlOptions {
        CurlOptions {
            bin: OsString::from("curl"),
            cacert: None,
            timeout,
        }
    }

    fn arg_strings(target: &Target, options: &CurlOptions) -> Vec<String> {
        build_curl_args(target, options)
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn value_after<'a>(args: &'a [String], flag: &str) -> &'a str {
        let position = args
            .iter()
            .position(|arg| arg == flag)
            .unwrap_or_else(|| panic!("{flag} missing"));
        args.get(position + 1)
            .unwrap_or_else(|| panic!("{flag} has no value"))
    }

    #[test]
    fn hostnames_are_pinned_with_resolve_but_ip_literals_are_not() {
        let named = target("https://relay.invalid:8443/chapter", "93.184.216.34");
        let args = arg_strings(&named, &options(Duration::from_secs(30)));
        assert_eq!(
            value_after(&args, "--resolve"),
            "relay.invalid:8443:93.184.216.34"
        );
        // ポート省略時はスキームの既定ポートを使う。
        assert_eq!(
            value_after(
                &arg_strings(
                    &target("https://relay.invalid/", "93.184.216.34"),
                    &options(Duration::from_secs(30))
                ),
                "--resolve"
            ),
            "relay.invalid:443:93.184.216.34"
        );
        assert_eq!(
            value_after(
                &arg_strings(
                    &target("http://relay.invalid/path", "93.184.216.34"),
                    &options(Duration::from_secs(30))
                ),
                "--resolve"
            ),
            "relay.invalid:80:93.184.216.34"
        );

        for url in [
            "http://127.0.0.1/path",
            "https://[2606:4700:4700::1111]/path",
        ] {
            let args = arg_strings(&target(url, "127.0.0.1"), &options(Duration::from_secs(30)));
            assert!(
                !args.iter().any(|arg| arg == "--resolve"),
                "{url} must not add --resolve"
            );
        }

        // ホスト名でも解決先が IPv6 なら Node 版と同じ `[address]` 形式にする。
        let ipv6 = target("https://relay.invalid:8443/", "2606:4700:4700::1111");
        let args = arg_strings(&ipv6, &options(Duration::from_secs(30)));
        assert_eq!(
            value_after(&args, "--resolve"),
            "relay.invalid:8443:[2606:4700:4700::1111]"
        );
    }

    #[test]
    fn cacert_is_only_added_when_configured() {
        let target = target("https://relay.invalid/", "93.184.216.34");
        let without = arg_strings(&target, &options(Duration::from_secs(30)));
        assert!(!without.iter().any(|arg| arg == "--cacert"));

        let mut with_cacert = options(Duration::from_secs(30));
        with_cacert.cacert = Some(PathBuf::from("ca.pem"));
        let args = arg_strings(&target, &with_cacert);
        assert_eq!(value_after(&args, "--cacert"), "ca.pem");
    }

    #[test]
    fn timeouts_are_derived_from_the_transport_timeout() {
        let target = target("https://relay.invalid/", "93.184.216.34");
        let args = arg_strings(&target, &options(Duration::from_secs(30)));
        assert_eq!(value_after(&args, "--connect-timeout"), "10");
        assert_eq!(value_after(&args, "--max-time"), "30");

        // Node 版はミリ秒を秒の JS 数値文字列にする (300ms -> "0.3")。
        let args = arg_strings(&target, &options(Duration::from_millis(300)));
        assert_eq!(value_after(&args, "--connect-timeout"), "0.3");
        assert_eq!(value_after(&args, "--max-time"), "0.3");

        let args = arg_strings(&target, &options(Duration::from_millis(2500)));
        assert_eq!(value_after(&args, "--connect-timeout"), "2.5");
        assert_eq!(value_after(&args, "--max-time"), "2.5");
    }

    #[test]
    fn base_arguments_match_the_node_construction() {
        let target = target("https://relay.invalid:8443/chapter?x=1", "93.184.216.34");
        let args = arg_strings(&target, &options(Duration::from_secs(30)));
        let base: Vec<&str> = args[..8].iter().map(String::as_str).collect();
        assert_eq!(
            base,
            [
                "-q",
                "-sS",
                "-i",
                "--compressed",
                "--noproxy",
                "*",
                "--proto",
                "=http,https"
            ]
        );
        assert_eq!(value_after(&args, "--max-filesize"), "16777216");
        let tail: Vec<&str> = args[args.len() - 4..].iter().map(String::as_str).collect();
        assert_eq!(
            tail,
            [
                "--header",
                "@-",
                "--",
                "https://relay.invalid:8443/chapter?x=1"
            ]
        );
    }

    #[test]
    fn headers_keep_casing_and_order_and_end_with_crlf() {
        let headers = vec![
            ("X-First".to_string(), "one".to_string()),
            ("x-first".to_string(), "two".to_string()),
            ("Cookie".to_string(), "sid=1".to_string()),
            ("X-Label".to_string(), "café".to_string()),
        ];
        let block = header_block(&headers);
        assert_eq!(
            block,
            b"X-First: one\r\nx-first: two\r\nCookie: sid=1\r\nX-Label: caf\xc3\xa9\r\n".to_vec()
        );
        assert!(header_block(&[]).is_empty());
    }

    #[test]
    fn split_head_parses_status_headers_and_body() {
        let output = b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nLocation: https://example.com/next\r\nSet-Cookie: a=1; Path=/\r\nSet-Cookie: b=2\r\n\r\nhello";
        let response = split_head(output).expect("response");
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "text/html; charset=utf-8");
        assert_eq!(
            response.location.as_deref(),
            Some("https://example.com/next")
        );
        assert_eq!(response.set_cookie, vec!["a=1; Path=/", "b=2"]);
        assert_eq!(response.body, b"hello");
        assert_eq!(response.via, "curl");

        // 空の Location は Node 版と同じく省略する。
        let response =
            split_head(b"HTTP/1.1 204 No Content\r\nLocation: \r\n\r\n").expect("response");
        assert_eq!(response.status, 204);
        assert_eq!(response.content_type, "");
        assert_eq!(response.location, None);
        assert!(response.set_cookie.is_empty());
        assert!(response.body.is_empty());

        // 先頭の `:` や `:` を含まない行は無視する。
        let response = split_head(b"HTTP/1.1 200 OK\r\n: bad\r\nnonsense\r\nX-Good: yes\r\n\r\n")
            .expect("response");
        assert_eq!(response.content_type, "");
        let _ = response;
    }

    #[test]
    fn interim_responses_are_skipped_but_http_bodies_are_not() {
        let output = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 103 Early Hints\r\nLink: </a>\r\n\r\nHTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nHTTP/1.1 404 Not Found\r\n\r\nrest";
        let response = split_head(output).expect("response");
        assert_eq!(response.status, 200);
        assert_eq!(
            response.body,
            b"HTTP/1.1 404 Not Found\r\n\r\nrest".to_vec()
        );
    }

    #[test]
    fn incomplete_output_is_rejected() {
        for output in [
            &b""[..],
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain",
            b"HTTP/1.1 199 Early\r\n\r\n",
            b"HTTP/1.1 600 Nope\r\n\r\n",
            b"garbage\r\n\r\n",
        ] {
            assert_eq!(
                split_head(output).unwrap_err().kind,
                TransportErrorKind::Incomplete,
                "{output:?}"
            );
        }
    }

    #[test]
    fn oversized_body_is_too_large() {
        let mut output = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        output.resize(output.len() + MAX_BODY_BYTES + 1, b'x');
        assert_eq!(
            split_head(&output).unwrap_err().kind,
            TransportErrorKind::TooLarge
        );
    }

    #[test]
    fn oversized_head_is_incomplete() {
        let mut output = b"HTTP/1.1 200 OK\r\n".to_vec();
        output.resize(MAX_HEAD_BYTES + 1, b'a');
        output.extend_from_slice(b"\r\n\r\n");
        assert_eq!(
            split_head(&output).unwrap_err().kind,
            TransportErrorKind::Incomplete
        );
    }

    #[test]
    fn curl_exit_codes_map_to_the_node_reasons() {
        for code in [51, 58, 60, 77] {
            assert_eq!(
                curl_exit_kind(Some(code)),
                TransportErrorKind::TlsVerification,
                "{code}"
            );
        }
        assert_eq!(curl_exit_kind(Some(28)), TransportErrorKind::TimedOut);
        assert_eq!(curl_exit_kind(Some(63)), TransportErrorKind::TooLarge);
        assert_eq!(curl_exit_kind(Some(18)), TransportErrorKind::Incomplete);
        assert_eq!(curl_exit_kind(None), TransportErrorKind::Failed);
        for code in [1, 6, 7, 22, 35, 52, 56, 67, 99] {
            assert_eq!(
                curl_exit_kind(Some(code)),
                TransportErrorKind::Failed,
                "{code}"
            );
        }
    }

    #[test]
    fn spawn_errors_map_to_not_installed_or_not_executable() {
        assert_eq!(
            spawn_error_kind(std::io::ErrorKind::NotFound),
            TransportErrorKind::NotInstalled
        );
        assert_eq!(
            spawn_error_kind(std::io::ErrorKind::PermissionDenied),
            TransportErrorKind::NotExecutable
        );
        assert_eq!(
            spawn_error_kind(std::io::ErrorKind::Other),
            TransportErrorKind::Failed
        );
    }

    fn curl_available() -> bool {
        std::process::Command::new("curl")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[tokio::test]
    async fn missing_curl_binary_is_not_installed() {
        let options = CurlOptions {
            bin: OsString::from("narou-relay-missing-curl-binary"),
            cacert: None,
            timeout: Duration::from_secs(1),
        };
        let error = via_curl(
            &target("https://relay.invalid/", "93.184.216.34"),
            &[],
            &options,
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind, TransportErrorKind::NotInstalled);
    }

    #[tokio::test]
    async fn unresponsive_upstream_times_out_and_kills_curl() {
        if !curl_available() {
            eprintln!("curl is not available; skipping");
            return;
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("upstream listener");
        let port = listener.local_addr().expect("upstream address").port();
        let holding = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("upstream accept");
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer).await;
            // 応答を返さず接続を保持し、クライアント側の期限で切られることを確認する。
            tokio::time::sleep(Duration::from_secs(10)).await;
        });
        let options = CurlOptions {
            bin: OsString::from("curl"),
            cacert: None,
            timeout: Duration::from_millis(300),
        };
        let target = target(&format!("http://relay.invalid:{port}/"), "127.0.0.1");
        let error = via_curl(&target, &[], &options).await.unwrap_err();
        assert_eq!(error.kind, TransportErrorKind::TimedOut);
        holding.abort();
    }

    #[tokio::test]
    async fn real_curl_fetches_from_a_local_upstream() {
        if !curl_available() {
            eprintln!("curl is not available; skipping");
            return;
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("upstream listener");
        let port = listener.local_addr().expect("upstream address").port();
        let upstream = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("upstream accept");
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).await.expect("request read");
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 11\r\nConnection: close\r\n\r\nhello world",
                )
                .await
                .expect("response write");
            stream.flush().await.expect("response flush");
        });

        let target = target(&format!("http://relay.invalid:{port}/path"), "127.0.0.1");
        let options = CurlOptions {
            bin: OsString::from("curl"),
            cacert: None,
            timeout: Duration::from_secs(10),
        };
        let response = via_curl(
            &target,
            &[
                ("X-Custom".to_string(), "café".to_string()),
                ("Cookie".to_string(), "sid=1".to_string()),
            ],
            &options,
        )
        .await
        .expect("curl response");
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "text/plain");
        assert_eq!(response.location, None);
        assert!(response.set_cookie.is_empty());
        assert_eq!(response.body, b"hello world");
        assert_eq!(response.via, "curl");
        let _ = tokio::time::timeout(Duration::from_secs(5), upstream).await;
    }
}

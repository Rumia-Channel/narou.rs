//! wget トランスポート (HTTPS のみ)。
//!
//! 旧 Node 版 `scripts/relay/transport.mjs` の `viaWget` の移植。wget には
//! curl の `--resolve` に相当する接続先の固定が無いため、127.0.0.1 に使い捨ての
//! CONNECT プロキシを立て、[`Target::address`] の検証済みアドレスへだけ
//! トンネルする。TLS の SNI と証明書検証は wget が元のホスト名に対して行う。
//!
//! HTTP はプロキシ経由の絶対形式 URL になり接続先固定の前提が崩れるため、
//! Node 版と同じく [`TransportErrorKind::Failed`] を返して次のトランスポート
//! (curl) に任せる。wgetrc は `--no-config` で無効化し、環境プロキシは
//! ローカルプロキシへ向け直す。子プロセスの stderr は捨て、URL・ヘッダ・
//! Cookie・資格情報をログやエラーへ含めない。

use std::ffi::OsString;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::wire::{MAX_BODY_BYTES, MAX_HEAD_BYTES, MAX_TRANSPORT_OUTPUT};

use super::{
    KillOnDrop, Target, TransportError, TransportErrorKind, TransportResponse, WgetOptions,
};

/// 子プロセスの stdout を読む 1 回のバッファ。上限判定は累積バイト数で行う。
const READ_BUFFER_BYTES: usize = 64 * 1024;
/// プロキシの CONNECT 要求を読む 1 回のバッファ。
const CONNECT_BUFFER_BYTES: usize = 4 * 1024;
/// トンネル確立時にクライアントへ返す応答。
const CONNECT_ESTABLISHED: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";
/// wget へ転送しないヘッダ (旧 Node 版 `WGET_UNFORWARDED`)。
const UNFORWARDED_HEADERS: [&str; 6] = [
    "host",
    "user-agent",
    "accept-encoding",
    "connection",
    "proxy-connection",
    "content-length",
];

/// wget 経由で取得する (HTTPS のみ)。
///
/// 旧 Node 版 `viaWget` と同じく、検証済みアドレスへだけトンネルする使い捨て
/// CONNECT プロキシを 127.0.0.1 に立て、TLS の SNI・証明書検証は wget に任せる。
pub async fn via_wget(
    target: &Target,
    headers: &[(String, String)],
    options: &WgetOptions,
) -> Result<TransportResponse, TransportError> {
    if target.url.scheme() != "https" {
        return Err(TransportError::new(TransportErrorKind::Failed));
    }
    let proxy = PinnedProxy::start(target).await?;
    let proxy_url = format!("http://127.0.0.1:{}", proxy.port);
    let established = proxy.established_handle();
    let args = build_wget_args(target, headers, options);
    let mut command = tokio::process::Command::new(&options.bin);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        // 環境のプロキシ設定を潰し、取得先は必ず使い捨てプロキシだけにする。
        .env("http_proxy", &proxy_url)
        .env("https_proxy", &proxy_url)
        .env("HTTP_PROXY", &proxy_url)
        .env("HTTPS_PROXY", &proxy_url)
        .env("no_proxy", "")
        .env("NO_PROXY", "");
    // Node 版は spawn の失敗理由を区別せず not-installed にする。
    let mut child = command
        .spawn()
        .map_err(|_| TransportError::new(TransportErrorKind::NotInstalled))?;
    let stdout = child.stdout.take();
    let mut guard = KillOnDrop::new(child);

    let outcome = tokio::time::timeout(options.timeout, async {
        let output = read_stdout(stdout).await?;
        let _status = guard
            .child_mut()
            .wait()
            .await
            .map_err(|_| TransportError::new(TransportErrorKind::Failed))?;
        // ヘッドが読めない失敗は Node の `parseWgetOutput` 例外と同じ扱い
        // (RelayTransportError はそのまま传播し、非 RelayTransportError のみ
        // 終了コードで failed/incomplete に振り分ける)。Rust 側の解析失敗は
        // 常に TransportError なので、その理由をそのまま返す。
        let response = parse_wget_output(&output)?;
        // wget が環境プロキシを無視して直接接続した場合、検証済み IP への
        // 接続固定が効いていない。応答を成功として返さない。
        if !established.load(Ordering::SeqCst) {
            return Err(TransportError::new(TransportErrorKind::Failed));
        }
        Ok(response)
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

/// wget に渡す引数を組み立てる (実行ファイル名は含まない)。
fn build_wget_args(
    target: &Target,
    headers: &[(String, String)],
    options: &WgetOptions,
) -> Vec<OsString> {
    // Node: `Math.max(1, Math.ceil(timeoutMs / 1000))` 秒。
    let seconds = options.timeout.as_millis().div_ceil(1000).max(1);
    let mut args: Vec<OsString> = [
        "--no-config",
        "--no-verbose",
        "--save-headers",
        "--content-on-error",
        "--output-document=-",
        "--tries=1",
        "--max-redirect=0",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    args.push(OsString::from(format!(
        "--connect-timeout={}",
        seconds.min(10)
    )));
    args.push(OsString::from(format!("--read-timeout={seconds}")));
    args.push(OsString::from(format!("--timeout={seconds}")));
    if let Some(cacert) = &options.cacert {
        let mut arg = OsString::from("--ca-certificate=");
        arg.push(cacert.as_os_str());
        args.push(arg);
    }
    let user_agent = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
        .map(|(_, value)| value.as_str())
        .unwrap_or_default();
    if !user_agent.is_empty() {
        args.push(OsString::from(format!("--user-agent={user_agent}")));
    }
    for (name, value) in headers {
        if UNFORWARDED_HEADERS
            .iter()
            .any(|candidate| name.eq_ignore_ascii_case(candidate))
        {
            continue;
        }
        args.push(OsString::from(format!("--header={name}: {value}")));
    }
    args.push(OsString::from("--"));
    args.push(OsString::from(target.url.as_str()));
    args
}

/// 接続先を固定する使い捨て CONNECT プロキシ。
///
/// 受理ループと接続ごとのタスクを 1 個の tokio タスクにまとめ、drop で全体を
/// abort する。残った接続は `JoinSet` の drop で閉じられる。
struct PinnedProxy {
    port: u16,
    /// 検証済みアドレスへの CONNECT トンネルが 1 本以上確立したか。
    established: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl PinnedProxy {
    async fn start(target: &Target) -> Result<Self, TransportError> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|_| TransportError::new(TransportErrorKind::Failed))?;
        let port = listener
            .local_addr()
            .map_err(|_| TransportError::new(TransportErrorKind::Failed))?
            .port();
        let expected_host = connect_host(target);
        let expected_port = target_port(target);
        let address = target.address;
        let established = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&established);
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _peer)) = accepted else {
                            // accept の恒久エラーで空回りしないよう受理を止める。
                            return;
                        };
                        connections.spawn(handle_connection(
                            stream,
                            expected_host.clone(),
                            expected_port,
                            address,
                            Arc::clone(&flag),
                        ));
                    }
                    joined = connections.join_next(), if !connections.is_empty() => {
                        let _ = joined;
                    }
                }
            }
        });
        Ok(Self {
            port,
            established,
            task,
        })
    }

    /// トンネル確立フラグの参照。取得開始前に clone して使う。
    fn established_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.established)
    }
}

impl Drop for PinnedProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// CONNECT 要求を検証し、検証済みアドレスへのトンネルを確立する。
async fn handle_connection(
    mut client: TcpStream,
    expected_host: String,
    expected_port: u16,
    address: IpAddr,
    established: Arc<AtomicBool>,
) {
    let mut head = Vec::new();
    let mut buffer = vec![0u8; CONNECT_BUFFER_BYTES];
    let Some(end) = read_connect_head(&mut client, &mut head, &mut buffer).await else {
        return;
    };
    let request_line_end = head[..end]
        .windows(2)
        .position(|window| window == b"\r\n")
        .unwrap_or(end);
    let request_line = String::from_utf8_lossy(&head[..request_line_end]);
    if !authorize_connect(&request_line, &expected_host, expected_port) {
        return;
    }
    let Ok(mut upstream) = TcpStream::connect(SocketAddr::new(address, expected_port)).await else {
        return;
    };
    if client.write_all(CONNECT_ESTABLISHED).await.is_err() {
        return;
    }
    established.store(true, Ordering::SeqCst);
    let leftover = &head[end + 4..];
    if !leftover.is_empty() && upstream.write_all(leftover).await.is_err() {
        return;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
}

/// `\r\n\r\n` まで読む。上限超過・EOF・読み取りエラーでは `None`。
async fn read_connect_head(
    stream: &mut TcpStream,
    head: &mut Vec<u8>,
    buffer: &mut [u8],
) -> Option<usize> {
    loop {
        match stream.read(buffer).await {
            Ok(0) | Err(_) => return None,
            Ok(read) => head.extend_from_slice(&buffer[..read]),
        }
        if head.len() > MAX_HEAD_BYTES {
            return None;
        }
        if let Some(end) = find_terminator(head) {
            return Some(end);
        }
    }
}

/// 解釈済みの CONNECT 要求。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ConnectRequest {
    host: String,
    port: u16,
}

/// `CONNECT <host>:<port> HTTP/1.[01]` を解釈する。
///
/// 旧 Node 版の正規表現 `/^CONNECT\s+([^\s]+):(\d+)\s+HTTP\/1\.[01]$/i` と同じ
/// 規則: メソッドと HTTP 版は大文字小文字を無視し、ホストには空白を含めない。
fn parse_connect_line(line: &str) -> Option<ConnectRequest> {
    let prefix = line.get(..7)?;
    if !prefix.eq_ignore_ascii_case("CONNECT") {
        return None;
    }
    let after_connect = &line[7..];
    if !after_connect.starts_with(char::is_whitespace) {
        return None;
    }
    let body = after_connect.trim_start();
    let suffix_start = body.len().checked_sub("HTTP/1.1".len())?;
    let suffix = body.get(suffix_start..)?;
    if !suffix.eq_ignore_ascii_case("HTTP/1.0") && !suffix.eq_ignore_ascii_case("HTTP/1.1") {
        return None;
    }
    let before_http = body.get(..suffix_start)?;
    let host_port = before_http.trim_end();
    if host_port.len() == before_http.len() {
        return None;
    }
    let (host, port_text) = host_port.rsplit_once(':')?;
    if host.is_empty()
        || port_text.is_empty()
        || !port_text.bytes().all(|byte| byte.is_ascii_digit())
        || host.chars().any(char::is_whitespace)
    {
        return None;
    }
    Some(ConnectRequest {
        host: host.to_string(),
        port: port_text.parse().ok()?,
    })
}

/// CONNECT 要求が固定済みの取得先だけを指しているか。
fn authorize_connect(line: &str, expected_host: &str, expected_port: u16) -> bool {
    let Some(request) = parse_connect_line(line) else {
        return false;
    };
    request.port == expected_port && normalized_proxy_host(&request.host) == expected_host
}

/// Node の `normalizedHost`: IPv6 の `[]` を外し、末尾の `.` を 1 個外して
/// 小文字化する。
fn normalized_proxy_host(value: &str) -> String {
    let value = value.strip_prefix('[').unwrap_or(value);
    let value = value.strip_suffix(']').unwrap_or(value);
    let value = value.strip_suffix('.').unwrap_or(value);
    value.to_lowercase()
}

/// プロキシが期待する接続先ホスト名 (`Target::url` の正規化済みホスト)。
fn connect_host(target: &Target) -> String {
    normalized_proxy_host(target.url.host_str().unwrap_or_default())
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

/// `--save-headers` の出力 (応答ヘッド + 本文) を分解する。
fn parse_wget_output(buffer: &[u8]) -> Result<TransportResponse, TransportError> {
    let Some(end) = find_terminator(buffer) else {
        return Err(incomplete());
    };
    if end > MAX_HEAD_BYTES {
        return Err(incomplete());
    }
    let (status, headers) = parse_head_block(&String::from_utf8_lossy(&buffer[..end]))?;
    if !(200..=599).contains(&status) {
        return Err(incomplete());
    }
    let body = &buffer[end + 4..];
    if body.len() > MAX_BODY_BYTES {
        return Err(too_large());
    }
    Ok(build_response(status, &headers, body, "wget"))
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

/// 子プロセスの stdout を上限つきで読む。
async fn read_stdout(
    stdout: Option<tokio::process::ChildStdout>,
) -> Result<Vec<u8>, TransportError> {
    let Some(mut stdout) = stdout else {
        return Ok(Vec::new());
    };
    let mut output = Vec::new();
    let mut buffer = vec![0u8; READ_BUFFER_BYTES];
    loop {
        let read = stdout
            .read(&mut buffer)
            .await
            .map_err(|_| TransportError::new(TransportErrorKind::Incomplete))?;
        if read == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(read) > MAX_TRANSPORT_OUTPUT {
            return Err(TransportError::new(TransportErrorKind::TooLarge));
        }
        output.extend_from_slice(&buffer[..read]);
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
    use url::Url;

    fn target(url: &str, address: &str) -> Target {
        Target {
            url: Url::parse(url).expect("target URL"),
            address: address.parse().expect("target address"),
        }
    }

    fn options(timeout: Duration) -> WgetOptions {
        WgetOptions {
            bin: OsString::from("wget"),
            cacert: None,
            timeout,
        }
    }

    fn arg_strings(
        target: &Target,
        headers: &[(String, String)],
        options: &WgetOptions,
    ) -> Vec<String> {
        build_wget_args(target, headers, options)
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn wget_arguments_match_the_node_construction() {
        let target = target("https://relay.invalid:8443/chapter?x=1", "93.184.216.34");
        let headers = vec![
            ("Host".to_string(), "relay.invalid:8443".to_string()),
            ("User-Agent".to_string(), "ua/1".to_string()),
            ("Cookie".to_string(), "sid=1".to_string()),
            ("Accept-Encoding".to_string(), "gzip".to_string()),
            ("X-Custom".to_string(), "café".to_string()),
        ];
        let mut options = options(Duration::from_secs(21));
        options.cacert = Some(PathBuf::from("ca.pem"));
        let args = arg_strings(&target, &headers, &options);
        assert_eq!(
            args,
            [
                "--no-config",
                "--no-verbose",
                "--save-headers",
                "--content-on-error",
                "--output-document=-",
                "--tries=1",
                "--max-redirect=0",
                "--connect-timeout=10",
                "--read-timeout=21",
                "--timeout=21",
                "--ca-certificate=ca.pem",
                "--user-agent=ua/1",
                "--header=Cookie: sid=1",
                "--header=X-Custom: café",
                "--",
                "https://relay.invalid:8443/chapter?x=1",
            ]
        );
    }

    #[test]
    fn wget_timeouts_are_whole_seconds_and_cacert_is_optional() {
        let target = target("https://relay.invalid/", "93.184.216.34");
        let args = arg_strings(&target, &[], &options(Duration::from_millis(300)));
        assert_eq!(
            args,
            [
                "--no-config",
                "--no-verbose",
                "--save-headers",
                "--content-on-error",
                "--output-document=-",
                "--tries=1",
                "--max-redirect=0",
                "--connect-timeout=1",
                "--read-timeout=1",
                "--timeout=1",
                "--",
                "https://relay.invalid/",
            ]
        );
        assert!(!args.iter().any(|arg| arg == "--ca-certificate=ca.pem"));
    }

    #[test]
    fn wget_output_parses_status_headers_and_body() {
        let output = b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nLocation: https://example.com/next\r\nSet-Cookie: a=1; Path=/\r\nSet-Cookie: b=2\r\n\r\nhello";
        let response = parse_wget_output(output).expect("response");
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "text/html; charset=utf-8");
        assert_eq!(
            response.location.as_deref(),
            Some("https://example.com/next")
        );
        assert_eq!(response.set_cookie, vec!["a=1; Path=/", "b=2"]);
        assert_eq!(response.body, b"hello");
        assert_eq!(response.via, "wget");

        let response =
            parse_wget_output(b"HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\n\r\nmissing")
                .expect("response");
        assert_eq!(response.status, 404);
        assert_eq!(response.body, b"missing");
    }

    #[test]
    fn wget_output_keeps_http_looking_bodies() {
        let response = parse_wget_output(b"HTTP/1.1 200 OK\r\n\r\nHTTP/1.1 302 Found\r\n\r\nrest")
            .expect("response");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"HTTP/1.1 302 Found\r\n\r\nrest".to_vec());
    }

    #[test]
    fn wget_output_requires_a_complete_final_head() {
        for output in [
            &b""[..],
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain",
            b"HTTP/1.1 199 Early\r\n\r\n",
            b"HTTP/1.1 600 Nope\r\n\r\n",
            b"garbage\r\n\r\n",
        ] {
            assert_eq!(
                parse_wget_output(output).unwrap_err().kind,
                TransportErrorKind::Incomplete,
                "{output:?}"
            );
        }
    }

    #[test]
    fn wget_output_oversized_body_is_too_large() {
        let mut output = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        output.resize(output.len() + MAX_BODY_BYTES + 1, b'x');
        assert_eq!(
            parse_wget_output(&output).unwrap_err().kind,
            TransportErrorKind::TooLarge
        );
    }

    #[test]
    fn wget_output_oversized_head_is_incomplete() {
        let mut output = b"HTTP/1.1 200 OK\r\n".to_vec();
        output.resize(MAX_HEAD_BYTES + 1, b'a');
        output.extend_from_slice(b"\r\n\r\n");
        assert_eq!(
            parse_wget_output(&output).unwrap_err().kind,
            TransportErrorKind::Incomplete
        );
    }

    #[test]
    fn connect_lines_are_validated_against_the_pinned_target() {
        let host = "relay.invalid";
        let port = 8443;
        assert_eq!(
            parse_connect_line("CONNECT relay.invalid:8443 HTTP/1.1"),
            Some(ConnectRequest {
                host: "relay.invalid".to_string(),
                port,
            })
        );
        for line in [
            "CONNECT relay.invalid:8443 HTTP/1.1",
            "connect RELAY.INVALID.:8443 HTTP/1.0",
            "CONNECT  relay.invalid:8443   HTTP/1.1",
            "CONNECT\trelay.invalid:8443 HTTP/1.1",
            "CONNECT relay.invalid:08443 HTTP/1.1",
            "CONNECT relay.invalid.:8443 HTTP/1.1",
        ] {
            assert!(
                authorize_connect(line, host, port),
                "{line} should be accepted"
            );
        }
        for line in [
            "",
            "CONNECT",
            "CONNECT relay.invalid HTTP/1.1",
            "CONNECT relay.invalid:8443",
            "CONNECT relay.invalid:8443 HTTP/2.0",
            "CONNECT relay.invalid:8443 HTTP/1.1 extra",
            "CONNECT :8443 HTTP/1.1",
            "CONNECT relay.invalid: HTTP/1.1",
            "CONNECT relay.invalid:999999 HTTP/1.1",
            "CONNECT relay.invalid :8443 HTTP/1.1",
            "CONNECT relay.invalid:abc HTTP/1.1",
            "CONNECTrelay.invalid:8443 HTTP/1.1",
            "GET relay.invalid:8443 HTTP/1.1",
            "CONNECT other.invalid:8443 HTTP/1.1",
            "CONNECT relay.invalid:8444 HTTP/1.1",
        ] {
            assert!(
                !authorize_connect(line, host, port),
                "{line} should be rejected"
            );
        }
    }

    #[test]
    fn connect_lines_handle_ipv6_brackets_and_case() {
        let host = "2606:4700:4700::1111";
        let port = 443;
        for line in [
            "CONNECT [2606:4700:4700::1111]:443 HTTP/1.1",
            "CONNECT [2606:4700:4700::1111]:443 HTTP/1.1",
            "CONNECT 2606:4700:4700::1111:443 HTTP/1.1",
        ] {
            assert!(
                authorize_connect(line, host, port),
                "{line} should be accepted"
            );
        }
        assert!(authorize_connect(
            "CONNECT [2606:4700:4700::ABCD]:443 HTTP/1.1",
            "2606:4700:4700::abcd",
            443
        ));
        assert!(!authorize_connect(
            "CONNECT [2606:4700:4700::2222]:443 HTTP/1.1",
            host,
            port
        ));
    }

    #[tokio::test]
    async fn pinned_proxy_rejects_mismatched_connect_and_tunnels_the_expected_one() {
        let echo = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("echo listener");
        let echo_port = echo.local_addr().expect("echo address").port();
        let echo_task = tokio::spawn(async move {
            let (mut stream, _) = echo.accept().await.expect("echo accept");
            let mut buffer = [0u8; 8];
            stream.read_exact(&mut buffer).await.expect("echo read");
            assert_eq!(&buffer, b"tunneled");
            stream.write_all(&buffer).await.expect("echo write");
            stream.flush().await.expect("echo flush");
        });

        let target = target(
            &format!("https://relay.invalid:{echo_port}/path"),
            "127.0.0.1",
        );
        let proxy = PinnedProxy::start(&target).await.expect("proxy");
        assert!(
            !proxy.established.load(Ordering::SeqCst),
            "no tunnel may exist before the first CONNECT"
        );

        // 期待しない CONNECT はトンネルせず切断する。
        let mut rejected = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port))
            .await
            .expect("connect rejected");
        rejected
            .write_all(b"CONNECT evil.invalid:443 HTTP/1.1\r\nHost: evil.invalid:443\r\n\r\n")
            .await
            .expect("write rejected");
        let mut buffer = [0u8; 16];
        let outcome = tokio::time::timeout(Duration::from_secs(5), rejected.read(&mut buffer))
            .await
            .expect("rejected CONNECT must not hang");
        assert!(
            matches!(outcome, Ok(0) | Err(_)),
            "unexpected rejected read: {outcome:?}"
        );
        assert!(
            !proxy.established.load(Ordering::SeqCst),
            "a rejected CONNECT must not mark the tunnel as established"
        );

        // 期待どおりの CONNECT はトンネルし、バイト列が往復する。
        let mut tunneled = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port))
            .await
            .expect("connect tunneled");
        let request = format!(
            "CONNECT relay.invalid:{echo_port} HTTP/1.1\r\nHost: relay.invalid:{echo_port}\r\n\r\n"
        );
        tunneled
            .write_all(request.as_bytes())
            .await
            .expect("write connect");
        let mut response_head = Vec::new();
        while !response_head.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = tunneled
                .read(&mut buffer)
                .await
                .expect("proxy response read");
            assert!(read > 0, "proxy closed before establishing the tunnel");
            response_head.extend_from_slice(&buffer[..read]);
        }
        assert!(response_head.starts_with(b"HTTP/1.1 200"));
        assert!(
            proxy.established.load(Ordering::SeqCst),
            "an authorized CONNECT must mark the tunnel as established"
        );
        tunneled.write_all(b"tunneled").await.expect("tunnel write");
        let mut echoed = [0u8; 8];
        tunneled.read_exact(&mut echoed).await.expect("tunnel read");
        assert_eq!(&echoed, b"tunneled");
        let joined = tokio::time::timeout(Duration::from_secs(5), echo_task)
            .await
            .expect("echo upstream finished");
        joined.expect("echo upstream task");
    }

    #[tokio::test]
    async fn wget_transport_rejects_plain_http_targets() {
        let target = target("http://relay.invalid/path", "127.0.0.1");
        let error = via_wget(&target, &[], &options(Duration::from_secs(1)))
            .await
            .unwrap_err();
        assert_eq!(error.kind, TransportErrorKind::Failed);
    }

    #[tokio::test]
    async fn wget_spawn_failures_are_not_installed() {
        let target = target("https://relay.invalid/path", "127.0.0.1");
        let options = WgetOptions {
            bin: OsString::from("narou-relay-missing-wget-binary"),
            cacert: None,
            timeout: Duration::from_secs(1),
        };
        let error = via_wget(&target, &[], &options).await.unwrap_err();
        assert_eq!(error.kind, TransportErrorKind::NotInstalled);
    }
}

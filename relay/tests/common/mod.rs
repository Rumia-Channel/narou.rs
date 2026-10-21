//! `relay/tests` 共有ハーネス。
//!
//! 実 HTTP の契約だけを検証するため、リレーへの生 HTTP クライアントと、
//! 応答をスクリプトできるテスト用上流 HTTP サーバーをここに置く。
//! 外部クレートは増やさず tokio / serde_json / base64 / url だけで書く。

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use narou_relay::config::{Availability, Config};
use narou_relay::security::{Target, TargetError, TargetResolver};
use narou_relay::server::{Logger, Relay, RelayHandle};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, Semaphore, watch};
use url::Url;

pub const TOKEN: &str = "relay-private-test-key";
pub const ALLOWED_HOST: &str = "relay.invalid";
pub const READ_TIMEOUT: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// 設定・リゾルバ
// ---------------------------------------------------------------------------

pub fn base_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("NAROU_RELAY_KEY".to_string(), TOKEN.to_string());
    env.insert(
        "NAROU_RELAY_ALLOWED_HOSTS".to_string(),
        ALLOWED_HOST.to_string(),
    );
    env
}

/// テスト既定の設定 (取得順 curl,wget)。
pub fn test_config() -> Config {
    Config::from_env_map(&base_env(), Availability { wget: true }).expect("test config")
}

/// 追加の環境変数から設定を作る。
pub fn config_with(pairs: &[(&str, &str)]) -> Config {
    let mut env = base_env();
    for (key, value) in pairs {
        env.insert((*key).to_string(), (*value).to_string());
    }
    Config::from_env_map(&env, Availability { wget: true }).expect("test config")
}

/// localhost 上流へ接続できるテスト専用 resolver (本番 `DnsResolver` の代わり)。
pub struct LocalResolver;

impl TargetResolver for LocalResolver {
    fn resolve<'a>(
        &'a self,
        raw_url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Target, TargetError>> + Send + 'a>> {
        Box::pin(async move {
            let url =
                Url::parse(raw_url).map_err(|_| TargetError::new(400, "invalid target URL"))?;
            let host = url.host_str().unwrap_or_default();
            if !matches!(url.scheme(), "http" | "https")
                || !url.username().is_empty()
                || url.password().is_some()
                || !matches!(host, "127.0.0.1" | "relay.invalid")
            {
                return Err(TargetError::new(400, "invalid local fixture URL"));
            }
            Ok(Target {
                url,
                address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            })
        })
    }
}

pub fn local_resolver() -> Arc<dyn TargetResolver> {
    Arc::new(LocalResolver)
}

/// 指定ステータスの `TargetError` を返す resolver (resolver 失敗経路の検証)。
pub struct FixedErrorResolver {
    pub status: u16,
    pub message: &'static str,
}

impl TargetResolver for FixedErrorResolver {
    fn resolve<'a>(
        &'a self,
        _raw_url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Target, TargetError>> + Send + 'a>> {
        Box::pin(async move { Err(TargetError::new(self.status, self.message)) })
    }
}

/// 解決が「開始した」ことだけ通知し、解除されるまで待つ resolver。
pub struct DelayedResolver {
    pub started: Arc<Notify>,
    pub release: Arc<Semaphore>,
}

impl TargetResolver for DelayedResolver {
    fn resolve<'a>(
        &'a self,
        raw_url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Target, TargetError>> + Send + 'a>> {
        let started = Arc::clone(&self.started);
        let release = Arc::clone(&self.release);
        Box::pin(async move {
            started.notify_one();
            let permit = release
                .acquire()
                .await
                .map_err(|_| TargetError::new(500, "internal server error"))?;
            drop(permit);
            LocalResolver.resolve(raw_url).await
        })
    }
}

// ---------------------------------------------------------------------------
// 起動済みリレー
// ---------------------------------------------------------------------------

pub struct TestRelay {
    pub addr: SocketAddr,
    pub base: String,
    pub logs: Arc<Mutex<Vec<String>>>,
    handle: RelayHandle,
}

impl TestRelay {
    pub async fn start(config: Config) -> Self {
        Self::start_with(config, local_resolver()).await
    }

    pub async fn start_with(config: Config, resolver: Arc<dyn TargetResolver>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind relay");
        let addr = listener.local_addr().expect("relay addr");
        let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&logs);
        let logger: Logger = Arc::new(move |line: &str| {
            sink.lock().expect("logs lock").push(line.to_string());
        });
        let relay = Arc::new(Relay::new(Arc::new(config), resolver, logger));
        let handle = RelayHandle::start(relay, listener).expect("start relay");
        assert_eq!(handle.addr, addr, "relay must listen on the bound address");
        Self {
            addr,
            base: format!("http://{addr}"),
            logs,
            handle,
        }
    }

    pub fn log_text(&self) -> String {
        self.logs.lock().expect("logs lock").join("\n")
    }

    pub async fn stop(self) {
        self.handle.shutdown_token().cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.handle.wait()).await;
    }
}

// ---------------------------------------------------------------------------
// base64 / body ヘルパ
// ---------------------------------------------------------------------------

pub fn b64_encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

pub fn b64_decode(text: &str) -> Vec<u8> {
    STANDARD.decode(text).expect("valid base64")
}

pub fn decode_body(value: &Value) -> Vec<u8> {
    let text = value
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("response has no base64 body: {value}"));
    b64_decode(text)
}

pub fn body_text(value: &Value) -> String {
    String::from_utf8(decode_body(value)).expect("body is UTF-8")
}

/// 上流の gzip テストで使う本文。
pub fn compressed_body() -> Vec<u8> {
    "compressed 日本語 body".repeat(100).into_bytes()
}

/// 16 MiB バイナリ本文 (境界値: 先頭 0x00 / 65535 番目 0x01 / 末尾 0xff)。
pub fn max_binary() -> Vec<u8> {
    let mut body = vec![0xa5u8; narou_relay::wire::MAX_BODY_BYTES];
    body[0] = 0;
    body[65535] = 1;
    let last = body.len() - 1;
    body[last] = 0xff;
    body
}

/// サイズちょうどの POST JSON (padding で水増しする)。
pub fn padded_json(url: &str, total_bytes: usize) -> Vec<u8> {
    let url_json = serde_json::to_string(url).expect("url json");
    let mut text = format!(r#"{{"url":{url_json},"padding":""#);
    let fixed = text.len() + 2;
    assert!(
        fixed <= total_bytes,
        "total_bytes={total_bytes} is smaller than the fixed JSON envelope"
    );
    text.push_str(&"x".repeat(total_bytes - fixed));
    text.push_str(r#""}"#);
    let bytes = text.into_bytes();
    assert_eq!(bytes.len(), total_bytes);
    bytes
}

/// 2 バイト文字で境界を超える POST JSON (文字数ではなくバイト数で数えること)。
pub fn multibyte_padded_json(url: &str) -> Vec<u8> {
    let url_json = serde_json::to_string(url).expect("url json");
    let mut text = format!(r#"{{"url":{url_json},"padding":""#);
    text.push_str(&"日".repeat(narou_relay::wire::MAX_REQUEST_BYTES / 2));
    text.push_str(r#""}"#);
    let bytes = text.into_bytes();
    assert!(bytes.len() > narou_relay::wire::MAX_REQUEST_BYTES);
    bytes
}

/// chunked 本文を組み立てる。
pub fn chunked_body(chunks: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in chunks {
        out.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

// ---------------------------------------------------------------------------
// 生 HTTP クライアント
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RawResponse {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RawResponse {
    /// 同名ヘッダを ", " で連結して返す (Node の headers と同じ)。
    pub fn header(&self, name: &str) -> Option<String> {
        let mut found = false;
        let mut value = String::new();
        for (header, header_value) in &self.headers {
            if header.eq_ignore_ascii_case(name) {
                if found {
                    value.push_str(", ");
                }
                value.push_str(header_value);
                found = true;
            }
        }
        found.then_some(value)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|error| {
            panic!(
                "relay response is not JSON ({error}): status={} headers={:?} body={:?}",
                self.status,
                self.headers,
                self.text()
            )
        })
    }

    pub fn error_message(&self) -> String {
        self.json()
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    pub fn expect_status(&self, status: u16) -> &Self {
        assert_eq!(
            self.status,
            status,
            "unexpected relay status; body={:?} headers={:?}",
            self.text(),
            self.headers
        );
        self
    }
}

/// HTTP/1.1 リクエストを組み立てる。`body` が None のときは Content-Length を
/// 自動付与しない。
pub fn build_request(
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> Vec<u8> {
    let mut head = format!("{method} {target} HTTP/1.1\r\nHost: relay.test\r\n");
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    let has_length = headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-length"));
    let has_transfer = headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"));
    if let Some(body) = body
        && !has_length
        && !has_transfer
    {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("connection"))
    {
        head.push_str("Connection: close\r\n");
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    if let Some(body) = body {
        bytes.extend_from_slice(body);
    }
    bytes
}

pub struct RawClient {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl RawClient {
    pub async fn connect(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr)
            .await
            .unwrap_or_else(|error| panic!("connect to relay {addr}: {error}"));
        let _ = stream.set_nodelay(true);
        Self {
            stream,
            buf: Vec::new(),
        }
    }

    pub async fn send_raw(&mut self, bytes: &[u8]) {
        self.stream
            .write_all(bytes)
            .await
            .unwrap_or_else(|error| panic!("write request: {error}"));
        self.stream
            .flush()
            .await
            .unwrap_or_else(|error| panic!("flush request: {error}"));
    }

    /// リクエストを組み立てて送る。`body` が None のときは本文も
    /// Content-Length も自動付与しない (宣言だけの境界試験用)。
    pub async fn send_request(
        &mut self,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) {
        let bytes = build_request(method, target, headers, body);
        self.send_raw(&bytes).await;
    }

    /// 書き込みエラーを無視して送る (上流が早期に 400 を返して閉じる境界試験用)。
    pub async fn send_lossy(&mut self, bytes: &[u8]) {
        let _ = self.stream.write_all(bytes).await;
        let _ = self.stream.flush().await;
    }

    pub async fn read_response(&mut self) -> RawResponse {
        self.read_response_within(READ_TIMEOUT).await
    }

    pub async fn read_response_within(&mut self, timeout: Duration) -> RawResponse {
        match tokio::time::timeout(timeout, self.read_response_inner()).await {
            Ok(response) => response,
            Err(_) => panic!("timed out after {timeout:?} waiting for a relay response"),
        }
    }

    async fn read_response_inner(&mut self) -> RawResponse {
        let head = loop {
            if let Some(position) = find_bytes(&self.buf, b"\r\n\r\n") {
                break self.buf.drain(..position + 4).collect::<Vec<u8>>();
            }
            let read = self
                .read_more()
                .await
                .unwrap_or_else(|error| panic!("read relay response head: {error}"));
            if read == 0 {
                panic!(
                    "relay closed before sending a response head; buffered={:?}",
                    String::from_utf8_lossy(&self.buf)
                );
            }
        };
        let (status, reason, headers) = parse_response_head(&head);
        let chunked = headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
        });
        let body = if chunked {
            self.read_chunked_body().await
        } else if let Some(length) = header_value(&headers, "content-length")
            .and_then(|value| value.parse::<usize>().ok())
        {
            while self.buf.len() < length {
                let read = self
                    .read_more()
                    .await
                    .unwrap_or_else(|error| panic!("read relay response body: {error}"));
                assert!(
                    read > 0,
                    "relay closed after {} of {length} response body bytes",
                    self.buf.len()
                );
            }
            self.buf.drain(..length).collect()
        } else {
            loop {
                let read = self
                    .read_more()
                    .await
                    .unwrap_or_else(|error| panic!("read relay response body: {error}"));
                if read == 0 {
                    break std::mem::take(&mut self.buf);
                }
            }
        };
        RawResponse {
            status,
            reason,
            headers,
            body,
        }
    }

    async fn read_chunked_body(&mut self) -> Vec<u8> {
        let mut body = Vec::new();
        loop {
            let line = self.read_line().await;
            let size_text = String::from_utf8_lossy(&line);
            let size = usize::from_str_radix(size_text.trim(), 16)
                .unwrap_or_else(|_| panic!("invalid chunk size line: {size_text:?}"));
            if size == 0 {
                loop {
                    if self.read_line().await.is_empty() {
                        break;
                    }
                }
                return body;
            }
            while self.buf.len() < size + 2 {
                self.read_more()
                    .await
                    .unwrap_or_else(|error| panic!("read chunk: {error}"));
            }
            body.extend_from_slice(&self.buf[..size]);
            self.buf.drain(..size + 2);
        }
    }

    async fn read_line(&mut self) -> Vec<u8> {
        loop {
            if let Some(position) = find_bytes(&self.buf, b"\r\n") {
                let line = self.buf[..position].to_vec();
                self.buf.drain(..position + 2);
                return line;
            }
            self.read_more()
                .await
                .unwrap_or_else(|error| panic!("read line: {error}"));
        }
    }

    async fn read_more(&mut self) -> io::Result<usize> {
        let mut chunk = [0u8; 65536];
        let read = self.stream.read(&mut chunk).await?;
        self.buf.extend_from_slice(&chunk[..read]);
        Ok(read)
    }

    /// サーバーが接続を閉じるまで読む。閉じなければ timeout で panic する。
    pub async fn read_to_eof(&mut self, timeout: Duration) -> Vec<u8> {
        let leftover = std::mem::take(&mut self.buf);
        let read = tokio::time::timeout(timeout, async {
            let mut out = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match self.stream.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(count) => out.extend_from_slice(&chunk[..count]),
                    Err(error) => panic!("read to EOF: {error}"),
                }
            }
            out
        })
        .await
        .unwrap_or_else(|_| panic!("relay did not close the connection within {timeout:?}"));
        let mut all = leftover;
        all.extend_from_slice(&read);
        all
    }
}

fn parse_response_head(head: &[u8]) -> (u16, String, Vec<(String, String)>) {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    assert!(
        version.starts_with("HTTP/"),
        "unexpected relay status line: {status_line:?}"
    );
    let status = parts
        .next()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("unexpected relay status line: {status_line:?}"));
    let reason = parts.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.to_string(), value.trim_start().to_string()));
        }
    }
    (status, reason, headers)
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ---------------------------------------------------------------------------
// 生 HTTP のワンショットヘルパ
// ---------------------------------------------------------------------------

pub async fn request_once(
    addr: SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> RawResponse {
    let mut client = RawClient::connect(addr).await;
    client.send_request(method, target, headers, body).await;
    client.read_response().await
}

pub async fn raw_request_once(addr: SocketAddr, raw: &[u8]) -> RawResponse {
    let mut client = RawClient::connect(addr).await;
    client.send_raw(raw).await;
    client.read_response().await
}

pub async fn post_proxy_once(
    addr: SocketAddr,
    token: Option<&str>,
    body: &[u8],
) -> RawResponse {
    let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
    if let Some(token) = token {
        headers.push(("X-Proxy-Token", token));
    }
    request_once(addr, "POST", "/proxy", &headers, Some(body)).await
}

pub async fn post_proxy_json(addr: SocketAddr, token: Option<&str>, value: &Value) -> RawResponse {
    post_proxy_once(addr, token, value.to_string().as_bytes()).await
}

/// POST /proxy (JSON: `{"url": ..., "headers": ...}`)。
pub async fn call_proxy(
    addr: SocketAddr,
    token: &str,
    url: &str,
    headers: &Value,
) -> RawResponse {
    let body = json!({ "url": url, "headers": headers });
    post_proxy_json(addr, Some(token), &body).await
}

/// POST /proxy で headers を省略する。
pub async fn call_proxy_without_headers(
    addr: SocketAddr,
    token: &str,
    url: &str,
) -> RawResponse {
    let body = json!({ "url": url });
    post_proxy_json(addr, Some(token), &body).await
}

/// GET /proxy?url=...&headers=<base64>。
pub async fn get_proxy_once(
    addr: SocketAddr,
    token: &str,
    url: &str,
    headers: Option<&str>,
) -> RawResponse {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("url", url);
    if let Some(headers) = headers {
        query.append_pair("headers", headers);
    }
    let target = format!("/proxy?{}", query.finish());
    request_once(addr, "GET", &target, &[("X-Proxy-Token", token)], None).await
}

pub async fn get_health(addr: SocketAddr) -> RawResponse {
    request_once(addr, "GET", "/health", &[], None).await
}

// ---------------------------------------------------------------------------
// テスト用上流 HTTP サーバー
// ---------------------------------------------------------------------------

/// 上流が受け取った 1 リクエスト。
#[derive(Debug, Clone)]
pub struct UpstreamRequest {
    pub method: String,
    /// リクエストターゲット (クエリ込み)。
    pub path: String,
    pub raw_head: Vec<u8>,
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Vec<u8>,
}

impl UpstreamRequest {
    pub fn path_only(&self) -> &str {
        self.path.split('?').next().unwrap_or_default()
    }

    /// 同名ヘッダを ", " で連結した生バイト列。
    pub fn header_bytes(&self, name: &str) -> Option<Vec<u8>> {
        join_header_values(&self.headers, name)
    }

    /// 1 バイト = 1 文字 (latin1) として読んだヘッダ値。
    pub fn header_latin1(&self, name: &str) -> Option<String> {
        self.header_bytes(name).map(latin1)
    }

    pub fn header_utf8(&self, name: &str) -> Option<String> {
        self.header_bytes(name)
            .map(|bytes| String::from_utf8(bytes).expect("header value is UTF-8"))
    }
}

fn latin1(bytes: Vec<u8>) -> String {
    bytes.iter().map(|&byte| byte as char).collect()
}

fn join_header_values(headers: &[(String, Vec<u8>)], name: &str) -> Option<Vec<u8>> {
    let mut found = false;
    let mut out = Vec::new();
    for (header, value) in headers {
        if header.eq_ignore_ascii_case(name) {
            if found {
                out.extend_from_slice(b", ");
            }
            out.extend_from_slice(value);
            found = true;
        }
    }
    found.then_some(out)
}

/// 上流が返す動作。
pub enum Action {
    /// 通常の HTTP 応答 (Content-Length を自動付与し keep-alive を維持する)。
    Response {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// 生バイトをそのまま書き、接続を閉じる。
    Raw(Vec<u8>),
    /// gzip 圧縮した本文を返す。
    Gzip { body: Vec<u8> },
    /// chunked で本文を返す。
    Chunked { chunks: Vec<Vec<u8>> },
    /// Content-Length を宣言して一部だけ送り FIN する。
    Truncated { declared: usize, sent: Vec<u8> },
    /// 接続を保持したまま一切応答しない (timeout / 切断試験用)。
    Hang,
}

pub type Handler = dyn Fn(&UpstreamRequest) -> Action + Send + Sync + 'static;

#[derive(Default)]
pub struct Seen {
    requests: AtomicUsize,
    connections: AtomicUsize,
    closed: AtomicUsize,
    raw_heads: Mutex<Vec<Vec<u8>>>,
    paths: Mutex<Vec<String>>,
}

impl Seen {
    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    pub fn closed(&self) -> usize {
        self.closed.load(Ordering::SeqCst)
    }

    pub fn paths(&self) -> Vec<String> {
        self.paths.lock().expect("paths lock").clone()
    }

    pub fn count_path(&self, path: &str) -> usize {
        self.paths
            .lock()
            .expect("paths lock")
            .iter()
            .filter(|seen| seen.as_str() == path)
            .count()
    }

    pub fn last_raw_head(&self) -> Vec<u8> {
        self.raw_heads
            .lock()
            .expect("heads lock")
            .last()
            .cloned()
            .unwrap_or_default()
    }

    /// 直近リクエストヘッダを 1 バイト = 1 文字として読む。
    pub fn last_head_latin1(&self) -> String {
        latin1(self.last_raw_head())
    }

    pub fn last_header_bytes(&self, name: &str) -> Option<Vec<u8>> {
        parse_header_bytes(&self.last_raw_head(), name)
    }

    pub fn last_header(&self, name: &str) -> Option<String> {
        self.last_header_bytes(name).map(latin1)
    }

    fn record(&self, request: &UpstreamRequest) {
        self.requests.fetch_add(1, Ordering::SeqCst);
        self.raw_heads
            .lock()
            .expect("heads lock")
            .push(request.raw_head.clone());
        self.paths
            .lock()
            .expect("paths lock")
            .push(request.path.clone());
    }

    pub async fn wait_requests(&self, count: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.requests() < count {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {count} upstream request(s); saw {}",
                self.requests()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    pub async fn wait_closed(&self, count: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.closed() < count {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {count} closed upstream socket(s); saw {}",
                self.closed()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

pub struct Upstream {
    pub addr: SocketAddr,
    pub seen: Arc<Seen>,
    shutdown: Option<watch::Sender<bool>>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl Upstream {
    pub async fn start(handler: impl Fn(&UpstreamRequest) -> Action + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind test upstream");
        let addr = listener.local_addr().expect("upstream addr");
        let seen = Arc::new(Seen::default());
        let handler: Arc<Handler> = Arc::new(handler);
        let (shutdown, receiver) = watch::channel(false);
        let join = tokio::spawn(serve_upstream(
            listener,
            handler,
            Arc::clone(&seen),
            receiver,
        ));
        Self {
            addr,
            seen,
            shutdown: Some(shutdown),
            join: Some(join),
        }
    }

    /// `relay.invalid` をホスト名にしたターゲット URL。
    pub fn target(&self, path: &str) -> String {
        format!("http://{}{path}", self.host_port())
    }

    /// ループバック IP をホスト名にしたターゲット URL。
    pub fn loopback_target(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.addr.port())
    }

    pub fn host_port(&self) -> String {
        format!("relay.invalid:{}", self.addr.port())
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub async fn stop(self) {}

    fn shutdown_now(&mut self) {
        if let Some(sender) = self.shutdown.take() {
            let _ = sender.send(true);
        }
        if let Some(join) = self.join.take() {
            join.abort();
        }
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.shutdown_now();
    }
}

async fn serve_upstream(
    listener: TcpListener,
    handler: Arc<Handler>,
    seen: Arc<Seen>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _peer)) => {
                        seen.connections.fetch_add(1, Ordering::SeqCst);
                        let handler = Arc::clone(&handler);
                        let seen = Arc::clone(&seen);
                        tokio::spawn(async move {
                            handle_upstream_connection(stream, handler, seen).await;
                        });
                    }
                    Err(_) => break,
                }
            }
        }
    }
}

enum HeadOutcome {
    Request(Vec<u8>),
    Closed,
    Garbage,
}

async fn handle_upstream_connection(mut stream: TcpStream, handler: Arc<Handler>, seen: Arc<Seen>) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let head = match read_request_head(&mut stream, &mut buf).await {
            HeadOutcome::Request(head) => head,
            HeadOutcome::Garbage => {
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                let _ = stream.flush().await;
                let _ = stream.shutdown().await;
                wait_eof(&mut stream).await;
                seen.closed.fetch_add(1, Ordering::SeqCst);
                return;
            }
            HeadOutcome::Closed => {
                seen.closed.fetch_add(1, Ordering::SeqCst);
                return;
            }
        };
        let Some(mut request) = parse_upstream_request(&head) else {
            let _ = stream.shutdown().await;
            seen.closed.fetch_add(1, Ordering::SeqCst);
            return;
        };
        // 本文 (GET しか来ない想定だが Content-Length があれば読む)。
        if let Some(length) = content_length(&request.headers) {
            while buf.len() < length {
                let mut chunk = [0u8; 4096];
                match stream.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(count) => buf.extend_from_slice(&chunk[..count]),
                    Err(_) => break,
                }
            }
            let take = length.min(buf.len());
            request.body = buf.drain(..take).collect();
        }
        seen.record(&request);
        match handler(&request) {
            Action::Response {
                status,
                headers,
                body,
            } => {
                if write_upstream_response(&mut stream, status, &headers, &body)
                    .await
                    .is_err()
                {
                    seen.closed.fetch_add(1, Ordering::SeqCst);
                    return;
                }
            }
            Action::Raw(bytes) => {
                let _ = stream.write_all(&bytes).await;
                let _ = stream.flush().await;
                let _ = stream.shutdown().await;
                wait_eof(&mut stream).await;
                seen.closed.fetch_add(1, Ordering::SeqCst);
                return;
            }
            Action::Gzip { body } => {
                let compressed = gzip_stored(&body);
                let headers = vec![
                    ("Content-Type".to_string(), "text/plain; charset=UTF-8".to_string()),
                    ("Content-Encoding".to_string(), "gzip".to_string()),
                ];
                if write_upstream_response(&mut stream, 200, &headers, &compressed)
                    .await
                    .is_err()
                {
                    seen.closed.fetch_add(1, Ordering::SeqCst);
                    return;
                }
            }
            Action::Chunked { chunks } => {
                let _ = write_upstream_chunked(&mut stream, &chunks).await;
                let _ = stream.shutdown().await;
                wait_eof(&mut stream).await;
                seen.closed.fetch_add(1, Ordering::SeqCst);
                return;
            }
            Action::Truncated { declared, sent } => {
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                     Content-Length: {declared}\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&sent).await;
                let _ = stream.flush().await;
                let _ = stream.shutdown().await;
                wait_eof(&mut stream).await;
                seen.closed.fetch_add(1, Ordering::SeqCst);
                return;
            }
            Action::Hang => {
                wait_eof(&mut stream).await;
                seen.closed.fetch_add(1, Ordering::SeqCst);
                return;
            }
        }
    }
}

async fn read_request_head(stream: &mut TcpStream, buf: &mut Vec<u8>) -> HeadOutcome {
    loop {
        if let Some(position) = find_bytes(buf, b"\r\n\r\n") {
            return HeadOutcome::Request(buf.drain(..position + 4).collect());
        }
        if !buf.is_empty() && !looks_like_http_head(buf) {
            return HeadOutcome::Garbage;
        }
        if buf.len() > 64 * 1024 {
            return HeadOutcome::Garbage;
        }
        let mut chunk = [0u8; 4096];
        match stream.read(&mut chunk).await {
            Ok(0) => {
                return if buf.is_empty() {
                    HeadOutcome::Closed
                } else {
                    HeadOutcome::Garbage
                };
            }
            Ok(count) => buf.extend_from_slice(&chunk[..count]),
            Err(_) => return HeadOutcome::Closed,
        }
    }
}

/// TLS ClientHello などを HTTP として待ち続けないための早期判定。
fn looks_like_http_head(buf: &[u8]) -> bool {
    if !buf.is_empty() && !buf[0].is_ascii_uppercase() {
        return false;
    }
    match buf.iter().position(|&byte| byte == b' ') {
        Some(position) => position <= 16 && buf[..position].iter().all(|byte| byte.is_ascii_uppercase()),
        // 先頭 16 バイトに空白が無ければ HTTP ではない。
        None => buf.len() < 16,
    }
}

fn parse_upstream_request(head: &[u8]) -> Option<UpstreamRequest> {
    let text_end = head.len().saturating_sub(4);
    let text = String::from_utf8_lossy(&head[..text_end]);
    let mut lines = text.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split(' ');
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim_start().as_bytes().to_vec()));
        }
    }
    Some(UpstreamRequest {
        method,
        path,
        raw_head: head.to_vec(),
        headers,
        body: Vec::new(),
    })
}

fn content_length(headers: &[(String, Vec<u8>)]) -> Option<usize> {
    join_header_values(headers, "content-length")
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| text.parse::<usize>().ok())
}

async fn write_upstream_response(
    stream: &mut TcpStream,
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason_phrase(status));
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: keep-alive\r\n\r\n",
        body.len()
    ));
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await
}

async fn write_upstream_chunked(stream: &mut TcpStream, chunks: &[Vec<u8>]) -> io::Result<()> {
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
              Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        )
        .await?;
    for chunk in chunks {
        stream
            .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
            .await?;
        stream.write_all(chunk).await?;
        stream.write_all(b"\r\n").await?;
    }
    stream.write_all(b"0\r\n\r\n").await?;
    stream.flush().await
}

async fn wait_eof(stream: &mut TcpStream) {
    let mut scratch = [0u8; 4096];
    loop {
        match stream.read(&mut scratch).await {
            Ok(0) | Err(_) => return,
            Ok(_) => continue,
        }
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "Unknown",
    }
}

fn parse_header_bytes(head: &[u8], name: &str) -> Option<Vec<u8>> {
    let mut found = false;
    let mut out = Vec::new();
    for line in head.split(|&byte| byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            break;
        }
        let Some(position) = line.iter().position(|&byte| byte == b':') else {
            continue;
        };
        let raw_name = &line[..position];
        if !raw_name.eq_ignore_ascii_case(name.as_bytes()) {
            continue;
        }
        let mut value = &line[position + 1..];
        while let Some((first, rest)) = value.split_first() {
            if *first == b' ' || *first == b'\t' {
                value = rest;
            } else {
                break;
            }
        }
        if found {
            out.extend_from_slice(b", ");
        }
        out.extend_from_slice(value);
        found = true;
    }
    found.then_some(out)
}

// ---------------------------------------------------------------------------
// gzip (deflate の stored ブロックだけで十分)
// ---------------------------------------------------------------------------

fn gzip_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1fu8, 0x8b, 0x08, 0, 0, 0, 0, 0, 0, 0xff];
    let mut remaining = data;
    if remaining.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
    }
    while !remaining.is_empty() {
        let take = remaining.len().min(65535);
        let last = take == remaining.len();
        out.push(if last { 0x01 } else { 0x00 });
        out.extend_from_slice(&(take as u16).to_le_bytes());
        out.extend_from_slice(&(!(take as u16)).to_le_bytes());
        out.extend_from_slice(&remaining[..take]);
        remaining = &remaining[take..];
    }
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xedb8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

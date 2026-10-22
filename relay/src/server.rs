//! リレー本体の HTTP/1.1 サーバー。
//!
//! 旧 Node 版 `scripts/relay/server.mjs` の移植。依存を増やさず、必要な範囲
//! (GET/POST の 2 ルート、Content-Length / chunked 本文、keep-alive、期限、
//! クライアント切断) だけを実装する。
//!
//! 契約:
//! - `GET /health` は認証不要で `{"ok":true,"fetch":"<取得順>"}`。
//! - `GET|POST /proxy` は `X-Proxy-Token` (SHA-256 の定数時間比較) 必須。
//! - 1 リクエストにつき上流取得は 1 回だけ。リダイレクトは辿らない。
//! - 期限 (既定 75 秒) とクライアント切断で処理 future を drop し、
//!   取得中の curl / wget を子プロセスの drop ガードで止める。
//! - 成功応答は JSON `{status, contentType, location?, setCookie, via, body}`。
//!   4xx/5xx は `{"error": "..."}` と `Connection: close`。

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::config::Config;
use crate::security::{normalize_host, Target, TargetError, TargetResolver};
use crate::transport::{
    self, CurlOptions, TransportError, TransportErrorKind, TransportResponse, WgetOptions,
};
use crate::wire::{self, MAX_HEAD_BYTES, MAX_REQUEST_BYTES};

/// keep-alive で次のリクエストを待つ時間 (Node の keepAliveTimeout 相当)。
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(5);
/// 応答書き込みの上限時間。読み手が居ない接続でタスクを止めない。
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// 同時接続数の上限。未認証クライアントによるソケット・タスク枯渇を防ぐ。
const MAX_CONNECTIONS: usize = 128;
/// 停止時に処理中のリクエストを待つ時間。
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// ログ出力 (既定は stderr)。値・URL・ヘッダは渡さないこと。
pub type Logger = Arc<dyn Fn(&str) + Send + Sync>;

/// 既定のロガー (stderr)。
pub fn stderr_logger() -> Logger {
    Arc::new(|line: &str| eprintln!("{line}"))
}

/// 協調的キャンセル用トークン。`cancel()` 後は `wait()` が解決する。
#[derive(Debug, Clone)]
pub struct CancelToken {
    sender: tokio::sync::watch::Sender<bool>,
}

impl CancelToken {
    pub fn new() -> Self {
        let (sender, _receiver) = tokio::sync::watch::channel(false);
        Self { sender }
    }

    pub fn cancel(&self) {
        let _ = self.sender.send(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.sender.borrow()
    }

    fn wait(&self) -> CancelWait {
        CancelWait {
            receiver: self.sender.subscribe(),
        }
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

struct CancelWait {
    receiver: tokio::sync::watch::Receiver<bool>,
}

impl CancelWait {
    async fn cancelled(mut self) {
        let _ = self.receiver.wait_for(|cancelled| *cancelled).await;
    }
}

/// 取得リレー。
pub struct Relay {
    config: Arc<Config>,
    resolver: Arc<dyn TargetResolver>,
    logger: Logger,
}

impl Relay {
    pub fn new(config: Arc<Config>, resolver: Arc<dyn TargetResolver>, logger: Logger) -> Self {
        Self {
            config,
            resolver,
            logger,
        }
    }

    /// 起動ログ用の `curl:path, wget:path` 表示。
    pub fn fetch_summary(&self) -> String {
        self.config.fetch_summary()
    }

    /// `/health` の `fetch` フィールド。
    pub fn fetch_names(&self) -> String {
        self.config.fetch_names()
    }

    pub fn fetch_order(&self) -> &[transport::TransportKind] {
        &self.config.fetch_order
    }

    /// 接続を受け付けて処理する。`shutdown` が cancel されるまで待つ。
    pub async fn serve(
        self: Arc<Self>,
        listener: TcpListener,
        shutdown: CancelToken,
    ) -> io::Result<()> {
        let mut tasks: JoinSet<()> = JoinSet::new();
        let permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
        loop {
            // JoinSet は join するまで完了タスクの結果を保持する。accept の
            // たびに回収しないと、接続数に比例してメモリが増え続ける。
            while tasks.try_join_next().is_some() {}
            let cancelled = shutdown.wait();
            tokio::select! {
                _ = cancelled.cancelled() => break,
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _peer)) => {
                            let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                                // 上限超過。応答を待たせず閉じ、受付自体は継続する。
                                drop(stream);
                                continue;
                            };
                            let relay = Arc::clone(&self);
                            tasks.spawn(async move {
                                let _permit = permit;
                                relay.handle_connection(Connection::new(stream)).await;
                            });
                        }
                        Err(_) => {
                            // 一時的な accept 失敗 (fd 枯渇等) で回り続けない。
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                    }
                }
            }
        }
        drop(listener);
        let grace = tokio::time::sleep(SHUTDOWN_GRACE);
        tokio::pin!(grace);
        loop {
            tokio::select! {
                _ = &mut grace => {
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                    break;
                }
                joined = tasks.join_next() => {
                    if joined.is_none() {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    async fn handle_connection(self: Arc<Self>, mut conn: Connection) {
        loop {
            let head = match conn.read_head(self.config.headers_timeout).await {
                Ok(Some(head)) => head,
                Ok(None) | Err(HeadError::Closed) | Err(HeadError::Io) => return,
                Err(HeadError::Timeout) => {
                    conn.finish_error_reply(408, &error_body("request timed out"))
                        .await;
                    return;
                }
                Err(HeadError::TooLarge) => {
                    conn.finish_error_reply(431, &error_body("request headers too large"))
                        .await;
                    return;
                }
                Err(HeadError::BadRequest) => {
                    conn.finish_error_reply(400, &error_body("invalid request"))
                        .await;
                    return;
                }
            };
            let request = match parse_request(&head) {
                Ok(request) => request,
                Err(message) => {
                    conn.finish_error_reply(400, &error_body(message)).await;
                    return;
                }
            };
            let Some(url) = request_url(&request.target) else {
                conn.finish_error_reply(400, &error_body("invalid request"))
                    .await;
                return;
            };
            let is_proxy = url.path() == "/proxy";
            if is_proxy {
                // 認証は本文読み取りより先に行う (旧 Node 版と同じ順序)。
                // 認証前に本文を読むと、トークンを知らないクライアントでも
                // リクエスト期限 (既定 75 秒) まで接続とタスクを保持できてしまう。
                let supplied = request.joined_header("x-proxy-token");
                if !token_matches(&self.config.token, &supplied) {
                    (self.logger)(
                        "narou relay proxy: authentication failed (X-Proxy-Token mismatch)",
                    );
                    conn.finish_error_reply(403, &error_body("forbidden"))
                        .await;
                    return;
                }
                if !matches!(request.method.as_str(), "GET" | "POST") {
                    conn.finish_error_reply(405, &error_body("method not allowed"))
                        .await;
                    return;
                }
            } else if url.path() == "/health" {
                if request.method != "GET" {
                    conn.finish_error_reply(405, &error_body("method not allowed"))
                        .await;
                    return;
                }
            } else {
                conn.finish_error_reply(404, &error_body("not found"))
                    .await;
                return;
            }
            let deadline = Instant::now() + self.config.request_timeout;
            // 本文を読むのは認証済みの POST /proxy だけ。GET や /health で
            // 本文が宣言されている場合は、未読のまま keep-alive すると次の
            // リクエストと混ざるため、この応答で接続を閉じる。
            let mut close = !request.keep_alive;
            let body = if is_proxy && request.method == "POST" {
                if request.expect_continue
                    && conn.write_raw(b"HTTP/1.1 100 Continue\r\n\r\n").await.is_err()
                {
                    return;
                }
                match conn.read_body(&request, deadline).await {
                    Ok(body) => body,
                    Err(BodyError::Io) => return,
                    Err(BodyError::Timeout) => {
                        conn.finish_error_reply(502, &error_body("request timed out"))
                            .await;
                        return;
                    }
                    Err(BodyError::TooLarge) | Err(BodyError::BadRequest) => {
                        conn.finish_error_reply(400, &error_body("invalid request"))
                            .await;
                        return;
                    }
                }
            } else {
                close = close || request.expects_body();
                Vec::new()
            };
            // 期限切れ・クライアント切断では process の future を drop し、
            // 取得中の子プロセスを止める。
            let process = self.process(&request, &body);
            tokio::pin!(process);
            let reply = tokio::select! {
                reply = &mut process => reply,
                _ = tokio::time::sleep_until(deadline) => {
                    (self.logger)("narou relay request: cancelled");
                    Reply::RequestTimedOut
                }
                _ = conn.wait_disconnect() => Reply::Disconnected,
            };
            match reply {
                Reply::Disconnected => return,
                Reply::RawJson {
                    status,
                    body,
                    force_close,
                } => {
                    close = close || force_close || status >= 400;
                    if conn.reply_json(status, &body, close).await.is_err() {
                        return;
                    }
                }
                Reply::Upstream(response) => {
                    let metadata = response.metadata();
                    let body = match wire::encode_response(&metadata, &response.body) {
                        Ok(body) => body,
                        Err(_) => {
                            (self.logger)("narou relay request: too-large");
                            let _ = conn
                                .reply_json(502, &error_body("response too large"), true)
                                .await;
                            return;
                        }
                    };
                    if conn.reply_json(200, &body, close).await.is_err() {
                        return;
                    }
                }
                other => {
                    let (status, message) = other.into_error();
                    if conn.reply_json(status, &error_body(message), true).await.is_err() {
                        return;
                    }
                    return;
                }
            }
            if close {
                return;
            }
        }
    }

    async fn process(&self, request: &Request, body: &[u8]) -> Reply {
        let Some(url) = request_url(&request.target) else {
            return Reply::BadRequest("invalid request");
        };
        if url.path() == "/health" {
            return if request.method == "GET" {
                // 旧 Node 版と同じ並び (`ok` → `fetch`) で返す。値は
                // トランスポート名とカンマだけなので JSON エスケープは不要。
                let body = format!("{{\"ok\":true,\"fetch\":\"{}\"}}", self.fetch_names());
                Reply::RawJson {
                    status: 200,
                    body: body.into_bytes(),
                    force_close: false,
                }
            } else {
                Reply::MethodNotAllowed
            };
        }
        if url.path() != "/proxy" {
            return Reply::NotFound;
        }
        let supplied = request.joined_header("x-proxy-token");
        if !token_matches(&self.config.token, &supplied) {
            (self.logger)("narou relay proxy: authentication failed (X-Proxy-Token mismatch)");
            return Reply::Forbidden;
        }
        let (target_url, headers) = match request.method.as_str() {
            "POST" => {
                let payload = match wire::parse_post_payload(body) {
                    Ok(payload) => payload,
                    Err(_) => return Reply::BadRequest("invalid request"),
                };
                let target_url = match wire::payload_url(&payload) {
                    Ok(url) => url.to_string(),
                    Err(_) => return Reply::BadRequest("invalid target URL"),
                };
                let headers = match wire::header_pairs(payload.headers.as_ref()) {
                    Ok(headers) => headers,
                    Err(_) => return Reply::BadRequest("invalid request headers"),
                };
                (target_url, headers)
            }
            "GET" => {
                let mut target_url: Option<String> = None;
                let mut raw_headers: Option<String> = None;
                for (key, value) in url.query_pairs() {
                    match key.as_ref() {
                        "url" if target_url.is_none() => target_url = Some(value.into_owned()),
                        "headers" if raw_headers.is_none() => raw_headers = Some(value.into_owned()),
                        _ => {}
                    }
                }
                let target_url = target_url.unwrap_or_default();
                let headers = match raw_headers {
                    None => Vec::new(),
                    Some(raw) => match wire::decode_header_json(&raw) {
                        Ok(value) => match wire::header_pairs(Some(&value)) {
                            Ok(headers) => headers,
                            Err(_) => return Reply::BadRequest("invalid request headers"),
                        },
                        Err(_) => return Reply::BadRequest("invalid request"),
                    },
                };
                (target_url, headers)
            }
            _ => return Reply::MethodNotAllowed,
        };
        let target = match self.resolver.resolve(&target_url).await {
            Ok(target) => target,
            Err(error) => {
                (self.logger)(&format!("narou relay target: {}", error.message));
                return Reply::Target(error);
            }
        };
        let headers = match forward_headers(&headers, &target) {
            Ok(headers) => headers,
            Err(()) => return Reply::BadRequest("invalid request headers"),
        };
        let curl = CurlOptions {
            bin: self.config.curl.bin.clone(),
            cacert: self.config.cacert.clone(),
            timeout: self.config.curl_timeout,
        };
        let wget = WgetOptions {
            bin: self.config.wget.bin.clone(),
            cacert: self.config.cacert.clone(),
            timeout: self.config.wget_timeout,
        };
        let mut last_error: Option<TransportError> = None;
        for kind in &self.config.fetch_order {
            match transport::fetch(*kind, &target, &headers, &curl, &wget).await {
                Ok(response) => {
                    let metadata = response.metadata();
                    if wire::metadata_wire_len(&metadata) > wire::MAX_METADATA_BYTES {
                        (self.logger)(&format!("narou relay {}: too-large", kind.name()));
                        (self.logger)("narou relay request: too-large");
                        return Reply::Transport(TransportError::new(TransportErrorKind::TooLarge));
                    }
                    if response.status == 403 {
                        (self.logger)("narou relay upstream status 403");
                    }
                    return Reply::Upstream(response);
                }
                Err(error) => {
                    (self.logger)(&format!("narou relay {}: {}", kind.name(), error.reason()));
                    if error.is_fatal() {
                        (self.logger)(&format!("narou relay request: {}", error.reason()));
                        return Reply::Transport(error);
                    }
                    last_error = Some(error);
                }
            }
        }
        let error = last_error.unwrap_or_else(|| TransportError::new(TransportErrorKind::Failed));
        (self.logger)(&format!("narou relay request: {}", error.reason()));
        Reply::Transport(error)
    }
}

/// 起動済みリレーのハンドル。
pub struct RelayHandle {
    pub addr: SocketAddr,
    shutdown: CancelToken,
    join: tokio::task::JoinHandle<io::Result<()>>,
}

impl RelayHandle {
    /// バインド済みリスナーで処理を開始する。
    pub fn start(relay: Arc<Relay>, listener: TcpListener) -> io::Result<Self> {
        let addr = listener.local_addr()?;
        let shutdown = CancelToken::new();
        let token = shutdown.clone();
        let join = tokio::spawn(async move { relay.serve(listener, token).await });
        Ok(Self {
            addr,
            shutdown,
            join,
        })
    }

    pub fn shutdown_token(&self) -> CancelToken {
        self.shutdown.clone()
    }

    /// 停止する (グレース付き)。
    pub async fn shutdown(self) -> io::Result<()> {
        self.shutdown.cancel();
        self.join.await.map_err(io::Error::other)?
    }

    /// `shutdown_token().cancel()` されるまで待つ。
    pub async fn wait(self) -> io::Result<()> {
        self.join.await.map_err(io::Error::other)?
    }
}

/// 処理結果。
enum Reply {
    RawJson {
        status: u16,
        body: Vec<u8>,
        force_close: bool,
    },
    Upstream(TransportResponse),
    BadRequest(&'static str),
    NotFound,
    MethodNotAllowed,
    Forbidden,
    Target(TargetError),
    Transport(TransportError),
    RequestTimedOut,
    Disconnected,
}

impl Reply {
    fn into_error(self) -> (u16, &'static str) {
        match self {
            Self::BadRequest(message) => (400, message),
            Self::NotFound => (404, "not found"),
            Self::MethodNotAllowed => (405, "method not allowed"),
            Self::Forbidden => (403, "forbidden"),
            Self::Target(error) => (error.status, error.message),
            Self::Transport(error) => (502, error.message()),
            Self::RequestTimedOut => (502, "request cancelled"),
            Self::RawJson { .. } | Self::Upstream(_) | Self::Disconnected => {
                (500, "internal server error")
            }
        }
    }
}

fn error_body(message: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"error": message})).unwrap_or_else(|_| b"{\"error\":\"internal server error\"}".to_vec())
}

fn token_matches(expected: &str, supplied: &str) -> bool {
    let expected = Sha256::digest(expected.as_bytes());
    let supplied = Sha256::digest(supplied.as_bytes());
    bool::from(expected.as_slice().ct_eq(supplied.as_slice()))
}

fn request_url(target: &str) -> Option<url::Url> {
    if target.starts_with("http://") || target.starts_with("https://") {
        url::Url::parse(target).ok()
    } else {
        url::Url::parse(&format!("http://localhost{target}")).ok()
    }
}

fn forward_headers(
    pairs: &[(String, String)],
    target: &Target,
) -> Result<Vec<(String, String)>, ()> {
    let target_host = normalize_host(target.url.host_str().ok_or(())?).map_err(|_| ())?;
    let mut hosts = 0usize;
    let mut out = Vec::with_capacity(pairs.len());
    for (name, value) in pairs {
        if name.eq_ignore_ascii_case("host") {
            hosts += 1;
            if hosts > 1 {
                return Err(());
            }
            let parsed = url::Url::parse(&format!("{}://{value}", target.url.scheme())).map_err(|_| ())?;
            if !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.path() != "/"
                || parsed.query().is_some()
                || parsed.fragment().is_some()
            {
                return Err(());
            }
            let host = normalize_host(parsed.host_str().ok_or(())?).map_err(|_| ())?;
            if host != target_host || parsed.port() != target.url.port() {
                return Err(());
            }
        }
        out.push((name.clone(), value.clone()));
    }
    Ok(out)
}

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&byte| byte as char).collect()
}

/// パース済みの HTTP リクエスト。
struct Request {
    method: String,
    target: String,
    headers: Vec<(String, Vec<u8>)>,
    content_length: Option<usize>,
    chunked: bool,
    expect_continue: bool,
    keep_alive: bool,
}

impl Request {
    fn joined_header(&self, name: &str) -> String {
        let mut out = String::new();
        for (header, value) in &self.headers {
            if header.eq_ignore_ascii_case(name) {
                if !out.is_empty() {
                    out.push_str(", ");
                }
                out.push_str(&latin1(value));
            }
        }
        out
    }

    /// 本文 (Content-Length > 0 または chunked) が宣言されているか。
    fn expects_body(&self) -> bool {
        self.chunked || self.content_length.is_some_and(|length| length > 0)
    }
}

fn parse_request(head: &[u8]) -> Result<Request, &'static str> {
    let invalid = || "invalid request";
    let mut headers = [httparse::EMPTY_HEADER; 128];
    let mut parsed = httparse::Request::new(&mut headers);
    match parsed.parse(head) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return Err(invalid()),
    }
    let method = parsed.method.ok_or_else(invalid)?.to_string();
    let target = parsed.path.ok_or_else(invalid)?.to_string();
    let minor = parsed.version.unwrap_or(1);
    let mut list: Vec<(String, Vec<u8>)> = Vec::with_capacity(parsed.headers.len());
    for header in parsed.headers {
        list.push((header.name.to_string(), header.value.to_vec()));
    }
    let mut request = Request {
        method,
        target,
        headers: list,
        content_length: None,
        chunked: false,
        expect_continue: false,
        keep_alive: minor >= 1,
    };
    let transfer_encoding = request.joined_header("transfer-encoding").to_ascii_lowercase();
    if !transfer_encoding.is_empty() {
        if !transfer_encoding.split(',').any(|token| token.trim() == "chunked") {
            return Err(invalid());
        }
        request.chunked = true;
    }
    let mut content_length: Option<usize> = None;
    for (name, value) in &request.headers {
        if !name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        let text = latin1(value);
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid());
        }
        let length = text.parse::<usize>().map_err(|_| invalid())?;
        if let Some(previous) = content_length
            && previous != length
        {
            return Err(invalid());
        }
        content_length = Some(length);
    }
    if request.chunked && content_length.is_some() {
        return Err(invalid());
    }
    request.content_length = content_length;
    request.expect_continue = minor >= 1
        && request
            .joined_header("expect")
            .to_ascii_lowercase()
            .contains("100-continue");
    let connection = request.joined_header("connection").to_ascii_lowercase();
    request.keep_alive = if minor >= 1 {
        !connection.contains("close")
    } else {
        connection.contains("keep-alive")
    };
    Ok(request)
}

enum HeadError {
    Closed,
    Timeout,
    TooLarge,
    BadRequest,
    Io,
}

enum BodyError {
    Io,
    Timeout,
    TooLarge,
    BadRequest,
}

struct Connection {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl Connection {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            buf: Vec::with_capacity(1024),
        }
    }

    async fn read_head(&mut self, headers_timeout: Duration) -> Result<Option<Vec<u8>>, HeadError> {
        let mut data_since: Option<Instant> = if self.buf.is_empty() {
            None
        } else {
            Some(Instant::now())
        };
        loop {
            if let Some(end) = find_head_end(&self.buf) {
                let head: Vec<u8> = self.buf.drain(..end + 4).collect();
                return Ok(Some(head));
            }
            if self.buf.len() > MAX_HEAD_BYTES {
                return Err(HeadError::TooLarge);
            }
            let wait = match data_since {
                None => KEEP_ALIVE_TIMEOUT,
                Some(start) => headers_timeout.saturating_sub(start.elapsed()),
            };
            if wait.is_zero() {
                return Err(if data_since.is_none() {
                    HeadError::Closed
                } else {
                    HeadError::Timeout
                });
            }
            let mut chunk = [0u8; 8192];
            match tokio::time::timeout(wait, self.stream.read(&mut chunk)).await {
                Err(_) => {
                    return Err(if data_since.is_none() {
                        HeadError::Closed
                    } else {
                        HeadError::Timeout
                    });
                }
                Ok(Err(_)) => return Err(HeadError::Io),
                Ok(Ok(0)) => {
                    return if self.buf.is_empty() {
                        Ok(None)
                    } else {
                        Err(HeadError::BadRequest)
                    };
                }
                Ok(Ok(count)) => {
                    self.buf.extend_from_slice(&chunk[..count]);
                    if data_since.is_none() {
                        data_since = Some(Instant::now());
                    }
                }
            }
        }
    }

    async fn read_body(&mut self, request: &Request, deadline: Instant) -> Result<Vec<u8>, BodyError> {
        if request.chunked {
            return self.read_chunked(deadline).await;
        }
        let Some(length) = request.content_length else {
            return Ok(Vec::new());
        };
        if length > MAX_REQUEST_BYTES {
            return Err(BodyError::TooLarge);
        }
        while self.buf.len() < length {
            self.fill(deadline).await?;
        }
        Ok(self.buf.drain(..length).collect())
    }

    async fn read_chunked(&mut self, deadline: Instant) -> Result<Vec<u8>, BodyError> {
        let mut body = Vec::new();
        loop {
            let line = self.take_line(deadline).await?;
            let size_part = line.split(|&byte| byte == b';').next().unwrap_or(&line);
            let text = std::str::from_utf8(size_part)
                .map_err(|_| BodyError::BadRequest)?
                .trim();
            let size = usize::from_str_radix(text, 16).map_err(|_| BodyError::BadRequest)?;
            if size == 0 {
                loop {
                    let trailer = self.take_line(deadline).await?;
                    if trailer.is_empty() {
                        break;
                    }
                }
                return Ok(body);
            }
            if body.len().saturating_add(size) > MAX_REQUEST_BYTES {
                return Err(BodyError::TooLarge);
            }
            while self.buf.len() < size + 2 {
                self.fill(deadline).await?;
            }
            body.extend_from_slice(&self.buf[..size]);
            self.buf.drain(..size);
            if &self.buf[..2] != b"\r\n" {
                return Err(BodyError::BadRequest);
            }
            self.buf.drain(..2);
        }
    }

    async fn take_line(&mut self, deadline: Instant) -> Result<Vec<u8>, BodyError> {
        loop {
            if let Some(position) = self.buf.windows(2).position(|window| window == b"\r\n") {
                if position > 8192 {
                    return Err(BodyError::BadRequest);
                }
                let line: Vec<u8> = self.buf.drain(..position + 2).collect();
                return Ok(line[..position].to_vec());
            }
            if self.buf.len() > 8192 {
                return Err(BodyError::BadRequest);
            }
            self.fill(deadline).await?;
        }
    }

    async fn fill(&mut self, deadline: Instant) -> Result<(), BodyError> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(BodyError::Timeout);
        }
        let mut chunk = [0u8; 8192];
        match tokio::time::timeout(remaining, self.stream.read(&mut chunk)).await {
            Err(_) => Err(BodyError::Timeout),
            Ok(Err(_)) => Err(BodyError::Io),
            Ok(Ok(0)) => Err(BodyError::BadRequest),
            Ok(Ok(count)) => {
                self.buf.extend_from_slice(&chunk[..count]);
                Ok(())
            }
        }
    }

    /// 処理中にクライアントが切れたかを監視する (データ到着では解決しない)。
    async fn wait_disconnect(&mut self) {
        let mut byte = [0u8; 1];
        loop {
            match self.stream.peek(&mut byte).await {
                Ok(0) => return,
                Ok(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                Err(_) => return,
            }
        }
    }

    async fn write_raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        tokio::time::timeout(WRITE_TIMEOUT, self.stream.write_all(bytes))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "write timed out"))??;
        Ok(())
    }

    async fn reply_json(&mut self, status: u16, body: &[u8], close: bool) -> io::Result<()> {
        let head = format!(
            "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: {}\r\n\r\n",
            status,
            reason_phrase(status),
            body.len(),
            if close { "close" } else { "keep-alive" },
        );
        tokio::time::timeout(WRITE_TIMEOUT, async {
            self.stream.write_all(head.as_bytes()).await?;
            self.stream.write_all(body).await?;
            self.stream.flush().await
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "write timed out"))?
    }

    /// 早期エラー (ヘッダ・本文の読み出し前後) への応答を書いて接続を閉じる。
    ///
    /// リクエスト本文を読み切る前に閉じると、未読データのぶんカーネルが RST を
    /// 返して応答がクライアントへ届かないことがある。Node 版の `req.resume()`
    /// と同じく、送信側を半クローズしてから短時間だけ受信を読み捨てる。
    async fn finish_error_reply(&mut self, status: u16, body: &[u8]) {
        let _ = self.reply_json(status, body, true).await;
        let _ = self.stream.shutdown().await;
        let mut scratch = [0u8; 4096];
        let drain = async {
            loop {
                match self.stream.read(&mut scratch).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        };
        let _ = tokio::time::timeout(Duration::from_millis(250), drain).await;
    }
}

fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(url: &str, address: &str) -> Target {
        Target {
            url: url::Url::parse(url).unwrap(),
            address: address.parse().unwrap(),
        }
    }

    #[test]
    fn token_comparison_accepts_only_the_exact_token() {
        assert!(token_matches("secret", "secret"));
        assert!(!token_matches("secret", "secre"));
        assert!(!token_matches("secret", "secret "));
        assert!(!token_matches("secret", ""));
        assert!(!token_matches("secret", "Secret"));
    }

    #[test]
    fn host_forwarding_must_name_the_validated_target() {
        let target = target("https://relay.invalid:8443/chapter", "127.0.0.1");
        let pairs = vec![
            ("Accept".to_string(), "text/html".to_string()),
            ("Host".to_string(), "relay.invalid:8443".to_string()),
        ];
        assert_eq!(forward_headers(&pairs, &target).unwrap(), pairs);
        for host in [
            "relay.invalid",
            "relay.invalid:443",
            "metadata.google.internal",
            "relay.invalid:8443",
        ] {
            let pairs = vec![("Host".to_string(), host.to_string())];
            let result = forward_headers(&pairs, &target);
            if host == "relay.invalid:8443" {
                assert!(result.is_ok());
            } else {
                assert!(result.is_err(), "{host}");
            }
        }
        for bad in ["user@relay.invalid:8443", "relay.invalid:8443/path", "relay.invalid:8443?x=1", "relay.invalid:8443#f"] {
            let pairs = vec![("Host".to_string(), bad.to_string())];
            assert!(forward_headers(&pairs, &target).is_err(), "{bad}");
        }
        let duplicate = vec![
            ("Host".to_string(), "relay.invalid:8443".to_string()),
            ("host".to_string(), "relay.invalid:8443".to_string()),
        ];
        assert!(forward_headers(&duplicate, &target).is_err());
        // 大文字小文字を保持したまま通す。
        let casing = vec![("Sec-Fetch-Dest".to_string(), "document".to_string())];
        assert_eq!(forward_headers(&casing, &target).unwrap(), casing);
    }

    #[test]
    fn request_parser_handles_methods_bodies_and_connection_semantics() {
        let head = b"POST /proxy HTTP/1.1\r\nHost: relay.invalid\r\nContent-Length: 2\r\nX-Proxy-Token: t\r\nConnection: keep-alive\r\n\r\n";
        let request = parse_request(head).unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/proxy");
        assert_eq!(request.content_length, Some(2));
        assert!(!request.chunked);
        assert!(request.keep_alive);
        assert_eq!(request.joined_header("x-proxy-token"), "t");

        let head = b"GET /health HTTP/1.0\r\nConnection: keep-alive\r\n\r\n";
        let request = parse_request(head).unwrap();
        assert!(request.keep_alive);
        let head = b"GET /health HTTP/1.0\r\n\r\n";
        assert!(!parse_request(head).unwrap().keep_alive);
        let head = b"GET /health HTTP/1.1\r\nConnection: close\r\n\r\n";
        assert!(!parse_request(head).unwrap().keep_alive);

        let chunked = b"POST /proxy HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";
        let request = parse_request(chunked).unwrap();
        assert!(request.chunked);
        let both = b"POST /proxy HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Length: 1\r\n\r\n";
        assert!(parse_request(both).is_err());
        let duplicate_lengths = b"POST /proxy HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n";
        assert!(parse_request(duplicate_lengths).is_err());
        let unsupported = b"POST /proxy HTTP/1.1\r\nTransfer-Encoding: gzip\r\n\r\n";
        assert!(parse_request(unsupported).is_err());
    }

    #[test]
    fn request_url_accepts_origin_and_absolute_forms() {
        assert_eq!(request_url("/health").unwrap().path(), "/health");
        assert_eq!(
            request_url("http://localhost/proxy?url=x").unwrap().path(),
            "/proxy"
        );
        assert!(request_url(":::").is_none());
    }

    #[test]
    fn error_replies_keep_the_node_messages() {
        assert_eq!(Reply::Forbidden.into_error(), (403, "forbidden"));
        assert_eq!(Reply::NotFound.into_error(), (404, "not found"));
        assert_eq!(
            Reply::BadRequest("invalid request headers").into_error(),
            (400, "invalid request headers")
        );
        assert_eq!(
            Reply::Transport(TransportError::new(TransportErrorKind::TimedOut)).into_error(),
            (502, "upstream request timed out")
        );
        assert_eq!(Reply::RequestTimedOut.into_error(), (502, "request cancelled"));
        assert_eq!(
            Reply::Target(TargetError::new(400, "target host is not allowed")).into_error(),
            (400, "target host is not allowed")
        );
    }

    #[test]
    fn find_head_end_ignores_partial_terminators() {
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n\r"), None);
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n\r\n"), Some(14));
        assert_eq!(find_head_end(b"\r\n\r\nbody"), Some(0));
    }
}

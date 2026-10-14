use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures::future::{self, Either};
use narou_rs::downloader::security::{
    CONNECT_TIMEOUT_SECS, MAX_REDIRECTS, READ_TIMEOUT_SECS, TOTAL_TIMEOUT_SECS,
};
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::http::{RedirectChain, validate_http_url_syntax};
use narou_rs::platform::http1::{Http1ResponseDecoder, SocketTarget, encode_request};
use narou_rs::platform::relay::{RelayRequest, RelayResponse, response_wire_limit};
use narou_rs::platform::{
    CookieStore, HttpClient, HttpMethod, HttpRequest, HttpResponse, PlatformFuture,
};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use worker::web_sys::{ReadableStreamDefaultReader, WritableStreamDefaultWriter};
use worker::worker_sys;
use worker::{Fetch, Headers, Method, Request, RequestInit, RequestRedirect, console_warn};

use crate::budget::SubrequestBudget;

/// 1 レスポンスの上限。
///
/// Workers の 128 MiB 予算のうち、取得したバイト列に許す枠。実データのうごイラは
/// zip 4.3 MiB (19 フレーム 1920×1080) で、挿絵も数 MB なので 16 MiB で足りる。
/// 大きいのは「入力」ではなく APNG の出力側で、そちらは
/// `illustration_animation` の上限 (40 MiB) で別に制限している。
pub const MAX_WORKER_HTTP_BODY: usize = 16 * 1024 * 1024;


#[derive(Clone, Default)]
pub struct WorkerHttpClient {
    subrequests: SubrequestBudget,
    /// 保存済みログイン Cookie。`Set-Cookie` の書き戻しにだけ使う。
    cookie_store: Option<Arc<dyn CookieStore>>,
    /// 送信する User-Agent。native の `NativeHttpClient` と同じ役割で、
    /// リクエストが自前の UA を持たないときだけ付ける (サイト別の UA を尊重)。
    user_agent: String,
    /// 踏み台 (SORAHOST リレー) の接続先と認証トークン。
    /// `SORAHOST_PROXY_ENDPOINT` / `SORAHOST_PROXY_KEY` が揃ったときだけ Some。
    relay: Option<RelayConfig>,
}

/// A single fallback hop; neither transport follows redirects itself.
enum HopExchange<'a> {
    Socket,
    Relay(&'a RelayConfig),
}

/// 踏み台リレーの接続情報。
#[derive(Clone)]
struct RelayConfig {
    /// 踏み台のベース URL (`http://<IP>:<port>`)。オリジン部分だけが使われる。
    base_url: String,
    /// `X-Proxy-Token` に入れる共有トークン。
    token: String,
}

impl std::fmt::Debug for WorkerHttpClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerHttpClient")
            .field("cookie_store", &self.cookie_store.is_some())
            .finish()
    }
}

impl WorkerHttpClient {
    pub fn new(subrequests: SubrequestBudget) -> Self {
        Self {
            subrequests,
            cookie_store: None,
            user_agent: String::new(),
            relay: None,
        }
    }

    /// 送信する User-Agent を設定する (`narou_rs::downloader::resolve_user_agent`)。
    pub fn with_user_agent(mut self, user_agent: String) -> Self {
        self.user_agent = user_agent;
        self
    }

    /// SORAHOST リレー経由の取得段を有効にする。
    ///
    /// `base_url` は `http://<IP>:<port>` 形式。スキームが無い値
    /// (`<IP>:<port>` だけの環境変数) は `http://` を補う。末尾の `/` の有無は
    /// `RelayRequest::new` が origin に正規化するため問わない。
    pub fn with_relay(mut self, base_url: String, token: String) -> Self {
        let base_url = if base_url.starts_with("http://") || base_url.starts_with("https://") {
            base_url
        } else {
            format!("http://{base_url}")
        };
        self.relay = Some(RelayConfig { base_url, token });
        self
    }

    /// 保存済み Cookie を `Set-Cookie` で更新できるようにする (native と同じ挙動)。
    pub fn with_cookie_store(mut self, store: Arc<dyn CookieStore>) -> Self {
        self.cookie_store = Some(store);
        self
    }


    /// Execute one hop over `exchange`: a raw socket GET, or one relay call
    /// whose JSON response is turned back into an [`HttpResponse`].
    async fn exchange_hop(
        &self,
        exchange: &HopExchange<'_>,
        url: &str,
        headers: &[(String, String)],
        deadline_ms: f64,
    ) -> Result<HttpResponse> {
        // `connect()` もサブリクエスト枠を消費するので fetch と同じく計上する。
        self.subrequests.record();
        match exchange {
            HopExchange::Socket => {
                let target = SocketTarget::parse(url)?;
                socket_exchange(&target, HttpMethod::Get, headers, None, deadline_ms, MAX_WORKER_HTTP_BODY).await
            }
            HopExchange::Relay(relay) => {
                let relay_request =
                    RelayRequest::new(&relay.base_url, &relay.token, url, headers)?.into_request();
                let target = SocketTarget::parse(&relay_request.url)?;
                let wire_response = socket_exchange(
                    &target, relay_request.method, &relay_request.headers,
                    relay_request.body.as_deref().map(Vec::as_slice),
                    deadline_ms, response_wire_limit(MAX_WORKER_HTTP_BODY),
                ).await?;
                let relay_response = RelayResponse::parse(
                    wire_response.status, &wire_response.body, MAX_WORKER_HTTP_BODY,
                )?;
                let mut response_headers = Vec::new();
                if let Some(content_type) = relay_response.content_type {
                    response_headers.push(("content-type".to_string(), content_type));
                }
                if let Some(location) = relay_response.location {
                    response_headers.push(("location".to_string(), location));
                }
                for cookie in relay_response.set_cookie {
                    response_headers.push(("set-cookie".to_string(), cookie));
                }
                Ok(HttpResponse {
                    status: relay_response.status,
                    headers: response_headers,
                    body: relay_response.body,
                })
            }
        }
    }

    async fn send_fetch_hop(&self, request: &HttpRequest) -> Result<HttpResponse> {
        let method = match request.method {
            HttpMethod::Get => Method::Get,
            HttpMethod::Post => Method::Post,
            HttpMethod::Put => Method::Put,
            HttpMethod::Delete => Method::Delete,
        };
        let headers = Headers::new();
        for (name, value) in &request.headers {
            headers
                .append(name, value)
                .map_err(|error| NarouError::Platform(format!("invalid HTTP header: {error}")))?;
        }
        let mut init = RequestInit::new();
        init.with_method(method);
        init.with_redirect(RequestRedirect::Manual);
        init.with_headers(headers);
        if let Some(body) = request.body.as_deref() {
            if body.len() > MAX_WORKER_HTTP_BODY {
                return Err(NarouError::Platform(
                    "Worker HTTP request body exceeds limit".to_string(),
                ));
            }
            init.with_body(Some(JsValue::from(js_sys::Uint8Array::from(body.as_slice()))));
        }
        let trusted_limit = request.trusted_endpoint.then_some(request.trusted_response_body_limit).flatten();
        let request = Request::new_with_init(&request.url, &init)
            .map_err(|error| NarouError::Platform(format!("invalid HTTP request: {error}")))?;
        self.subrequests.record();
        let response = Fetch::Request(request)
            .send()
            .await
            .map_err(|error| NarouError::Http(error.to_string()))?;
        let status = response.status_code();
        let mut headers = response.headers().entries()
            .filter(|(name, _)| !name.eq_ignore_ascii_case("set-cookie"))
            .collect::<Vec<_>>();
        for cookie in response.headers().get_all("set-cookie")
            .map_err(|error| NarouError::Http(error.to_string()))?
        {
            headers.push(("set-cookie".to_string(), cookie));
        }
        let limit =
            narou_rs::platform::http::response_body_limit(status, trusted_limit, MAX_WORKER_HTTP_BODY);
        if response
            .headers()
            .get("content-length")
            .map_err(|error| {
                NarouError::Platform(format!("invalid HTTP response headers: {error}"))
            })?
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|size| size > limit)
        {
            if let worker::ResponseBody::Stream(stream) = response.into_parts().1 {
                let _ = with_timeout(
                    JsFuture::from(stream.cancel()).map(|result| result.map(|_| ()).map_err(js_error)),
                    5_000.0, "Fetch body cancel",
                ).await;
            }
            return Err(NarouError::Platform(
                "Worker HTTP response body exceeds limit".to_string(),
            ));
        }
        let body = read_fetch_body(response.into_parts().1, limit).await?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
    fn send_inner<'a>(
        &'a self,
        mut request: HttpRequest,
        pacing: Option<narou_rs::platform::http::HttpPacing<'a>>,
    ) -> PlatformFuture<'a, Result<HttpResponse>> {
        if !self.user_agent.is_empty() && request.header("User-Agent").is_none() {
            request.headers.push(("User-Agent".to_string(), self.user_agent.clone()));
        }
        Box::pin(async move {
            let mut chain = RedirectChain::new(request, MAX_REDIRECTS);
            loop {
                let request = chain.request();
                validate_http_url_syntax(&request.url)?;
                if !request.trusted_endpoint {
                    self.validate_url(&request.url).await?;
                }
                let scope = pacing.map(|policy| policy.scope_for(&chain)).transpose()?;
                if let (Some(policy), Some(scope)) = (pacing, scope.as_ref()) {
                    policy.limiter.acquire(scope).await?;
                }
                // Fetch is always manual. A partial/failed Fetch body returns
                // an error above; only a complete 403 can start fallback.
                let fetch_response = self.send_fetch_hop(request).await?;
                let response = if fetch_response.status == 403
                    && request.method == HttpMethod::Get
                    && !request.trusted_endpoint
                {
                    if let (Some(policy), Some(scope)) = (pacing, scope.as_ref()) {
                        policy.limiter.acquire(scope).await?;
                    }
                    let deadline_ms = js_sys::Date::now() + TOTAL_TIMEOUT_SECS as f64 * 1_000.0;
                    match self.exchange_hop(
                        &HopExchange::Socket, &request.url, &request.headers, deadline_ms,
                    ).await {
                        Ok(response) if response.is_success() || response.is_redirection() => response,
                        socket_result => {
                            let relay_result = match self.relay.as_ref() {
                                Some(relay) => {
                                    if let (Some(policy), Some(scope)) = (pacing, scope.as_ref()) {
                                        policy.limiter.acquire(scope).await?;
                                    }
                                    let deadline_ms = js_sys::Date::now() + TOTAL_TIMEOUT_SECS as f64 * 1_000.0;
                                    self.exchange_hop(
                                        &HopExchange::Relay(relay), &request.url,
                                        &request.headers, deadline_ms,
                                    ).await.map(Some)
                                }
                                None => Ok(None),
                            };
                            match relay_result {
                                Ok(Some(response))
                                    if response.is_success() || response.is_redirection() => response,
                                relay_result => {
                                    match socket_result {
                                        Ok(response) => console_warn!(
                                            "socket transport returned {}, keeping fetch 403", response.status
                                        ),
                                        Err(error) => console_warn!(
                                            "socket transport failed, keeping fetch 403: {error}"
                                        ),
                                    }
                                    match relay_result {
                                        Ok(Some(response)) => console_warn!(
                                            "relay transport returned {}, keeping fetch 403", response.status
                                        ),
                                        Ok(None) => {}
                                        Err(error) => console_warn!(
                                            "relay transport failed, keeping fetch 403: {error}"
                                        ),
                                    }
                                    fetch_response
                                }
                            }
                        }
                    }
                } else {
                    fetch_response
                };
                if let Some(store) = self.cookie_store.as_deref() {
                    narou_rs::platform::cookie_store::persist_response_set_cookie(
                        store, &response, chain.request().header("Cookie"),
                    ).await;
                }
                if !chain.advance(&response)? {
                    return Ok(response);
                }
            }
        })
    }
}

impl HttpClient for WorkerHttpClient {
    /// wasm には DNS 解決手段が無いので、構文とアドレスリテラルの判定だけを
    /// 行う (到達可否は Cloudflare の egress 側の制約に委ねる)。ホスト名が
    /// 非公開アドレスに解決される場合を弾けない点が native との差。
    fn validate_url<'a>(&'a self, url: &'a str) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            narou_rs::platform::url_policy::validate_url_syntax(url)
                .map_err(narou_rs::error::NarouError::Http)
        })
    }

    fn send<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<HttpResponse>> {
        self.send_inner(request, None)
    }

    fn send_paced<'a>(
        &'a self,
        request: HttpRequest,
        pacing: narou_rs::platform::http::HttpPacing<'a>,
    ) -> PlatformFuture<'a, Result<HttpResponse>> {
        self.send_inner(request, Some(pacing))
    }

}

/// Consume the real stream, cancelling it on size/read failure. Using the
/// reader directly keeps an aborted partial body from becoming successful EOF.
async fn read_fetch_body(body: worker::ResponseBody, limit: usize) -> Result<Vec<u8>> {
    match body {
        worker::ResponseBody::Empty => Ok(Vec::new()),
        worker::ResponseBody::Body(body) if body.len() <= limit => Ok(body),
        worker::ResponseBody::Body(_) => Err(NarouError::Platform(
            "Worker HTTP response body exceeds limit".to_string(),
        )),
        worker::ResponseBody::Stream(stream) => {
            let reader = stream.get_reader()
                .dyn_into::<ReadableStreamDefaultReader>()
                .map_err(|_| NarouError::Http("invalid Fetch response reader".into()))?;
            let result = async {
                let mut body = Vec::new();
                loop {
                    let value = JsFuture::from(reader.read()).await
                        .map_err(|error| NarouError::Http(format!("Fetch body read failed: {error:?}")))?;
                    if js_sys::Reflect::get(&value, &JsValue::from_str("done"))
                        .map_err(js_error)?.is_truthy()
                    {
                        return Ok(body);
                    }
                    let chunk = js_sys::Reflect::get(&value, &JsValue::from_str("value"))
                        .map_err(js_error)?.unchecked_into::<js_sys::Uint8Array>();
                    let length = chunk.length() as usize;
                    if length > limit.saturating_sub(body.len()) {
                        return Err(NarouError::Platform(
                            "Worker HTTP response body exceeds limit".to_string(),
                        ));
                    }
                    let start = body.len();
                    body.resize(start + length, 0);
                    chunk.copy_to(&mut body[start..]);
                }
            }.await;
            if result.is_err() {
                let _ = with_timeout(
                    JsFuture::from(reader.cancel()).map(|result| result.map(|_| ()).map_err(js_error)),
                    5_000.0, "Fetch body cancel",
                ).await;
            }
            reader.release_lock();
            result
        }
    }
}

/// A `connect()` TCP/TLS socket driven directly through its web streams.
///
/// `worker::Socket` implements tokio's AsyncRead/AsyncWrite, which this crate
/// does not depend on, so the readable/writable streams are consumed
/// manually. The writer is deliberately **never** closed: on Workers,
/// closing the writable side first also tears down the readable side and the
/// response arrives as zero bytes (measured, not documented).
struct RawSocket {
    inner: worker_sys::Socket,
    writer: WritableStreamDefaultWriter,
    reader: ReadableStreamDefaultReader,
}

/// Milliseconds remaining until `deadline_ms` (`Date.now()` clock), or an
/// error once the total budget is spent.
fn deadline_left(deadline_ms: f64, what: &str) -> Result<f64> {
    let left = deadline_ms - js_sys::Date::now();
    if left <= 0.0 {
        return Err(NarouError::Http(format!("socket {what}: total timeout")));
    }
    Ok(left)
}

/// `fut` with a deadline, whichever fires first. The loser is dropped — for
/// `read()`/`write()` that abandons the pending promise, which the socket's
/// `close()` then settles.
async fn with_timeout<T>(fut: impl Future<Output = Result<T>>, ms: f64, what: &str) -> Result<T> {
    match future::select(
        pin!(fut),
        pin!(worker::Delay::from(Duration::from_millis(
            ms.max(1.0) as u64
        ))),
    )
    .await
    {
        Either::Left((result, _delay)) => result,
        Either::Right(((), _pending)) => Err(NarouError::Http(format!("socket {what} timed out"))),
    }
}

fn js_error(value: JsValue) -> NarouError {
    NarouError::Http(match value.as_string() {
        Some(message) => format!("socket I/O: {message}"),
        None => format!("socket I/O: {value:?}"),
    })
}

/// Open the socket: `connect()` returns synchronously, `opened()` resolves
/// once TLS negotiation (when requested) and the TCP handshake complete.
async fn connect_socket(target: &SocketTarget, deadline_ms: f64) -> Result<RawSocket> {
    let address = js_sys::Object::new();
    js_sys::Reflect::set(
        &address,
        &JsValue::from_str("hostname"),
        &JsValue::from_str(&target.host),
    )
    .map_err(js_error)?;
    js_sys::Reflect::set(
        &address,
        &JsValue::from_str("port"),
        &JsValue::from_f64(f64::from(target.port)),
    )
    .map_err(js_error)?;
    let options = js_sys::Object::new();
    js_sys::Reflect::set(
        &options,
        &JsValue::from_str("secureTransport"),
        &JsValue::from_str(if target.tls { "on" } else { "off" }),
    )
    .map_err(js_error)?;
    // We never half-close; keep the flag explicit anyway.
    js_sys::Reflect::set(
        &options,
        &JsValue::from_str("allowHalfOpen"),
        &JsValue::TRUE,
    )
    .map_err(js_error)?;

    let inner = worker_sys::connect(address.into(), options.into()).map_err(js_error)?;
    let left = deadline_left(deadline_ms, "connect")?;
    with_timeout(
        JsFuture::from(inner.opened().map_err(js_error)?)
            .map(|result| result.map(|_| ()).map_err(js_error)),
        left.min(CONNECT_TIMEOUT_SECS as f64 * 1_000.0),
        "connect",
    )
    .await?;

    let readable = inner.readable().map_err(js_error)?;
    let reader: ReadableStreamDefaultReader = readable
        .get_reader()
        .dyn_into()
        .map_err(|value| NarouError::Http(format!("socket reader unavailable: {value:?}")))?;
    let writable = inner.writable().map_err(js_error)?;
    let writer = WritableStreamDefaultWriter::new(&writable).map_err(js_error)?;
    Ok(RawSocket {
        inner,
        writer,
        reader,
    })
}

impl RawSocket {
    /// One atomic write — request heads are a few hundred bytes, far below
    /// the stream's internal queue, so a single `write()` suffices.
    async fn write_all(&self, bytes: &[u8], deadline_ms: f64) -> Result<()> {
        let left = deadline_left(deadline_ms, "write")?;
        with_timeout(
            JsFuture::from(
                self.writer
                    .write_with_chunk(&JsValue::from(js_sys::Uint8Array::from(bytes))),
            )
            .map(|result| result.map(|_| ()).map_err(js_error)),
            left.min(READ_TIMEOUT_SECS as f64 * 1_000.0),
            "write",
        )
        .await
    }

    /// Next chunk of response bytes; `None` at EOF.
    async fn read_chunk(&mut self, deadline_ms: f64) -> Result<Option<js_sys::Uint8Array>> {
        let left = deadline_left(deadline_ms, "read")?;
        let value = with_timeout(
            JsFuture::from(self.reader.read()).map(|result| result.map_err(js_error)),
            left.min(READ_TIMEOUT_SECS as f64 * 1_000.0),
            "read",
        )
        .await?;
        let done = js_sys::Reflect::get(&value, &JsValue::from_str("done"))
            .map_err(js_error)?
            .is_truthy();
        if done {
            return Ok(None);
        }
        let chunk = js_sys::Reflect::get(&value, &JsValue::from_str("value"))
            .map_err(js_error)?
            .unchecked_into::<js_sys::Uint8Array>();
        Ok(Some(chunk))
    }

    /// Release stream locks and close the socket. Best-effort: a stuck close
    /// must not hang the request (the runtime reclaims the socket anyway).
    async fn close(self) {
        self.reader.release_lock();
        self.writer.release_lock();
        if let Ok(promise) = self.inner.close() {
            let _ = with_timeout(
                JsFuture::from(promise).map(|result| result.map(|_| ()).map_err(js_error)),
                5_000.0,
                "close",
            )
            .await;
        }
    }
}

/// One socket round-trip: connect → write request head → parse the response.
async fn socket_exchange(
    target: &SocketTarget,
    method: HttpMethod,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    deadline_ms: f64,
    body_limit: usize,
) -> Result<HttpResponse> {
    let mut socket = connect_socket(target, deadline_ms).await?;
    let result = socket_roundtrip(&mut socket, target, method, headers, body, deadline_ms, body_limit).await;
    socket.close().await;
    result
}

async fn socket_roundtrip(
    socket: &mut RawSocket,
    target: &SocketTarget,
    method: HttpMethod,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    deadline_ms: f64,
    body_limit: usize,
) -> Result<HttpResponse> {
    let request = encode_request(target, method, headers, body.map(<[u8]>::len));
    socket.write_all(&request, deadline_ms).await?;
    if let Some(body) = body {
        socket.write_all(body, deadline_ms).await?;
    }
    let mut decoder = Http1ResponseDecoder::new(body_limit);
    // Stage only a bounded slice in wasm memory. A coalesced oversized JS
    // chunk must reach the decoder's checks before a whole-chunk allocation.
    let mut buffer = [0u8; 8192];
    loop {
        match socket.read_chunk(deadline_ms).await? {
            Some(chunk) => {
                let mut offset = 0u32;
                let chunk_len = chunk.length();
                while offset < chunk_len {
                    let end = offset.saturating_add(buffer.len() as u32).min(chunk_len);
                    let length = (end - offset) as usize;
                    chunk.subarray(offset, end).copy_to(&mut buffer[..length]);
                    if decoder.feed(&buffer[..length])? {
                        return decoder.into_response();
                    }
                    offset = end;
                }
            }
            None => return decoder.finish(),
        }
    }
}


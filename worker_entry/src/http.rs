use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures::future::{self, Either};
use narou_rs::downloader::http_policy::same_site_hosts;
use narou_rs::downloader::security::{
    CONNECT_TIMEOUT_SECS, MAX_REDIRECTS, READ_TIMEOUT_SECS, TOTAL_TIMEOUT_SECS,
};
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::http1::{Http1ResponseDecoder, SocketTarget, encode_request};
use narou_rs::platform::relay::{RelayRequest, RelayResponse, TOKEN_HEADER};
use narou_rs::platform::{
    CookieStore, HttpClient, HttpMethod, HttpRequest, HttpResponse, PlatformFuture, RedirectMode,
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

    /// 送信した Cookie に対応する資格情報だけを `Set-Cookie` で更新する。
    ///
    /// サイトは複数のログイン情報を持てるので、送っていない資格情報を書き換えては
    /// いけない (別アカウントのセッションを壊す)。保存値は暗号化されて書き戻る。
    async fn persist_set_cookie(
        &self,
        url: &str,
        response: &HttpResponse,
        sent_cookie: Option<&str>,
    ) {
        let Some(store) = self.cookie_store.as_ref() else {
            return;
        };
        let values: Vec<String> = response
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
            .map(|(_, value)| value.clone())
            .collect();
        if values.is_empty() {
            return;
        }
        let Some(host) = narou_rs::platform::cookie_host_for_url(url) else {
            return;
        };
        let Some(sent) = sent_cookie else {
            return;
        };
        let sent_pairs = narou_rs::platform::parse_cookie_header(sent);
        if sent_pairs.is_empty() {
            return;
        }
        let Ok(all) = store.list().await else {
            return;
        };
        for key in narou_rs::platform::cookie_lookup_hosts(&host) {
            let Some(credentials) = all.get(&key) else {
                continue;
            };
            let mut updated = credentials.clone();
            let mut changed = false;
            for credential in updated.iter_mut() {
                if !narou_rs::platform::credential_was_sent(&credential.cookie, &sent_pairs) {
                    continue;
                }
                let cookie = narou_rs::platform::apply_set_cookie(&credential.cookie, &values);
                if cookie != credential.cookie {
                    credential.cookie = cookie;
                    changed = true;
                }
            }
            if changed {
                let _ = store.save_all(&key, &updated).await;
            }
        }
    }

    /// GET over a raw `connect()` socket, following the request's redirect
    /// policy. Returns `Err` whenever the socket path cannot produce a
    /// trustworthy response (connection refused by the runtime, timeout,
    /// truncated body, malformed head) so the caller keeps the fetch result.
    async fn send_via_socket(&self, request: &HttpRequest) -> Result<HttpResponse> {
        let mut target = SocketTarget::parse(&request.url)?;
        // RedirectMode::Follow は fetch 経路と揃えてソケット側でも辿る。
        // native と同じく、サイトの外に出る hop では認証系ヘッダを落とす
        // (fetch は Cookie を勝手に付けないので寄せる意味もある)。
        let mut headers = request.headers.clone();
        // ソケット経路でも UA 無しは送らない (fetch 経路と同じフォールバック)。
        // カクヨムの CloudFront は User-Agent なしの接続を 403 で弾くため、
        // これが無いと生ソケットでも同じ 403 が返りフォールバックが無意味になる
        // (一時アカウントで実測: UA 無し→CloudFront 403、UA 有り→200)。
        if !self.user_agent.is_empty()
            && !headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
        {
            headers.push(("User-Agent".to_string(), self.user_agent.clone()));
        }
        let deadline_ms = js_sys::Date::now() + TOTAL_TIMEOUT_SECS as f64 * 1_000.0;
        for hop in 0..=MAX_REDIRECTS {
            // `connect()` もサブリクエスト枠を消費するので fetch と同じく計上する。
            self.subrequests.record();
            let response = socket_exchange(&target, &headers, deadline_ms).await?;
            if request.redirect != RedirectMode::Follow || !response.is_redirection() {
                return Ok(response);
            }
            let Some(location) = response.header("Location").map(str::to_string) else {
                return Ok(response);
            };
            let next = target.join(&location)?;
            if !same_site_hosts(Some(&next.host), Some(&target.host)) {
                headers.retain(|(name, _)| {
                    !name.eq_ignore_ascii_case("cookie")
                        && !name.eq_ignore_ascii_case("authorization")
                });
            }
            if hop == MAX_REDIRECTS {
                return Err(NarouError::Http(
                    "redirect limit exceeded on socket transport".to_string(),
                ));
            }
            target = next;
        }
        unreachable!("redirect loop returns within MAX_REDIRECTS")
    }

    /// GET via the SORAHOST relay, reached over a plain-http `connect()`
    /// socket (`RelayRequest` keeps the full site URL and headers inside the
    /// query, so the wire request is a GET to the relay's `/proxy`).
    ///
    /// Returns `Ok(None)` when the relay is not configured. `Err` covers every
    /// way the relay path cannot produce a trustworthy response (bad config,
    /// connect/timeout/malformed head, `RelayError`) so the caller keeps the
    /// fetch result — same policy as [`Self::send_via_socket`].
    ///
    /// リダイレクトは踏み台側が辿る (`redirect` パラメータ)。`Manual` のときは
    /// 上流の 3xx が `status`/`location` に載って返り、そのまま採用する。
    async fn send_via_relay(&self, request: &HttpRequest) -> Result<Option<HttpResponse>> {
        let Some(relay) = self.relay.as_ref() else {
            return Ok(None);
        };
        // サイト定義のヘッダはそのまま転送する。UA が無いときは socket 経路と
        // 同じフォールバックを足す (UA 無しは上流で 403 になりやすい)。
        let mut headers = request.headers.clone();
        if !self.user_agent.is_empty()
            && !headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
        {
            headers.push(("User-Agent".to_string(), self.user_agent.clone()));
        }
        let relay_request = RelayRequest::new(
            &relay.base_url,
            &relay.token,
            &request.url,
            &headers,
            request.redirect,
        )?;
        let target = SocketTarget::parse(relay_request.url())?;
        // 踏み台への認証はトランスポート層のヘッダで送る (クエリには載せない)。
        let wire_headers = vec![(TOKEN_HEADER.to_string(), relay_request.token().to_string())];
        let deadline_ms = js_sys::Date::now() + TOTAL_TIMEOUT_SECS as f64 * 1_000.0;
        // `connect()` もサブリクエスト枠を消費するので fetch と同じく計上する。
        self.subrequests.record();
        let wire_response = socket_exchange(&target, &wire_headers, deadline_ms).await?;
        let relay_response = RelayResponse::parse(wire_response.status, &wire_response.body)?;
        let mut response_headers = Vec::new();
        if let Some(content_type) = relay_response.content_type {
            response_headers.push(("content-type".to_string(), content_type));
        }
        if let Some(location) = relay_response.location {
            response_headers.push(("location".to_string(), location));
        }
        Ok(Some(HttpResponse {
            status: relay_response.status,
            headers: response_headers,
            body: relay_response.body,
        }))
    }

    async fn send_request(&self, request: &HttpRequest) -> Result<HttpResponse> {
        let method = match request.method {
            HttpMethod::Get => Method::Get,
            HttpMethod::Post => Method::Post,
        };
        let redirect = match request.redirect {
            RedirectMode::Follow => RequestRedirect::Follow,
            RedirectMode::Manual => RequestRedirect::Manual,
        };
        let headers = Headers::new();
        for (name, value) in &request.headers {
            headers
                .append(name, value)
                .map_err(|error| NarouError::Platform(format!("invalid HTTP header: {error}")))?;
        }
        // UA が無いリクエストは多くのサイトで 403 になるので、必ず 1 つ送る。
        if !self.user_agent.is_empty()
            && !request
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
        {
            headers
                .append("user-agent", &self.user_agent)
                .map_err(|error| NarouError::Platform(format!("invalid HTTP header: {error}")))?;
        }
        let mut init = RequestInit::new();
        init.with_method(method);
        init.with_redirect(redirect);
        init.with_headers(headers);
        if let Some(body) = request.body.as_deref() {
            if body.len() > MAX_WORKER_HTTP_BODY {
                return Err(NarouError::Platform(
                    "Worker HTTP request body exceeds limit".to_string(),
                ));
            }
            init.with_body(Some(JsValue::from(js_sys::Uint8Array::from(body))));
        }
        let request = Request::new_with_init(&request.url, &init)
            .map_err(|error| NarouError::Platform(format!("invalid HTTP request: {error}")))?;
        self.subrequests.record();
        let mut response = Fetch::Request(request)
            .send()
            .await
            .map_err(|error| NarouError::Http(error.to_string()))?;
        let status = response.status_code();
        let headers = response.headers().entries().collect::<Vec<_>>();
        if response
            .headers()
            .get("content-length")
            .map_err(|error| {
                NarouError::Platform(format!("invalid HTTP response headers: {error}"))
            })?
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|size| size > MAX_WORKER_HTTP_BODY)
        {
            return Err(NarouError::Platform(
                "Worker HTTP response body exceeds limit".to_string(),
            ));
        }
        let body = response
            .bytes()
            .await
            .map_err(|error| NarouError::Http(error.to_string()))?;
        if body.len() > MAX_WORKER_HTTP_BODY {
            return Err(NarouError::Platform(
                "Worker HTTP response body exceeds limit".to_string(),
            ));
        }
        Ok(HttpResponse {
            status,
            headers,
            body,
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
        let url = request.url.clone();
        let sent_cookie = request.header("Cookie").map(str::to_string);
        Box::pin(async move {
            let fetch_response = self.send_request(&request).await?;
            // fetch は Cloudflare 配下のサイトでエッジから 403 が返ることが
            // ある (`cf-mitigated`)。`connect()` は fetch とは別の egress
            // プレフィックスを使うため、403 のときだけ生ソケットで取り直す。
            // それでも 2xx/3xx が得られないときは SORAHOST リレー (設定済みの
            // 場合のみ) で最後に取り直す。GET のみ (encoder が GET しか
            // 組み立てない)。
            let response = if fetch_response.status == 403 && request.method == HttpMethod::Get {
                match self.send_via_socket(&request).await {
                    // 通常の応答が取れたときだけ採用する。
                    Ok(socket_response)
                        if socket_response.is_success() || socket_response.is_redirection() =>
                    {
                        socket_response
                    }
                    socket_result => {
                        // 第三段: 踏み台経由 (設定済みの場合のみ)。こちらも
                        // 2xx/3xx が取れたときだけ採用する。
                        match self.send_via_relay(&request).await {
                            Ok(Some(relay_response))
                                if relay_response.is_success()
                                    || relay_response.is_redirection() =>
                            {
                                relay_response
                            }
                            relay_result => {
                                // どの経路でも採用できないので fetch の 403 を
                                // そのまま返す (挙動を変えない)。
                                match socket_result {
                                    Ok(socket_response) => console_warn!(
                                        "socket transport returned {}, keeping fetch 403",
                                        socket_response.status
                                    ),
                                    Err(error) => console_warn!(
                                        "socket transport failed, keeping fetch 403: {error}"
                                    ),
                                }
                                match relay_result {
                                    Ok(Some(relay_response)) => console_warn!(
                                        "relay transport returned {}, keeping fetch 403",
                                        relay_response.status
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
            self.persist_set_cookie(&url, &response, sent_cookie.as_deref())
                .await;
            Ok(response)
        })
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
    async fn read_chunk(&mut self, deadline_ms: f64) -> Result<Option<Vec<u8>>> {
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
        Ok(Some(chunk.to_vec()))
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
    headers: &[(String, String)],
    deadline_ms: f64,
) -> Result<HttpResponse> {
    let mut socket = connect_socket(target, deadline_ms).await?;
    let result = socket_roundtrip(&mut socket, target, headers, deadline_ms).await;
    socket.close().await;
    result
}

async fn socket_roundtrip(
    socket: &mut RawSocket,
    target: &SocketTarget,
    headers: &[(String, String)],
    deadline_ms: f64,
) -> Result<HttpResponse> {
    let request = encode_request(target, headers);
    socket.write_all(&request, deadline_ms).await?;
    let mut decoder = Http1ResponseDecoder::new(MAX_WORKER_HTTP_BODY);
    loop {
        match socket.read_chunk(deadline_ms).await? {
            Some(chunk) => {
                if decoder.feed(&chunk)? {
                    return decoder.into_response();
                }
            }
            None => return decoder.finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_limit_is_explicit_and_shared() {
        assert_eq!(MAX_WORKER_HTTP_BODY, 16 * 1024 * 1024);
    }
}

//! Owned Fetch/socket streams; reads follow downstream demand and drop cancels I/O.

use super::*;
use futures::StreamExt;
use narou_rs::platform::{AssetStream, http::{HttpStreamResponse, HttpPacing}, staging::{ASSET_CHUNK_BYTES, owned_bytes_stream}};

struct FetchReader {
    reader: ReadableStreamDefaultReader,
    pending: Option<js_sys::Uint8Array>,
    offset: u32,
    finished: bool,
}

impl Drop for FetchReader {
    fn drop(&mut self) {
        if !self.finished {
            let promise = self.reader.cancel();
            wasm_bindgen_futures::spawn_local(async move { let _ = JsFuture::from(promise).await; });
        }
        self.reader.release_lock();
    }
}

fn fetch_stream(body: worker::ResponseBody, limit: Option<u64>, expected: Option<u64>) -> Result<AssetStream> {
    let worker::ResponseBody::Stream(stream) = body else {
        return match body {
            worker::ResponseBody::Empty if expected.unwrap_or(0) == 0 => Ok(Box::pin(futures::stream::empty())),
            worker::ResponseBody::Empty => Err(NarouError::Http("truncated Fetch body".into())),
            worker::ResponseBody::Body(bytes) => {
                if limit.is_some_and(|limit| bytes.len() as u64 > limit)
                    || expected.is_some_and(|length| bytes.len() as u64 != length) {
                    return Err(NarouError::Http("invalid Fetch body length".into()));
                }
                Ok(owned_bytes_stream(bytes))
            }
            worker::ResponseBody::Stream(_) => unreachable!(),
        };
    };
    let reader = stream.get_reader().dyn_into::<ReadableStreamDefaultReader>()
        .map_err(|_| NarouError::Http("invalid Fetch stream reader".into()))?;
    let reader = FetchReader { reader, pending: None, offset: 0, finished: false };
    Ok(Box::pin(futures::stream::try_unfold((reader, 0u64), move |(mut state, total)| async move {
        loop {
            if let Some(chunk) = state.pending.as_ref() {
                if state.offset < chunk.length() {
                    let end = state.offset.saturating_add(ASSET_CHUNK_BYTES as u32).min(chunk.length());
                    let length = (end - state.offset) as usize;
                    let next = total.checked_add(length as u64).ok_or_else(|| NarouError::Http("Fetch body length overflow".into()))?;
                    if limit.is_some_and(|limit| next > limit) || expected.is_some_and(|length| next > length) {
                        return Err(NarouError::Http("Fetch body exceeds declared limit".into()));
                    }
                    let mut bytes = vec![0; length];
                    chunk.subarray(state.offset, end).copy_to(&mut bytes);
                    state.offset = end;
                    if end == chunk.length() { state.pending = None; state.offset = 0; }
                    return Ok(Some((bytes, (state, next))));
                }
            }
            let value = with_timeout(JsFuture::from(state.reader.read()).map(|value| value.map_err(js_error)),
                READ_TIMEOUT_SECS as f64 * 1_000.0, "Fetch body read").await?;
            if js_sys::Reflect::get(&value, &JsValue::from_str("done")).map_err(js_error)?.is_truthy() {
                if expected.is_some_and(|length| length != total) { return Err(NarouError::Http("truncated Fetch body".into())); }
                state.finished = true;
                drop(state);
                return Ok(None);
            }
            state.pending = Some(js_sys::Reflect::get(&value, &JsValue::from_str("value")).map_err(js_error)?
                .dyn_into::<js_sys::Uint8Array>().map_err(|_| NarouError::Http("invalid Fetch chunk".into()))?);
            state.offset = 0;
        }
    })))
}

impl WorkerHttpClient {
    async fn send_fetch_stream_hop(&self, request: &HttpRequest) -> Result<HttpStreamResponse> {
        let method = match request.method { HttpMethod::Get => Method::Get, HttpMethod::Post => Method::Post,
            HttpMethod::Put => Method::Put, HttpMethod::Delete => Method::Delete };
        let headers = Headers::new();
        for (name, value) in &request.headers {
            headers.append(name, value).map_err(|error| NarouError::Http(error.to_string()))?;
        }
        let mut init = RequestInit::new();
        init.with_method(method).with_redirect(RequestRedirect::Manual).with_headers(headers);
        if let Some(bytes) = request.body.as_deref() {
            if bytes.len() > MAX_WORKER_HTTP_BODY { return Err(NarouError::Http("Worker request body exceeds limit".into())); }
            init.with_body(Some(JsValue::from(js_sys::Uint8Array::from(bytes.as_slice()))));
        }
        let fetch = Request::new_with_init(&request.url, &init).map_err(|error| NarouError::Http(error.to_string()))?;
        self.subrequests.record();
        let response = Fetch::Request(fetch).send().await.map_err(|error| NarouError::Http(error.to_string()))?;
        let status = response.status_code();
        let mut headers = response.headers().entries().filter(|(name, _)| !name.eq_ignore_ascii_case("set-cookie")).collect::<Vec<_>>();
        for cookie in response.headers().get_all("set-cookie").map_err(|error| NarouError::Http(error.to_string()))? {
            headers.push(("set-cookie".into(), cookie));
        }
        let encoded = headers.iter().any(|(name, value)| name.eq_ignore_ascii_case("content-encoding") && !value.eq_ignore_ascii_case("identity"));
        let expected = if encoded { None } else {
            headers.iter().find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.parse::<u64>()).transpose().map_err(|_| NarouError::Http("invalid Content-Length".into()))?
        };
        let limit = if request.trusted_endpoint && (200..300).contains(&status) {
            request.trusted_response_body_limit.map(|value| value as u64)
        } else if !(200..300).contains(&status) { Some(MAX_WORKER_HTTP_BODY as u64) } else { None };
        let body = fetch_stream(response.into_parts().1, limit, expected)?;
        Ok(HttpStreamResponse { status, url: request.url.clone(), headers, body })
    }

    pub(super) fn send_stream_inner<'a>(&'a self, mut request: HttpRequest, pacing: Option<HttpPacing<'a>>) -> PlatformFuture<'a, Result<HttpStreamResponse>> {
        if !self.user_agent.is_empty() && request.header("User-Agent").is_none() {
            request.headers.push(("User-Agent".into(), self.user_agent.clone()));
        }
        Box::pin(async move {
            let mut chain = RedirectChain::new(request, MAX_REDIRECTS);
            loop {
                let request = chain.request();
                validate_http_url_syntax(&request.url)?;
                if !request.trusted_endpoint { self.validate_url(&request.url).await?; }
                let scope = pacing.map(|policy| policy.scope_for(&chain)).transpose()?;
                if let (Some(policy), Some(scope)) = (pacing, scope.as_ref()) { policy.limiter.acquire(scope).await?; }
                let result = self.send_fetch_stream_hop(request).await;
                if let (Some(policy), Some(scope)) = (pacing, scope.as_ref()) { policy.limiter.complete(scope).await?; }
                let mut response = result?;
                if response.status == 403 && request.method == HttpMethod::Get && !request.trusted_endpoint {
                    // Preserve the original complete diagnostic response if all
                    // alternatives fail; a truncated 403 never starts fallback.
                    let mut diagnostic = Vec::new();
                    while let Some(chunk) = response.body.next().await {
                        let chunk = chunk?;
                        if chunk.len() > MAX_WORKER_HTTP_BODY - diagnostic.len() { return Err(NarouError::Http("403 body exceeds limit".into())); }
                        diagnostic.extend_from_slice(&chunk);
                    }
                    response.body = owned_bytes_stream(diagnostic);
                    if let (Some(policy), Some(scope)) = (pacing, scope.as_ref()) { policy.limiter.acquire(scope).await?; }
                    self.subrequests.record();
                    let socket = socket_stream(&request.url, &request.headers).await;
                    if let (Some(policy), Some(scope)) = (pacing, scope.as_ref()) { policy.limiter.complete(scope).await?; }
                    match socket {
                        Ok(value) if value.is_success() || value.is_redirection() => response = value,
                        _ => {
                            if let Some(relay) = self.relay.as_ref() {
                                if let (Some(policy), Some(scope)) = (pacing, scope.as_ref()) { policy.limiter.acquire(scope).await?; }
                                let mut relay_request = RelayRequest::new(&relay.base_url, &relay.token, &request.url, &request.headers)?.into_stream_request();
                                if relay_request.header("User-Agent").is_none() {
                                    let user_agent = request.header("User-Agent").unwrap_or(
                                        "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:158.0) Gecko/20100101 Firefox/158.0");
                                    relay_request.headers.push(("User-Agent".into(), user_agent.to_string()));
                                }
                                let relay_response = match self.send_fetch_stream_hop(&relay_request).await {
                                    Ok(response) => narou_rs::platform::relay::decode_relay_stream(response).await,
                                    Err(error) => Err(error),
                                };
                                if let (Some(policy), Some(scope)) = (pacing, scope.as_ref()) { policy.limiter.complete(scope).await?; }
                                if let Ok(mut value) = relay_response {
                                    value.url = request.url.clone();
                                    if value.is_success() || value.is_redirection() { response = value; }
                                }
                            }
                        }
                    }
                }
                let head = response.head();
                if let Some(store) = self.cookie_store.as_deref() {
                    narou_rs::platform::cookie_store::persist_response_set_cookie(store, &head, chain.request().header("Cookie")).await;
                }
                if !chain.advance(&head)? { return Ok(response); }
            }
        })
    }
}

struct SocketReader {
    socket: RawSocket,
    decoder: Http1ResponseDecoder,
    pending: Option<js_sys::Uint8Array>,
    offset: u32,
    deadline: f64,
    complete: bool,
}

impl Drop for SocketReader {
    fn drop(&mut self) {
        self.socket.reader.release_lock();
        self.socket.writer.release_lock();
        if let Ok(promise) = self.socket.inner.close() {
            wasm_bindgen_futures::spawn_local(async move { let _ = JsFuture::from(promise).await; });
        }
    }
}

impl SocketReader {
    async fn advance(&mut self) -> Result<()> {
        if self.complete { return Ok(()); }
        if self.pending.is_none() {
            self.pending = self.socket.read_chunk(self.deadline).await?;
            self.offset = 0;
            if self.pending.is_none() {
                self.decoder.finish_stream()?;
                self.complete = true;
                return Ok(());
            }
        }
        let chunk = self.pending.as_ref().expect("socket chunk");
        let end = (self.offset + ASSET_CHUNK_BYTES as u32).min(chunk.length());
        let mut bytes = vec![0; (end - self.offset) as usize];
        chunk.subarray(self.offset, end).copy_to(&mut bytes);
        self.complete = self.decoder.feed(&bytes)?;
        self.offset = end;
        if end == chunk.length() { self.pending = None; self.offset = 0; }
        Ok(())
    }
}

async fn socket_stream(url: &str, headers: &[(String, String)]) -> Result<HttpStreamResponse> {
    let target = SocketTarget::parse(url)?;
    let deadline = js_sys::Date::now() + TOTAL_TIMEOUT_SECS as f64 * 1_000.0;
    let socket = connect_socket(&target, deadline).await?;
    let mut state = SocketReader { socket, decoder: Http1ResponseDecoder::new(usize::MAX), pending: None, offset: 0, deadline, complete: false };
    state.socket.write_all(&encode_request(&target, HttpMethod::Get, headers, None), deadline).await?;
    let (status, headers) = loop {
        state.advance().await?;
        if let Some(head) = state.decoder.response_head() { break head; }
        if state.complete { return Err(NarouError::Http("socket response omitted headers".into())); }
    };
    let body: AssetStream = Box::pin(futures::stream::try_unfold(state, |mut state| async move {
        loop {
            let bytes = state.decoder.take_body();
            if !bytes.is_empty() { return Ok(Some((bytes, state))); }
            if state.complete { return Ok(None); }
            state.advance().await?;
        }
    }));
    Ok(HttpStreamResponse { status, url: url.to_string(), headers, body })
}

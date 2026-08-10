use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{HttpClient, HttpMethod, HttpRequest, HttpResponse, PlatformFuture, RedirectMode};
use wasm_bindgen::JsValue;
use worker::{Fetch, Headers, Method, Request, RequestInit, RequestRedirect};

pub const MAX_WORKER_HTTP_BODY: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default)]
pub struct WorkerHttpClient;

impl WorkerHttpClient {
    pub const fn new() -> Self { Self }

    async fn send_request(request: HttpRequest) -> Result<HttpResponse> {
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
        let mut init = RequestInit::new();
        init.with_method(method);
        init.with_redirect(redirect);
        init.with_headers(headers);
        if let Some(body) = request.body {
            if body.len() > MAX_WORKER_HTTP_BODY {
                return Err(NarouError::Platform("Worker HTTP request body exceeds limit".to_string()));
            }
            init.with_body(Some(JsValue::from(js_sys::Uint8Array::from(body.as_slice()))));
        }
        let request = Request::new_with_init(&request.url, &init)
            .map_err(|error| NarouError::Platform(format!("invalid HTTP request: {error}")))?;
        let mut response = Fetch::Request(request)
            .send()
            .await
            .map_err(|error| NarouError::Http(error.to_string()))?;
        let status = response.status_code();
        let headers = response
            .headers()
            .entries()
            .collect::<Vec<_>>();
        if response
            .headers()
            .get("content-length")
            .map_err(|error| NarouError::Platform(format!("invalid HTTP response headers: {error}")))?
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|size| size > MAX_WORKER_HTTP_BODY)
        {
            return Err(NarouError::Platform("Worker HTTP response body exceeds limit".to_string()));
        }
        let body = response
            .bytes()
            .await
            .map_err(|error| NarouError::Http(error.to_string()))?;
        if body.len() > MAX_WORKER_HTTP_BODY {
            return Err(NarouError::Platform("Worker HTTP response body exceeds limit".to_string()));
        }
        Ok(HttpResponse { status, headers, body })
    }
}

impl HttpClient for WorkerHttpClient {
    fn send<'a>(&'a self, request: HttpRequest) -> PlatformFuture<'a, Result<HttpResponse>> {
        Box::pin(Self::send_request(request))
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

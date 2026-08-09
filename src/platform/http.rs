//! Minimal HTTP transport abstraction.
//!
//! This is deliberately *not* a copy of the reqwest API. It captures only what
//! narou.rs needs: a GET or POST with headers/body, and a response with status,
//! headers, and a byte body. Redirect policy, cookies, timeouts, TLS, and
//! transport fallbacks (curl/wget) are the responsibility of the concrete
//! implementation; the domain layer must not depend on them.
//!
//! The trait is async (boxed futures, no `Send` bound on the future) so a
//! Cloudflare Workers implementation can drive it on the Workers runtime while
//! the native implementation isolates blocking transports behind
//! `tokio::task::spawn_blocking`.
//!
//! URL safety validation lives in `downloader::security` (domain layer) and is
//! applied by callers before handing a URL to an implementation.

use super::PlatformFuture;

/// HTTP method used by [`HttpRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
}

/// How redirects are handled by the transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RedirectMode {
    /// Follow redirects automatically (default; matches the current
    /// `HttpFetcher` follow-client behaviour).
    #[default]
    Follow,
    /// Do not follow redirects; return the 3xx response as-is so the caller
    /// can inspect the `Location` header (used by `resolve_final_url`).
    Manual,
}

/// A minimal HTTP request. Owned so it can be moved across threads/executors
/// (e.g. into `spawn_blocking`).
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub url: String,
    pub method: HttpMethod,
    /// Header name/value pairs, in order. Implementations may add their own
    /// headers (User-Agent etc.) but must keep these.
    pub headers: Vec<(String, String)>,
    /// POST body. `None` for GET or bodiless POST.
    pub body: Option<Vec<u8>>,
    pub redirect: RedirectMode,
}

impl HttpRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            method: HttpMethod::Get,
            headers: Vec::new(),
            body: None,
            redirect: RedirectMode::Follow,
        }
    }

    pub fn post(url: impl Into<String>, body: impl Into<Vec<u8>>) -> Self {
        Self {
            url: url.into(),
            method: HttpMethod::Post,
            headers: Vec::new(),
            body: Some(body.into()),
            redirect: RedirectMode::Follow,
        }
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn with_redirect(mut self, mode: RedirectMode) -> Self {
        self.redirect = mode;
        self
    }

    /// Value of the first header with the given (case-insensitive) name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A minimal HTTP response. The body is fully buffered bytes; character
/// decoding is the caller's responsibility (see `downloader::util`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    /// Header name/value pairs as received.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Value of the first header with the given (case-insensitive) name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn is_redirection(&self) -> bool {
        (300..400).contains(&self.status)
    }
}

/// Transport for HTTP(S) requests.
///
/// Implementations decide redirect handling, cookies, timeouts, size limits,
/// and fallback strategies. The returned future is `Send` on native (so
/// domain services can be driven on a multi-threaded executor, e.g.
/// `tokio::spawn` for parallel updates) and relaxed on `wasm32`, via
/// [`PlatformFuture`].
pub trait HttpClient: Send + Sync {
    /// Perform one request and return the full response.
    fn send<'a>(
        &'a self,
        request: HttpRequest,
    ) -> PlatformFuture<'a, crate::error::Result<HttpResponse>>;
}

/// Convenience blanket impl: any `&T` where `T: HttpClient` is itself an
/// `HttpClient` (so `&client` can be passed around without cloning).
impl<T: HttpClient + ?Sized> HttpClient for &T {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
    ) -> PlatformFuture<'a, crate::error::Result<HttpResponse>> {
        (*self).send(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_builders_set_fields() {
        let req = HttpRequest::get("https://example.com/")
            .with_header("Cookie", "a=b")
            .with_redirect(RedirectMode::Manual);
        assert_eq!(req.method, HttpMethod::Get);
        assert_eq!(req.headers, vec![("Cookie".into(), "a=b".into())]);
        assert!(req.body.is_none());
        assert_eq!(req.redirect, RedirectMode::Manual);
        assert_eq!(req.header("cookie"), Some("a=b"));

        let post = HttpRequest::post("https://example.com/api", b"payload".to_vec());
        assert_eq!(post.method, HttpMethod::Post);
        assert_eq!(post.body, Some(b"payload".to_vec()));
        assert_eq!(post.redirect, RedirectMode::Follow);
    }

    #[test]
    fn response_helpers() {
        let resp = HttpResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "text/html".into())],
            body: b"<html>hi</html>".to_vec(),
        };
        assert!(resp.is_success());
        assert!(!resp.is_redirection());
        assert_eq!(resp.header("content-type"), Some("text/html"));

        let redirect = HttpResponse {
            status: 302,
            headers: vec![("Location".into(), "https://example.com/next".into())],
            body: Vec::new(),
        };
        assert!(!redirect.is_success());
        assert!(redirect.is_redirection());
        assert_eq!(redirect.header("Location"), Some("https://example.com/next"));
    }
}

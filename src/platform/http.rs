//! Minimal HTTP transport abstraction.
//!
//! This is deliberately *not* a copy of the reqwest API. It captures only what
//! narou.rs needs: a GET or POST with headers/body, and a response with status,
//! headers, and a byte body. Redirect policy, cookies, timeouts, TLS, and
//! transport fallbacks (curl/wget) are the responsibility of the concrete
//! implementation; the domain layer must not depend on them.
//!
//! URL safety validation lives in `downloader::security` (domain layer) and is
//! applied by callers before handing a URL to an implementation.

/// HTTP method used by [`HttpRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
}

/// A minimal HTTP request.
#[derive(Debug, Clone)]
pub struct HttpRequest<'a> {
    pub url: &'a str,
    pub method: HttpMethod,
    /// Header name/value pairs, in order. Implementations may add their own
    /// headers (User-Agent etc.) but must keep these.
    pub headers: Vec<(String, String)>,
    /// POST body. `None` for GET or bodiless POST.
    pub body: Option<&'a [u8]>,
}

impl<'a> HttpRequest<'a> {
    pub fn get(url: &'a str) -> Self {
        Self {
            url,
            method: HttpMethod::Get,
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn post(url: &'a str, body: &'a [u8]) -> Self {
        Self {
            url,
            method: HttpMethod::Post,
            headers: Vec::new(),
            body: Some(body),
        }
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// A minimal HTTP response. The body is fully buffered; streaming is a later
/// extension for the specific high-volume paths (illustrations, backups).
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

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Transport for HTTP(S) requests.
///
/// Implementations decide redirect handling, cookies, timeouts, size limits,
/// and fallback strategies. `get_text`/`get_bytes` are conveniences over
/// [`HttpClient::send`]; implementations may override them if a native
/// shortcut (e.g. a pre-built client) is more efficient.
pub trait HttpClient: Send + Sync {
    /// Perform one request and return the full response.
    fn send(&self, request: HttpRequest<'_>) -> crate::error::Result<HttpResponse>;

    /// GET and decode the body as UTF-8 (lossy).
    fn get_text(&self, url: &str) -> crate::error::Result<String> {
        Ok(self.send(HttpRequest::get(url))?.text())
    }
}

/// Convenience blanket impl: any `&T` where `T: HttpClient` is itself an
/// `HttpClient` (so `&client` can be passed around without cloning).
impl<T: HttpClient + ?Sized> HttpClient for &T {
    fn send(&self, request: HttpRequest<'_>) -> crate::error::Result<HttpResponse> {
        (*self).send(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_builders_set_fields() {
        let req = HttpRequest::get("https://example.com/").with_header("Cookie", "a=b");
        assert_eq!(req.method, HttpMethod::Get);
        assert_eq!(req.headers, vec![("Cookie".into(), "a=b".into())]);
        assert!(req.body.is_none());

        let post = HttpRequest::post("https://example.com/api", b"payload");
        assert_eq!(post.method, HttpMethod::Post);
        assert_eq!(post.body, Some(&b"payload"[..]));
    }

    #[test]
    fn response_helpers() {
        let resp = HttpResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "text/html".into())],
            body: b"<html>hi</html>".to_vec(),
        };
        assert!(resp.is_success());
        assert_eq!(resp.header("content-type"), Some("text/html"));
        assert_eq!(resp.text(), "<html>hi</html>");
    }
}

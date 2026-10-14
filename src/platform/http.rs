//! Minimal HTTP transport abstraction.
//!
//! This is deliberately *not* a copy of the reqwest API. It captures only what
//! narou.rs needs: a GET or POST with headers/body, and a response with status,
//! headers, and a byte body. Redirect transitions are shared here; cookies,
//! timeouts, TLS, and transport fallbacks (curl/wget) are the responsibility of
//! the concrete implementation.
//!
//! The trait is async (boxed futures, `Send` on native and relaxed on wasm) so
//! Cloudflare Workers can drive it on their runtime while the native
//! implementation moves each blocking hop and the owning redirect chain through
//! `tokio::task::spawn_blocking`.
//!
//! URL safety validation is split: the syntax/address-literal judgement lives in
//! [`super::url_policy`] (usable without DNS) and is applied by callers before
//! handing a URL to an implementation, while [`HttpClient::validate_url`] lets an
//! implementation that *can* resolve DNS enforce the resolved-address check.

use super::PlatformFuture;
use std::sync::Arc;

use crate::error::{NarouError, Result};

/// Upper bound for accepted header field names.
///
/// Site definitions and relay callers are user-configured, so names carry an
/// explicit bound: 64 bytes is far beyond any registered header name, and
/// rejecting longer ones keeps a degenerate definition from being forwarded.
/// The bound is part of the shared rule, not a site-only extra.
pub const MAX_HEADER_NAME_BYTES: usize = 64;

/// Whether `name` is safe as an HTTP header field name: an RFC 9110 token
/// (`tchar`, which includes `!#$%&'*+-.^_`|~` and the alphanumerics), at most
/// [`MAX_HEADER_NAME_BYTES`] bytes.
///
/// Every transport and policy helper uses this one rule. The full token set
/// (rather than just `[A-Za-z0-9_-]`) keeps names such as `X.Foo` accepted.
pub fn is_safe_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_HEADER_NAME_BYTES
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
        })
}

/// Whether `value` is safe as an HTTP header field value: no ASCII control
/// characters, so CR/LF (and NUL) cannot inject another header line or split
/// the message.
pub fn is_safe_header_value(value: &str) -> bool {
    !value.bytes().any(|byte| byte.is_ascii_control())
}

/// Body cap for a response: trusted infrastructure callers may ask for a
/// tighter limit on success; non-2xx diagnostic bodies keep the full cap.
///
/// Shared by the native and Worker transports, which differ only in `cap`.
pub fn response_body_limit(status: u16, trusted: Option<usize>, cap: usize) -> usize {
    if (200..300).contains(&status) {
        trusted.unwrap_or(cap).min(cap)
    } else {
        cap
    }
}

/// HTTP method used by [`HttpRequest`].
///
/// `Put`/`Delete` exist for object storage (S3-compatible APIs write with
/// `PUT` and remove with `DELETE`); site downloads only use `Get`/`Post`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
}

/// How redirects are handled by the transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RedirectMode {
    /// Follow redirects through [`RedirectChain`] (default).
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
    /// Request body, shared without copying when a redirect retains it.
    pub body: Option<Arc<Vec<u8>>>,
    pub redirect: RedirectMode,
    /// `true` when the URL comes from the user's own configuration (an object
    /// storage endpoint) instead of crawled content.
    ///
    /// Transports then skip the public-address check (a local MinIO endpoint is
    /// legitimate), honour [`trusted_response_body_limit`], and do not apply
    /// site fetch fallbacks. Site fetches must leave this `false`.
    ///
    /// [`trusted_response_body_limit`]: HttpRequest::trusted_response_body_limit
    pub trusted_endpoint: bool,
    /// Optional tighter success-body bound for trusted infrastructure calls.
    /// Transports enforce it while reading, not after allocating the full body.
    /// Site requests and non-2xx diagnostic bodies keep their existing limits.
    pub trusted_response_body_limit: Option<usize>,
}

impl HttpRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            method: HttpMethod::Get,
            headers: Vec::new(),
            body: None,
            redirect: RedirectMode::Follow,
            trusted_endpoint: false,
            trusted_response_body_limit: None,
        }
    }

    pub fn post(url: impl Into<String>, body: impl Into<Vec<u8>>) -> Self {
        Self {
            url: url.into(),
            method: HttpMethod::Post,
            headers: Vec::new(),
            body: Some(Arc::new(body.into())),
            redirect: RedirectMode::Follow,
            trusted_endpoint: false,
            trusted_response_body_limit: None,
        }
    }

    /// `PUT` with a body, used by object storage writes.
    pub fn put(url: impl Into<String>, body: impl Into<Vec<u8>>) -> Self {
        Self {
            url: url.into(),
            method: HttpMethod::Put,
            headers: Vec::new(),
            body: Some(Arc::new(body.into())),
            redirect: RedirectMode::Follow,
            trusted_endpoint: false,
            trusted_response_body_limit: None,
        }
    }

    /// `DELETE` without a body, used by object storage removals.
    pub fn delete(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            method: HttpMethod::Delete,
            headers: Vec::new(),
            body: None,
            redirect: RedirectMode::Follow,
            trusted_endpoint: false,
            trusted_response_body_limit: None,
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

    /// Mark the request as an infrastructure call (object storage endpoint).
    ///
    /// See [`HttpRequest::trusted_endpoint`] for what transports change.
    pub fn with_trusted_endpoint(mut self) -> Self {
        self.trusted_endpoint = true;
        self
    }

    pub fn with_trusted_response_body_limit(mut self, limit: usize) -> Self {
        self.trusted_response_body_limit = Some(limit);
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

/// HTTP(S) syntax validation without a public-address restriction.
///
/// Trusted infrastructure may legitimately use a local MinIO endpoint.
/// Untrusted adapters must additionally apply their public-URL/DNS policy.
pub fn validate_http_url_syntax(url: &str) -> Result<()> {
    parse_http_url(url).map(|_| ())
}

fn parse_http_url(value: &str) -> Result<url::Url> {
    if value.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(NarouError::Http("URL contains control characters".into()));
    }
    let parsed = url::Url::parse(value)
        .map_err(|error| NarouError::Http(format!("invalid URL: {error}")))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(NarouError::Http("URL must be http(s) with a host".into()));
    }
    Ok(parsed)
}

/// Cookie-compatible hosts: equal, or one is a parent of the other.
pub fn same_site_hosts(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            a == b
                || a.strip_suffix(b).is_some_and(|prefix| prefix.ends_with('.'))
                || b.strip_suffix(a).is_some_and(|prefix| prefix.ends_with('.'))
        }
        _ => false,
    }
}

fn strip_cross_site_credentials(
    headers: &mut Vec<(String, String)>,
    from: &url::Url,
    to: &url::Url,
) {
    let origin_changed = from.origin() != to.origin();
    let drop_cookie = !same_site_hosts(from.host_str(), to.host_str())
        || (from.scheme() == "https" && to.scheme() == "http");
    headers.retain(|(name, _)| {
        !(origin_changed && name.eq_ignore_ascii_case("authorization"))
            && !(drop_cookie && name.eq_ignore_ascii_case("cookie"))
    });
}

fn next_redirect_hop(
    current: &url::Url,
    status: u16,
    location: Option<&str>,
) -> Result<Option<url::Url>> {
    if !matches!(status, 301 | 302 | 303 | 307 | 308) {
        return Ok(None);
    }
    let Some(location) = location else {
        return Ok(None);
    };
    if location.is_empty()
        || location.bytes().any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(NarouError::Http("invalid redirect location".into()));
    }
    let next = current.join(location)
        .map_err(|error| NarouError::Http(format!("invalid redirect location: {error}")))?;
    validate_http_url_syntax(next.as_str())?;
    Ok(Some(next))
}

/// The sole owner of an outgoing request and its redirect transitions.
///
/// Concrete transports execute one hop with automatic redirects disabled,
/// persist that hop's cookies, then call [`Self::advance`]. They must validate
/// each borrowed request before sending it, including trusted endpoint syntax.
pub struct RedirectChain {
    request: HttpRequest,
    original_url: String,
    hops: usize,
    limit: usize,
}

impl RedirectChain {
    pub fn new(request: HttpRequest, limit: usize) -> Self {
        Self {
            original_url: request.url.clone(),
            request,
            hops: 0,
            limit,
        }
    }

    pub fn request(&self) -> &HttpRequest {
        &self.request
    }

    pub fn hops(&self) -> usize {
        self.hops
    }

    /// Advance to the next outgoing hop, or return `false` for a terminal
    /// response. The limit is checked before any next request can be sent.
    pub fn advance(&mut self, response: &HttpResponse) -> Result<bool> {
        if !self.advance_parts(response.status, response.header("Location"))? {
            return Ok(false);
        }
        // Persisting adapters have just written this response against the
        // cookie actually sent. Replay its rotation only when that cookie
        // survived the next hop's scope/downgrade checks; never create or
        // restore a stripped cookie.
        if self.request.header("Cookie").is_some() {
            let values = response.headers.iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
                .map(|(_, value)| value.clone())
                .collect::<Vec<_>>();
            if !values.is_empty() {
                for (name, cookie) in &mut self.request.headers {
                    if name.eq_ignore_ascii_case("cookie") {
                        *cookie = super::cookie_store::apply_set_cookie(cookie, &values);
                    }
                }
            }
        }
        Ok(true)
    }

    pub fn advance_parts(&mut self, status: u16, location: Option<&str>) -> Result<bool> {
        if self.request.redirect == RedirectMode::Manual
            || !matches!(status, 301 | 302 | 303 | 307 | 308)
            || location.is_none()
        {
            return Ok(false);
        }
        let current = parse_http_url(&self.request.url)?;
        let Some(next) = next_redirect_hop(&current, status, location).map_err(|error| {
            NarouError::Http(format!("{error} while redirecting {}", self.original_url))
        })? else {
            return Ok(false);
        };
        if self.hops == self.limit {
            return Err(NarouError::Http(format!(
                "redirect limit exceeded for {} after {} hops",
                self.original_url, self.limit
            )));
        }
        strip_cross_site_credentials(&mut self.request.headers, &current, &next);
        if (status == 303 && self.request.method != HttpMethod::Get)
            || (matches!(status, 301 | 302) && self.request.method == HttpMethod::Post)
        {
            self.request.method = HttpMethod::Get;
            self.request.body = None;
            self.request.headers.retain(|(name, _)| {
                !name.get(..8).is_some_and(|prefix| prefix.eq_ignore_ascii_case("content-"))
                    && !name.eq_ignore_ascii_case("transfer-encoding")
                    && !name.eq_ignore_ascii_case("trailer")
                    && !name.eq_ignore_ascii_case("expect")
            });
        }
        self.request.url = next.into();
        self.hops += 1;
        Ok(true)
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

    /// Every value of the given (case-insensitive) name, in receipt order.
    ///
    /// Repeated headers (above all `Set-Cookie`, which may arrive several
    /// times) are part of the response; the first-value [`Self::header`] is
    /// not enough to write them back.
    pub fn header_values(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect()
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn is_redirection(&self) -> bool {
        (300..400).contains(&self.status)
    }
}

/// Borrowed site admission policy. Transport retries use the target scope,
/// including when the actual connection goes through a configured relay.
#[derive(Clone, Copy)]
pub struct HttpPacing<'a> {
    pub limiter: &'a dyn super::RateLimiter,
    pub scope: &'a super::RateLimitScope,
}

impl<'a> HttpPacing<'a> {
    /// The initial scope is already resolved by fetch policy. A redirect to a
    /// different host retains the site's floor and wait-step classification.
    pub fn scope_for(&self, chain: &RedirectChain) -> Result<std::borrow::Cow<'a, super::RateLimitScope>> {
        if chain.hops() == 0 {
            return Ok(std::borrow::Cow::Borrowed(self.scope));
        }
        let parsed = parse_http_url(&chain.request().url)?;
        let host = parsed.host_str().expect("validated HTTP host");
        if host == self.scope.site {
            return Ok(std::borrow::Cow::Borrowed(self.scope));
        }
        Ok(std::borrow::Cow::Owned(super::RateLimitScope {
            site: host.to_owned(),
            narou: self.scope.narou,
            min_interval: self.scope.min_interval,
        }))
    }
}

/// Transport for HTTP(S) requests.
///
/// Implementations drive [`RedirectChain`] and provide cookies, timeouts, size
/// limits and fallback strategies. The returned future is `Send` on native (so
/// domain services can be driven on a multi-threaded executor, e.g.
/// `tokio::spawn` for parallel updates) and relaxed on `wasm32`, via
/// [`PlatformFuture`].
pub trait HttpClient: Send + Sync {
    /// Perform the request under its redirect policy and return the full response.
    fn send<'a>(
        &'a self,
        request: HttpRequest,
    ) -> PlatformFuture<'a, crate::error::Result<HttpResponse>>;

    /// Site requests acquire once per physical network attempt. Transports
    /// with redirects or fallback tiers must override this single-hop default.
    fn send_paced<'a>(
        &'a self,
        request: HttpRequest,
        pacing: HttpPacing<'a>,
    ) -> PlatformFuture<'a, crate::error::Result<HttpResponse>> {
        Box::pin(async move {
            pacing.limiter.acquire(pacing.scope).await?;
            self.send(request).await
        })
    }

    /// Validate a URL before the request is sent.
    ///
    /// The default implementation checks the URL syntax and rejects
    /// non-public address literals, which is all a platform without DNS
    /// resolution (wasm) can do. Implementations that can resolve hosts must
    /// override this to also reject hostnames resolving to non-public
    /// addresses; native does so through `downloader::security`.
    fn validate_url<'a>(
        &'a self,
        url: &'a str,
    ) -> PlatformFuture<'a, crate::error::Result<()>> {
        Box::pin(async move {
            super::url_policy::validate_url_syntax(url).map_err(crate::error::NarouError::Http)
        })
    }
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

    fn send_paced<'a>(
        &'a self,
        request: HttpRequest,
        pacing: HttpPacing<'a>,
    ) -> PlatformFuture<'a, crate::error::Result<HttpResponse>> {
        (*self).send_paced(request, pacing)
    }

    fn validate_url<'a>(
        &'a self,
        url: &'a str,
    ) -> PlatformFuture<'a, crate::error::Result<()>> {
        (*self).validate_url(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn header_values_keep_repeats_in_receipt_order() {
        let response = HttpResponse {
            status: 200,
            headers: vec![
                ("Set-Cookie".into(), "a=1".into()),
                ("Content-Type".into(), "text/html".into()),
                ("set-cookie".into(), "b=2".into()),
            ],
            body: Vec::new(),
        };
        assert_eq!(response.header_values("set-cookie"), vec!["a=1", "b=2"]);
        assert_eq!(response.header("SET-COOKIE"), Some("a=1"));
        assert!(response.header_values("missing").is_empty());
    }

    #[test]
    fn header_name_rule_accepts_rfc_tokens_within_the_bound() {
        for name in [
            "Accept-Language",
            "X.Foo",
            "x_underscore",
            "!#$%&'*+-.^_`|~",
            "Narou-Test",
        ] {
            assert!(is_safe_header_name(name), "{name:?} is a token");
        }
        let long = "x".repeat(MAX_HEADER_NAME_BYTES + 1);
        for name in ["", "bad name", "bad:name", "bad\r\nname", long.as_str()] {
            assert!(!is_safe_header_name(name), "{name:?} must be rejected");
        }
        assert!(is_safe_header_name(&"x".repeat(MAX_HEADER_NAME_BYTES)));
    }

    #[test]
    fn header_value_rule_rejects_every_control_byte() {
        assert!(is_safe_header_value("session=abc123; path=/"));
        assert!(is_safe_header_value("text/html,application/xhtml+xml;q=0.9"));
        for value in ["a\r\nInjected: 1", "a\rb", "a\nb", "a\0b", "a\x1fb"] {
            assert!(!is_safe_header_value(value), "{value:?} must be rejected");
        }
    }

    #[test]
    fn response_body_limit_is_tighter_only_on_success() {
        assert_eq!(response_body_limit(200, Some(1024), 4096), 1024);
        assert_eq!(response_body_limit(200, None, 4096), 4096);
        // A trusted limit above the cap is clamped, on success and after it.
        assert_eq!(response_body_limit(204, Some(8192), 4096), 4096);
        assert_eq!(response_body_limit(404, Some(1024), 4096), 4096);
        assert_eq!(response_body_limit(503, None, 4096), 4096);
    }

    #[test]
    fn same_site_hosts_keeps_parent_and_subdomain_cookie_compatibility() {
        assert!(same_site_hosts(Some("syosetu.org"), Some("syosetu.org")));
        assert!(same_site_hosts(Some("h.syosetu.org"), Some("syosetu.org")));
        assert!(same_site_hosts(Some("syosetu.org"), Some("h.syosetu.org")));
        assert!(!same_site_hosts(Some("hsyosetu.org"), Some("syosetu.org")));
        assert!(!same_site_hosts(Some("syosetu.org"), None));
    }

    fn redirect(status: u16, location: &str) -> HttpResponse {
        HttpResponse {
            status,
            headers: vec![("Location".into(), location.into())],
            body: Vec::new(),
        }
    }

    #[test]
    fn redirect_credentials_follow_cookie_scope_and_exact_origin_separately() {
        for (destination, cookie, authorization) in [
            ("https://example.com/next", true, true),
            ("https://example.com:443/next", true, true),
            ("https://sub.example.com/next", true, false),
            ("https://example.com:8443/next", true, false),
            ("http://example.com/next", false, false),
            ("https://example.com.attacker.test/next", false, false),
            ("https://collector.test/next", false, false),
        ] {
            let request = HttpRequest::get("https://example.com/start")
                .with_header("cOoKiE", "sid=secret")
                .with_header("aUtHoRiZaTiOn", "Bearer secret")
                .with_header("Accept", "text/html");
            let mut chain = RedirectChain::new(request, 10);
            assert!(chain.advance(&redirect(302, destination)).unwrap());
            assert_eq!(chain.request().header("Cookie").is_some(), cookie, "{destination}");
            assert_eq!(chain.request().header("Authorization").is_some(), authorization, "{destination}");
            assert_eq!(chain.request().header("Accept"), Some("text/html"));
        }
        let mut chain = RedirectChain::new(
            HttpRequest::get("https://example.com/start")
                .with_header("Cookie", "sid=secret")
                .with_header("Authorization", "Bearer secret"),
            10,
        );
        chain.advance(&redirect(302, "http://example.com/next")).unwrap();
        chain.advance(&redirect(302, "https://example.com/return")).unwrap();
        assert!(chain.request().header("Cookie").is_none());
        assert!(chain.request().header("Authorization").is_none());
    }

    #[test]
    fn only_a_retained_sent_cookie_receives_the_intermediate_rotation() {
        for (destination, original, expected) in [
            ("https://sub.example.com/next", Some("sid=old; over18=yes"), Some("sid=fresh; over18=yes")),
            ("https://collector.test/next", Some("sid=old"), None),
            ("http://example.com/next", Some("sid=old"), None),
            ("https://example.com/next", None, None),
        ] {
            let mut request = HttpRequest::get("https://example.com/start");
            if let Some(cookie) = original {
                request = request.with_header("Cookie", cookie);
            }
            let mut response = redirect(302, destination);
            response.headers.push(("Set-Cookie".into(), "sid=fresh; Path=/".into()));
            let mut chain = RedirectChain::new(request, 10);
            assert!(chain.advance(&response).unwrap());
            assert_eq!(chain.request().header("Cookie"), expected, "{destination}");
        }
    }

    #[test]
    fn redirects_preserve_or_drop_the_owned_payload_with_the_method() {
        for (status, method, drops_body) in [
            (301, HttpMethod::Post, true),
            (302, HttpMethod::Post, true),
            (303, HttpMethod::Post, true),
            (303, HttpMethod::Put, true),
            (303, HttpMethod::Delete, true),
            (303, HttpMethod::Get, false),
            (301, HttpMethod::Put, false),
            (302, HttpMethod::Put, false),
            (307, HttpMethod::Post, false),
            (308, HttpMethod::Post, false),
        ] {
            let mut request = HttpRequest::post("https://example.com/start", b"payload".to_vec())
                .with_header("Content-Type", "application/octet-stream")
                .with_header("content-length", "7")
                .with_header("Content-Encoding", "identity")
                .with_header("Transfer-Encoding", "chunked")
                .with_header("Trailer", "Digest")
                .with_header("Expect", "100-continue")
                .with_header("X-Keep", "yes");
            request.method = method;
            let mut chain = RedirectChain::new(request, 10);
            assert!(chain.advance(&redirect(status, "../next")).unwrap());
            assert_eq!(chain.request().url, "https://example.com/next");
            assert_eq!(chain.request().method, if drops_body { HttpMethod::Get } else { method });
            if drops_body {
                assert!(chain.request().body.is_none());
                for name in ["Content-Type", "Content-Length", "Content-Encoding", "Transfer-Encoding", "Trailer", "Expect"] {
                    assert!(chain.request().header(name).is_none(), "{status}: {name}");
                }
            } else {
                assert_eq!(chain.request().body.as_deref().unwrap().as_slice(), b"payload");
                assert_eq!(chain.request().header("Content-Type"), Some("application/octet-stream"));
            }
            assert_eq!(chain.request().header("X-Keep"), Some("yes"));
        }
    }

    #[test]
    fn only_defined_redirect_statuses_follow_and_manual_never_advances() {
        for status in [200, 300, 304, 305, 306, 404] {
            let mut chain = RedirectChain::new(HttpRequest::get("https://example.com/start"), 10);
            assert!(!chain.advance(&redirect(status, "/next")).unwrap(), "{status}");
            assert_eq!(chain.request().url, "https://example.com/start");
        }
        for status in [301, 302, 303, 307, 308] {
            let mut chain = RedirectChain::new(
                HttpRequest::post("https://example.com/start", b"payload".to_vec())
                    .with_redirect(RedirectMode::Manual),
                0,
            );
            assert!(!chain.advance(&redirect(status, "/next")).unwrap());
            assert_eq!(chain.request().method, HttpMethod::Post);
            assert_eq!(chain.request().body.as_deref().unwrap().as_slice(), b"payload");
        }
    }

    #[test]
    fn redirect_limit_and_invalid_location_fail_before_another_request() {
        let mut chain = RedirectChain::new(HttpRequest::get("https://example.com/original"), 1);
        assert!(chain.advance(&redirect(302, "/first")).unwrap());
        let error = chain.advance(&redirect(307, "/second")).unwrap_err();
        assert!(error.to_string().contains("https://example.com/original"));
        assert_eq!(chain.request().url, "https://example.com/first");
        assert_eq!(chain.hops(), 1);

        for location in ["", "http://[::1", "javascript:alert(1)", "https://example.com/\nsecret", "/has space", "/\0secret"] {
            let mut chain = RedirectChain::new(HttpRequest::get("https://example.com/original"), 10);
            assert!(chain.advance(&redirect(302, location)).is_err(), "{location:?}");
            assert_eq!(chain.hops(), 0);
        }
        let mut chain = RedirectChain::new(HttpRequest::get("https://example.com/original"), 0);
        assert!(!chain.advance_parts(302, None).unwrap());
    }
}

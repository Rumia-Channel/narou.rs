//! Platform-neutral HTTP policy helpers for the downloader.
//!
//! These helpers sit between the domain logic and the [`HttpClient`]
//! transport. They own everything that is *not* transport-specific:
//!
//! - character decoding (`decode_with_encoding`)
//! - HTTP status → domain error mapping (`ensure_success_response`)
//! - manual redirect-chain resolution (`resolve_final_url`)
//! - the standard fetch-with-rate-limit flow (`fetch_text` / `fetch_bytes`)
//!
//! The transport (native curl/reqwest/wget fallback, Worker Fetch) only moves
//! bytes; it never decodes and never decides what a 404 means.


mod session;
use crate::error::{NarouError, Result};
use crate::platform::http::{RedirectChain, is_safe_header_name, is_safe_header_value};
use crate::platform::{
    HttpClient, HttpRequest, HttpResponse, HttpStreamResponse, RateLimitScope, RateLimiter, RedirectMode,
};

use super::security::MAX_REDIRECTS;

/// Everything a site fetch needs beyond the URL: the site's static headers
/// (cookie jar value plus any `headers:` declared in the site definition) and
/// whether it uses the narou rate-limit scope.
///
/// Every outbound request for a site carries one of these, so the site
/// definition is the single place that decides what is sent.
#[derive(Debug, Clone, Default)]
pub struct FetchPolicy {
    headers: Vec<(String, String)>,
    cookie_hosts: Vec<String>,
    narou: bool,
    /// Site definition's `min_interval`, applied to the rate-limit scope.
    min_interval: Option<f64>,
    session: Option<std::sync::Arc<parking_lot::Mutex<session::SessionCookies>>>,
}

impl FetchPolicy {
    /// Build the policy from a site definition. Header names/values that could
    /// inject a second request are dropped here rather than at send time.
    pub fn for_site(setting: &super::site_setting::SiteSetting) -> Self {
        let mut headers = Vec::new();
        if let Some(cookie) = setting.cookie() {
            headers.push(("Cookie".to_string(), cookie.to_string()));
        }
        headers.extend(setting.header_list());
        headers.retain(|(name, value)| is_safe_header_name(name) && is_safe_header_value(value));
        let mut cookie_hosts = Vec::with_capacity(1 + setting.cookie_hosts.len() + setting.login_cookie_hosts.len());
        cookie_hosts.push(setting.domain.to_ascii_lowercase());
        cookie_hosts.extend(setting.cookie_hosts.iter().filter(|host| !host.is_empty()).map(|host| host.to_ascii_lowercase()));
        cookie_hosts.extend(setting.login_cookie_hosts.iter().cloned());
        Self {
            headers,
            cookie_hosts,
            narou: setting.is_narou,
            min_interval: setting.min_interval,
            session: setting.cookie_session.then(|| std::sync::Arc::new(parking_lot::Mutex::new(session::SessionCookies::default()))),
        }
    }

    /// Build the policy for the site a novel record belongs to.
    ///
    /// The record's `toc_url` selects the site definition (`matches_url` —
    /// the same rule the native CLI uses); records matching no known site
    /// get the headerless default. Converter-side illustration fetches ride
    /// on this: image hosts such as `i.pximg.net` reject requests without
    /// the site's `Referer`.
    pub fn for_record(
        site_settings: &[super::site_setting::SiteSetting],
        record: &crate::db::NovelRecord,
    ) -> Self {
        site_settings
            .iter()
            .find(|setting| setting.matches_url(&record.toc_url))
            .map(Self::for_site)
            .unwrap_or_default()
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        let (name, value) = (name.into(), value.into());
        if is_safe_header_name(&name) && is_safe_header_value(&value) {
            self.headers.push((name, value));
        }
        self
    }

    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// The site definition's `min_interval`, when it declares one.
    pub fn min_interval(&self) -> Option<f64> {
        self.min_interval
    }

    pub fn narou(&self) -> bool {
        self.narou
    }

    /// Headers to send to `url`: the policy's headers minus the static cookie
    /// when `url` is outside the site's cookie scope.
    ///
    /// The single source for that scope rule — the direct fetch and the manual
    /// redirect chain both start from it.
    pub fn headers_for(&self, url: &str) -> Vec<(String, String)> {
        let mut headers = self.headers.clone();
        if !self.allows_cookie(url) {
            headers.retain(|(name, _)| !name.eq_ignore_ascii_case("Cookie"));
        } else if let Some(session) = &self.session {
            session.lock().apply(url, &mut headers);
        }
        headers
    }

    /// A configured Cookie is sent to this source; this does not prove access.
    pub(crate) fn has_cookie_for(&self, url: &str) -> bool {
        self.allows_cookie(url) && self.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("Cookie") && !value.trim().is_empty()
        })
    }

    fn allows_cookie(&self, url: &str) -> bool {
        let Ok(url) = url::Url::parse(url) else {
            return false;
        };
        let Some(host) = url.host_str() else {
            return false;
        };
        self.cookie_hosts.iter().any(|scope| {
            host == scope || host.strip_suffix(scope).is_some_and(|prefix| prefix.ends_with('.'))
        })
    }

    fn observe_response(&self, url: &str, response: &HttpResponse) {
        if self.allows_cookie(url) && let Some(session) = &self.session {
            session.lock().observe(url, &response.headers);
        }
    }

    /// Private representation identity; credentials are never exposed or logged.
    /// Independently created response-cookie sessions must not share cached input.
    pub(crate) fn cache_context(&self) -> Option<[u8; 32]> {
        use sha2::{Digest, Sha256};
        if self.session.is_some() { return None; }
        let mut digest = Sha256::new();
        for part in self.headers.iter().flat_map(|(name, value)| [name.as_str(), value.as_str()])
            .chain(self.cookie_hosts.iter().map(String::as_str))
        {
            digest.update((part.len() as u64).to_le_bytes());
            digest.update(part.as_bytes());
        }
        Some(digest.finalize().into())
    }
}


/// Decode response bytes using the site's declared encoding.
///
/// `None`/`utf-8` uses lossy UTF-8; other labels go through `encoding_rs`
/// (Shift_JIS etc.). This is the single decode point for all fetched text.
pub fn decode_with_encoding(bytes: &[u8], encoding: Option<&str>) -> String {
    let enc = match encoding {
        Some(e) if !e.eq_ignore_ascii_case("utf-8") && !e.eq_ignore_ascii_case("utf8") => e,
        _ => return String::from_utf8_lossy(bytes).into_owned(),
    };
    let encoder = encoding_rs::Encoding::for_label(enc.as_bytes());
    match encoder {
        Some(enc) => {
            let (cow, _encoding_used, _had_errors) = enc.decode(bytes);
            cow.into_owned()
        }
        None => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Map an HTTP status to the domain error narou.rb users expect.
///
/// - 404 → `NotFound` (novel deleted/private)
/// - 503 → `SuspendDownload` (rate limited)
/// - other 4xx/5xx → `Http` with the status
pub fn ensure_success_response(url: &str, response: HttpResponse) -> Result<HttpResponse> {
    ensure_success_status(url, response.status)?;
    Ok(response)
}

/// True when a fetch error should stop the tier fallback instead of trying
/// the next transport. 404/503 are definitive answers from the server.
pub fn should_stop_fetch_fallback(err: &NarouError) -> bool {
    matches!(
        err,
        NarouError::NotFound(_) | NarouError::SuspendDownload(_)
    )
}


/// Resolve the final URL through shared transitions using `Manual` transport
/// requests. Each hop is validated and rate-limited before sending.
pub async fn resolve_final_url(
    http: &dyn HttpClient,
    rate_limiter: &dyn RateLimiter,
    url: &str,
    policy: &FetchPolicy,
) -> Result<String> {
    Ok(resolve_final_url_with_body(http, rate_limiter, url, policy)
        .await?
        .0)
}

/// Same redirect-chain resolution as [`resolve_final_url`], additionally
/// returning the terminal response when the *first* request already reached
/// a non-redirect answer. Callers may reuse that body instead of issuing a
/// second GET for the same URL; a redirected chain returns `None` because
/// the cookie scope may have changed between hops.
pub async fn resolve_final_url_with_body(
    http: &dyn HttpClient,
    rate_limiter: &dyn RateLimiter,
    url: &str,
    policy: &FetchPolicy,
) -> Result<(String, Option<HttpResponse>)> {
    let mut request = HttpRequest::get(url);
    request.headers = policy.headers_for(url);
    let mut chain = RedirectChain::new(request, MAX_REDIRECTS);
    loop {
        http.validate_url(&chain.request().url).await?;
        let scope = scope_for(host_of(&chain.request().url), policy);
        let mut request = chain.request().clone().with_redirect(RedirectMode::Manual);
        if chain.hops() > 0 && policy.allows_cookie(&request.url) && let Some(session) = &policy.session {
            session.lock().apply(&request.url, &mut request.headers);
        }
        let response = http.send_paced(
            request, crate::platform::http::HttpPacing { limiter: rate_limiter, scope: &scope },
        ).await?;
        policy.observe_response(&chain.request().url, &response);
        if !chain.advance(&response)? {
            // Only an immediate terminal answer can be reused: later hops
            // may have a different cookie scope than the caller's fetch.
            let body = (chain.hops() == 0 && !response.is_redirection()).then_some(response);
            let final_url = url::Url::parse(&chain.request().url)
                .map_err(|error| NarouError::Http(error.to_string()))?;
            return Ok((final_url.into(), body));
        }
    }
}

/// Standard GET flow: validate → rate-limit → send → status policy → decode.
pub async fn fetch_text(
    http: &dyn HttpClient,
    rate_limiter: &dyn RateLimiter,
    url: &str,
    policy: &FetchPolicy,
    encoding: Option<&str>,
) -> Result<String> {
    let response = fetch_bytes(http, rate_limiter, url, policy).await?;
    Ok(decode_with_encoding(&response.body, encoding))
}

/// Standard GET flow returning raw bytes plus the content type.
pub async fn fetch_bytes(
    http: &dyn HttpClient,
    rate_limiter: &dyn RateLimiter,
    url: &str,
    policy: &FetchPolicy,
) -> Result<HttpResponse> {
    let mut request = HttpRequest::get(url);
    request.headers = policy.headers_for(url);
    send_request(http, rate_limiter, request, policy, true).await
}

/// Pull-driven binary GET with the same site headers, redirect transitions,
/// response cookies, URL validation and physical-attempt pacing as text fetches.
pub async fn fetch_stream(
    http: &dyn HttpClient,
    rate_limiter: &dyn RateLimiter,
    url: &str,
    policy: &FetchPolicy,
) -> Result<HttpStreamResponse> {
    let mut request = HttpRequest::get(url);
    request.headers = policy.headers_for(url);
    if policy.session.is_none() {
        http.validate_url(url).await?;
        let scope = scope_for(host_of(url), policy);
        let response = http.send_stream_paced(
            request, crate::platform::http::HttpPacing { limiter: rate_limiter, scope: &scope },
        ).await?;
        ensure_success_status(url, response.status)?;
        return Ok(response);
    }
    let mut chain = RedirectChain::new(request, MAX_REDIRECTS);
    loop {
        let current = &chain.request().url;
        http.validate_url(current).await?;
        let mut outgoing = chain.request().clone().with_redirect(RedirectMode::Manual);
        if chain.hops() > 0 && policy.allows_cookie(current) && let Some(session) = &policy.session {
            session.lock().apply(current, &mut outgoing.headers);
        }
        let scope = scope_for(host_of(current), policy);
        let response = http.send_stream_paced(
            outgoing, crate::platform::http::HttpPacing { limiter: rate_limiter, scope: &scope },
        ).await?;
        if policy.allows_cookie(current) && let Some(session) = &policy.session {
            session.lock().observe(current, &response.headers);
        }
        if !chain.advance_parts(response.status, response.header("Location"))? {
            ensure_success_status(&chain.request().url, response.status)?;
            return Ok(response);
        }
        // Dropping an unused redirect body cancels it instead of buffering it.
    }
}

fn ensure_success_status(url: &str, status: u16) -> Result<()> {
    match status {
        404 => Err(NarouError::NotFound(url.to_string())),
        503 => Err(NarouError::SuspendDownload("Rate limited (503)".into())),
        status if !(200..300).contains(&status) => Err(NarouError::Http(format!(
            "HTTP {status} while fetching {url}"
        ))),
        _ => Ok(()),
    }
}

/// JSON dependencies inspect status separately without bypassing URL, pacing,
/// redirects, credential scoping or response-cookie handling.
pub(crate) async fn fetch_json_response(
    http: &dyn HttpClient, rate_limiter: &dyn RateLimiter,
    url: &str, policy: &FetchPolicy,
) -> Result<HttpResponse> {
    let mut request = HttpRequest::get(url);
    request.headers = policy.headers_for(url);
    send_request(http, rate_limiter, request, policy, false).await
}

/// Fetch an encoded form through the site's credentials and admission policy.
pub async fn fetch_form_text(
    http: &dyn HttpClient,
    rate_limiter: &dyn RateLimiter,
    url: &str,
    body: String,
    headers: Vec<(String, String)>,
    policy: &FetchPolicy,
    encoding: Option<&str>,
) -> Result<String> {
    let mut request = HttpRequest::post(url, body);
    request.headers = policy.headers_for(url);
    request.headers.retain(|(name, _)| !name.eq_ignore_ascii_case("content-type"));
    request.headers.push(("Content-Type".into(), "application/x-www-form-urlencoded".into()));
    for (name, value) in headers {
        if !is_safe_header_name(&name) || !is_safe_header_value(&value)
            || matches!(name.to_ascii_lowercase().as_str(),
                "cookie" | "host" | "authorization" | "proxy-authorization"
                | "content-length" | "content-type" | "transfer-encoding")
        {
            return Err(NarouError::SiteSetting("unsafe form request header".into()));
        }
        request.headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(&name));
        request.headers.push((name, value));
    }
    let response = send_request(http, rate_limiter, request, policy, true).await?;
    Ok(decode_with_encoding(&response.body, encoding))
}

async fn send_request(
    http: &dyn HttpClient,
    rate_limiter: &dyn RateLimiter,
    request: HttpRequest,
    policy: &FetchPolicy,
    require_success: bool,
) -> Result<HttpResponse> {
    if policy.session.is_none() && request.method == crate::platform::HttpMethod::Get {
        http.validate_url(&request.url).await?;
        let scope = scope_for(host_of(&request.url), policy);
        let url = request.url.clone();
        let response = http.send_paced(
            request, crate::platform::http::HttpPacing { limiter: rate_limiter, scope: &scope },
        ).await?;
        return if require_success { ensure_success_response(&url, response) } else { Ok(response) };
    }
    let form_origin = (request.method == crate::platform::HttpMethod::Post)
        .then(|| url::Url::parse(&request.url).map(|url| url.origin()))
        .transpose().map_err(|error| NarouError::Http(error.to_string()))?;
    let mut chain = RedirectChain::new(request, MAX_REDIRECTS);
    loop {
        let current = &chain.request().url;
        http.validate_url(current).await?;
        if let Some(origin) = &form_origin {
            let target = url::Url::parse(current).map_err(|error| NarouError::Http(error.to_string()))?;
            if target.origin() != *origin {
                return Err(NarouError::Http("form redirect changed origin".into()));
            }
        }
        let mut outgoing = chain.request().clone().with_redirect(RedirectMode::Manual);
        if chain.hops() > 0 && policy.allows_cookie(current) && let Some(session) = &policy.session {
            session.lock().apply(current, &mut outgoing.headers);
        }
        let scope = scope_for(host_of(current), policy);
        let response = http.send_paced(
            outgoing, crate::platform::http::HttpPacing { limiter: rate_limiter, scope: &scope },
        ).await?;
        policy.observe_response(current, &response);
        if !chain.advance_parts(response.status, response.header("Location"))? {
            return if require_success { ensure_success_response(&chain.request().url, response) } else { Ok(response) };
        }
    }
}

fn scope_for(host: String, policy: &FetchPolicy) -> RateLimitScope {
    let scope = if policy.narou {
        RateLimitScope::narou(host)
    } else {
        RateLimitScope::site(host)
    };
    scope.with_min_interval(policy.min_interval)
}

pub(crate) fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_string))
        .unwrap_or_else(|| "__global__".to_string())
}

/// Extract the host/domain portion of an http(s) URL without parsing.
/// Used for display and site grouping; `host_of` is preferred when the
/// URL is known to be well-formed.
pub fn domain_of(url: &str) -> &str {
    let s = url.strip_prefix("https://").unwrap_or(url);
    let s = s.strip_prefix("http://").unwrap_or(s);
    s.split('/').next().unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_utf8_lossy_by_default() {
        assert_eq!(
            decode_with_encoding(b"<html>hi</html>", None),
            "<html>hi</html>"
        );
        assert_eq!(
            decode_with_encoding(b"<html>hi</html>", Some("utf-8")),
            "<html>hi</html>"
        );
        // Invalid UTF-8 falls back lossy, never panics.
        let _ = decode_with_encoding(&[0xff, 0xfe, 0x00], Some("utf-8"));
    }

    #[test]
    fn decode_shift_jis() {
        // "こんにちは" in Shift_JIS.
        let sjis = [0x82, 0xb1, 0x82, 0xf1, 0x82, 0xc9, 0x82, 0xbf, 0x82, 0xcd];
        assert_eq!(decode_with_encoding(&sjis, Some("shift_jis")), "こんにちは");
    }

    #[test]
    fn ensure_success_maps_statuses() {
        let ok = HttpResponse {
            status: 200,
            headers: vec![],
            body: vec![],
        };
        assert!(ensure_success_response("https://e.com/", ok).is_ok());

        let not_found = HttpResponse {
            status: 404,
            headers: vec![],
            body: vec![],
        };
        let err = ensure_success_response("https://e.com/missing", not_found).unwrap_err();
        assert!(matches!(err, NarouError::NotFound(_)));

        let suspended = HttpResponse {
            status: 503,
            headers: vec![],
            body: vec![],
        };
        let err = ensure_success_response("https://e.com/", suspended).unwrap_err();
        assert!(matches!(err, NarouError::SuspendDownload(_)));

        let server_error = HttpResponse {
            status: 500,
            headers: vec![],
            body: vec![],
        };
        let err = ensure_success_response("https://e.com/", server_error).unwrap_err();
        assert!(err.to_string().contains("HTTP 500"));
    }

    #[test]
    fn fetch_policy_carries_site_headers_and_cookie() {
        let setting: super::super::site_setting::SiteSetting = serde_yaml::from_str(
            r#"
name: Example
domain: example.com
top_url: https://example.com
sitename: Example
toc_url: https://example.com/\k<ncode>
cookie: over18=yes
headers:
  Referer: https://example.com/
"#,
        )
        .unwrap();
        let policy = FetchPolicy::for_site(&setting);

        assert_eq!(
            policy.headers(),
            &[
                ("Cookie".to_string(), "over18=yes".to_string()),
                ("Referer".to_string(), "https://example.com/".to_string()),
            ]
        );
        assert!(!policy.narou());
    }

    /// The real pixiv definition declares `Referer: \k<top_url>/`, and a
    /// record whose `toc_url` is one of the pixiv `url:` shapes (including
    /// the `/ajax/novel/{id}` form saved records use) resolves to a policy
    /// carrying that `Referer` — `i.pximg.net` rejects image fetches
    /// without it.
    #[test]
    fn for_record_carries_pixiv_referer() {
        let settings = super::super::site_setting::SiteSetting::load_bundled(&[
            include_str!("../../webnovel/www.pixiv.net.yaml"),
        ])
        .unwrap();

        for toc_url in [
            "https://www.pixiv.net/novel/show.php?id=12345",
            "https://www.pixiv.net/ajax/novel/12345",
        ] {
            let mut record = pixiv_record();
            record.toc_url = toc_url.to_string();
            let policy = FetchPolicy::for_record(&settings, &record);
            let referer = policy
                .headers()
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("referer"))
                .map(|(_, value)| value.as_str());
            assert_eq!(
                referer,
                Some("https://www.pixiv.net/"),
                "site-defined Referer must ride on fetches for {toc_url}"
            );
        }

        let mut record = pixiv_record();
        record.toc_url = "https://unknown.example/work/1".to_string();
        assert!(FetchPolicy::for_record(&settings, &record).headers().is_empty());
    }


    fn pixiv_record() -> crate::db::NovelRecord {
        crate::db::NovelRecord {
            id: 1,
            author: String::new(),
            title: String::new(),
            file_title: String::new(),
            toc_url: String::new(),
            sitename: String::new(),
            novel_type: 1,
            end: false,
            last_update: chrono::Utc::now(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: Vec::new(),
            ncode: None,
            domain: None,
            general_all_no: None,
            length: None,
            suspend: false,
            is_narou: false,
            last_check_date: None,
            convert_failure: false,
            requires_login: false,
            login_session: None,
            extra_fields: Default::default(),
        }
    }

    #[test]
    fn headers_for_drops_the_cookie_outside_its_site() {
        let setting: super::super::site_setting::SiteSetting = serde_yaml::from_str(
            r#"
name: Example
domain: example.com
top_url: https://example.com
sitename: Example
toc_url: https://example.com/\k<ncode>
cookie: session=canary
cookie_hosts:
  - AUTHORS.example.net
  - ""
headers:
  Referer: https://example.com/
"#,
        )
        .unwrap();
        let policy = FetchPolicy::for_site(&setting);

        let on_site = policy.headers_for("https://img.example.com/a.png");
        assert!(
            on_site
                .iter()
                .any(|(name, value)| name == "Cookie" && value == "session=canary")
        );
        let author_site = policy.headers_for("https://authors.example.net/list");
        assert!(author_site.iter().any(|(name, value)| name == "Cookie" && value == "session=canary"));
        for url in [
            "https://collector.example.net/a.png",
            "https://authors.example.net.attacker.test/a.png",
            "https://collector.example.net./a.png",
        ] {
            let off_site = policy.headers_for(url);
            assert!(
                !off_site.iter().any(|(name, _)| name.eq_ignore_ascii_case("cookie")),
                "the cookie must stay on declared hosts: {url}: {off_site:?}"
            );
            assert!(off_site.iter().any(|(name, _)| name == "Referer"));
        }
    }

    #[test]
    fn fetch_policy_drops_headers_that_could_inject_a_second_request() {
        let setting: super::super::site_setting::SiteSetting = serde_yaml::from_str(
            r#"
name: Example
domain: example.com
top_url: https://example.com
sitename: Example
toc_url: https://example.com/\k<ncode>
headers:
  "X-Bad\r\nInjected": value
  Referer: "https://example.com/\r\nX-Injected: 1"
"#,
        )
        .unwrap();
        let policy = FetchPolicy::for_site(&setting);

        assert!(
            policy.headers().is_empty(),
            "unsafe headers should be dropped: {:?}",
            policy.headers()
        );
    }


    #[test]
    fn stop_fallback_on_definitive_errors() {
        assert!(should_stop_fetch_fallback(&NarouError::NotFound(
            "x".into()
        )));
        assert!(should_stop_fetch_fallback(&NarouError::SuspendDownload(
            "x".into()
        )));
        assert!(!should_stop_fetch_fallback(&NarouError::Http("x".into())));
    }

    struct StreamOnlyHttp {
        sent: parking_lot::Mutex<Vec<HttpRequest>>,
        polls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl StreamOnlyHttp {
        fn new() -> Self {
            Self {
                sent: Default::default(),
                polls: Default::default(),
                dropped: Default::default(),
            }
        }
    }

    struct PullBody {
        polls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl futures::Stream for PullBody {
        type Item = Result<Vec<u8>>;
        fn poll_next(self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
            use std::sync::atomic::Ordering;
            let item = match self.polls.fetch_add(1, Ordering::SeqCst) {
                0 => Some(Ok(b"first chunk".to_vec())),
                1 => Some(Err(NarouError::Http("connection interrupted".into()))),
                _ => None,
            };
            std::task::Poll::Ready(item)
        }
    }

    impl Drop for PullBody {
        fn drop(&mut self) {
            self.dropped.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl HttpClient for StreamOnlyHttp {
        fn send<'a>(&'a self, _request: HttpRequest) -> crate::platform::PlatformFuture<'a, Result<HttpResponse>> {
            panic!("binary fetch must never use buffered send");
        }
        fn send_stream<'a>(&'a self, request: HttpRequest) -> crate::platform::PlatformFuture<'a, Result<HttpStreamResponse>> {
            Box::pin(async move {
                let url = request.url.clone();
                self.sent.lock().push(request);
                let (status, headers) = match url.as_str() {
                    "https://example.com/start" => (302, vec![
                        ("Location".into(), "/next".into()),
                        ("Set-Cookie".into(), "session=updated; Path=/".into()),
                    ]),
                    "https://example.com/next" => (302, vec![("Location".into(), "https://cdn.example.net/image".into())]),
                    "https://example.com/unsafe" => (302, vec![("Location".into(), "http://127.0.0.1/private".into())]),
                    "https://example.com/missing" => (404, Vec::new()),
                    "https://example.com/limited" => (503, Vec::new()),
                    _ => (200, vec![("Content-Type".into(), "image/png".into())]),
                };
                let body: crate::platform::AssetStream = if status == 302 {
                    Box::pin(futures::stream::empty())
                } else {
                    Box::pin(PullBody { polls: self.polls.clone(), dropped: self.dropped.clone() })
                };
                Ok(HttpStreamResponse { status, url, headers, body })
            })
        }
    }

    fn session_policy() -> FetchPolicy {
        FetchPolicy {
            headers: vec![
                ("Cookie".into(), "session=initial".into()),
                ("Referer".into(), "https://example.com/".into()),
            ],
            cookie_hosts: vec!["example.com".into()],
            session: Some(std::sync::Arc::new(parking_lot::Mutex::new(session::SessionCookies::default()))),
            ..FetchPolicy::default()
        }
    }

    #[test]
    fn streamed_fetch_is_pull_driven_and_preserves_redirect_cookie_and_pacing_policy() {
        futures::executor::block_on(async {
            use futures::StreamExt;
            use std::sync::atomic::Ordering;
            let http = StreamOnlyHttp::new();
            let limiter = crate::platform::mocks::FakeRateLimiter::new();
            let mut response = fetch_stream(&http, &limiter, "https://example.com/start", &session_policy()).await.unwrap();
            assert_eq!(response.url, "https://cdn.example.net/image");
            assert_eq!(response.header("content-type"), Some("image/png"));
            assert_eq!(http.polls.load(Ordering::SeqCst), 0);
            assert_eq!(limiter.acquisition_count(), 3);
            {
                let requests = http.sent.lock();
                assert!(requests.iter().all(|request| request.redirect == RedirectMode::Manual));
                assert!(requests[1].headers.iter().any(|(name, value)| name == "Cookie" && value == "session=updated"));
                assert!(!requests[2].headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("Cookie")));
                assert!(requests[2].headers.iter().any(|(name, _)| name == "Referer"));
            }
            assert_eq!(response.body.next().await.unwrap().unwrap(), b"first chunk");
            assert!(response.body.next().await.unwrap().is_err(), "an interrupted binary stream is not EOF");
            drop(response);
            assert!(http.dropped.load(Ordering::SeqCst));
        });
    }

    #[test]
    fn streamed_fetch_rejects_unsafe_redirects_before_sending_and_does_not_read_errors() {
        futures::executor::block_on(async {
            use std::sync::atomic::Ordering;
            let http = StreamOnlyHttp::new();
            let limiter = crate::platform::mocks::FakeRateLimiter::new();
            assert!(fetch_stream(&http, &limiter, "https://example.com/unsafe", &session_policy()).await.is_err());
            assert_eq!(http.sent.lock().len(), 1);
            assert!(matches!(fetch_stream(&http, &limiter, "https://example.com/missing", &FetchPolicy::default()).await, Err(NarouError::NotFound(_))));
            assert!(matches!(fetch_stream(&http, &limiter, "https://example.com/limited", &FetchPolicy::default()).await, Err(NarouError::SuspendDownload(_))));
            assert_eq!(http.polls.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn dropping_streamed_response_stops_reads_without_draining() {
        futures::executor::block_on(async {
            use std::sync::atomic::Ordering;
            let http = StreamOnlyHttp::new();
            let limiter = crate::platform::mocks::FakeRateLimiter::new();
            let response = fetch_stream(&http, &limiter, "https://example.com/image", &FetchPolicy::default()).await.unwrap();
            drop(response);
            assert_eq!(http.polls.load(Ordering::SeqCst), 0);
            assert!(http.dropped.load(Ordering::SeqCst));
        });
    }
}

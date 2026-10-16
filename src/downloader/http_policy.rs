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

use crate::error::{NarouError, Result};
use crate::platform::http::{RedirectChain, is_safe_header_name, is_safe_header_value};
use crate::platform::{
    HttpClient, HttpRequest, HttpResponse, RateLimitScope, RateLimiter, RedirectMode,
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
        }
        headers
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
    match response.status {
        404 => Err(NarouError::NotFound(url.to_string())),
        503 => Err(NarouError::SuspendDownload("Rate limited (503)".into())),
        status if !(200..300).contains(&status) => Err(NarouError::Http(format!(
            "HTTP {status} while fetching {url}"
        ))),
        _ => Ok(response),
    }
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
        let response = http.send_paced(
            chain.request().clone().with_redirect(RedirectMode::Manual),
            crate::platform::http::HttpPacing { limiter: rate_limiter, scope: &scope },
        ).await?;
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
    http.validate_url(url).await?;

    let mut request = HttpRequest::get(url);
    for (name, value) in policy.headers_for(url) {
        request = request.with_header(name, value);
    }

    let scope = scope_for(host_of(url), policy);
    let response = http.send_paced(
        request, crate::platform::http::HttpPacing { limiter: rate_limiter, scope: &scope },
    ).await?;
    ensure_success_response(url, response)
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

}

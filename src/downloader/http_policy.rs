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
use crate::platform::{
    HttpClient, HttpRequest, HttpResponse, RateLimitScope, RateLimiter, RedirectMode,
};

use super::security::{MAX_REDIRECTS, is_safe_header_value};

/// Everything a site fetch needs beyond the URL: the site's static headers
/// (cookie jar value plus any `headers:` declared in the site definition) and
/// whether it uses the narou rate-limit scope.
///
/// Every outbound request for a site carries one of these, so the site
/// definition is the single place that decides what is sent.
#[derive(Debug, Clone, Default)]
pub struct FetchPolicy {
    headers: Vec<(String, String)>,
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
        Self {
            headers,
            narou: setting.is_narou,
            min_interval: setting.min_interval,
        }
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
}

/// Header names must be tokens; anything else (spaces, colons, CR/LF) is
/// rejected so a site definition cannot smuggle a second header.
fn is_safe_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    #[test]
    fn hameln_definition_carries_browser_fetch_metadata() {
        // R18 分離ドメインの Cloudflare challenge を避けるためのヘッダが
        // サイト定義から実際に policy へ流れていることを固定する。
        let setting: super::super::site_setting::SiteSetting = serde_yaml::from_str(
            include_str!("../../webnovel/syosetu.org.yaml"),
        )
        .unwrap();
        let policy = FetchPolicy::for_site(&setting);

        for name in [
            "Sec-Fetch-Dest",
            "Sec-Fetch-Mode",
            "Sec-Fetch-Site",
            "Sec-Fetch-User",
            "Upgrade-Insecure-Requests",
            "Accept-Language",
        ] {
            assert!(
                policy.headers().iter().any(|(key, _)| key == name),
                "missing {name} in {:?}",
                policy.headers()
            );
        }
        assert!(
            policy
                .headers()
                .iter()
                .any(|(key, value)| key == "Cookie" && value == "over18=off")
        );
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

/// Returns true when two hosts belong to the same site for cookie purposes:
/// identical, or one is a subdomain of the other (e.g. `h.syosetu.org` and
/// `syosetu.org`). Browsers share domain-scoped cookies (`Domain=.example.com`)
/// across such hosts, so redirects within one site must keep the configured
/// Cookie header while cross-site redirects still drop it.
pub fn same_site_hosts(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            a == b
                || a.strip_suffix(b)
                    .is_some_and(|prefix| prefix.ends_with('.'))
                || b.strip_suffix(a)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        }
        _ => false,
    }
}

/// Resolve the final URL of a redirect chain using `Manual` requests.
///
/// The transport returns 3xx responses untouched; this loop follows
/// `Location` headers, re-validates every hop against the public-URL policy,
/// and drops the Cookie header when a hop leaves the current site. The
/// transport may still apply its own enhancements (e.g. the native curl probe
/// for CDN challenges) inside `send(Manual)`.
pub async fn resolve_final_url(
    http: &dyn HttpClient,
    rate_limiter: &dyn RateLimiter,
    url: &str,
    policy: &FetchPolicy,
) -> Result<String> {
    Ok(
        resolve_final_url_with_body(http, rate_limiter, url, policy)
            .await?
            .0,
    )
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
    http.validate_url(url).await?;
    let mut current = url::Url::parse(url).map_err(|e| NarouError::Http(e.to_string()))?;
    let mut headers = policy.headers().to_vec();

    for hop in 0..=MAX_REDIRECTS {
        http.validate_url(current.as_str()).await?;
        rate_limiter
            .acquire(&scope_for(host_of(current.as_str()), policy))
            .await?;

        let mut request = HttpRequest::get(current.as_str()).with_redirect(RedirectMode::Manual);
        for (name, value) in &headers {
            request = request.with_header(name.clone(), value.clone());
        }

        let response = http.send(request).await?;
        if response.is_redirection() {
            let Some(location) = response.header("Location") else {
                return Ok((current.to_string(), None));
            };
            let next = current
                .join(location)
                .map_err(|e| NarouError::Http(format!("invalid redirect location: {e}")))?;
            if !same_site_hosts(next.host_str(), current.host_str()) {
                // A hop that leaves the site must not carry the cookie jar,
                // however the site definition spelled the header name.
                headers.retain(|(name, _)| !name.eq_ignore_ascii_case("Cookie"));
            }
            current = next;
            continue;
        }
        // Only the first hop's body is reusable: later hops may have run with
        // a different cookie scope than the caller's `fetch_text` would use.
        let body = (hop == 0).then_some(response);
        return Ok((current.to_string(), body));
    }

    Err(NarouError::Http(format!(
        "redirect limit exceeded for {url} after {} hops",
        MAX_REDIRECTS
    )))
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
    rate_limiter
        .acquire(&scope_for(host_of(url), policy))
        .await?;

    let mut request = HttpRequest::get(url);
    for (name, value) in policy.headers() {
        request = request.with_header(name.clone(), value.clone());
    }

    let response = http.send(request).await?;
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
    fn fetch_policy_drops_the_cookie_when_a_hop_leaves_the_site() {
        // サイト定義が小文字で Cookie を書いても、サイト外へのホップでは
        // クッキージャーを落とす。
        let setting: super::super::site_setting::SiteSetting = serde_yaml::from_str(
            r#"
name: Example
domain: example.com
top_url: https://example.com
sitename: Example
toc_url: https://example.com/\k<ncode>
headers:
  cookie: "over18=yes"
  Referer: https://example.com/
"#,
        )
        .unwrap();
        let policy = FetchPolicy::for_site(&setting);
        let mut headers = policy.headers().to_vec();

        assert!(
            headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("Cookie"))
        );
        headers.retain(|(name, _)| !name.eq_ignore_ascii_case("Cookie"));
        assert!(
            !headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("Cookie")),
            "cookie should be gone, headers: {headers:?}"
        );
        assert!(headers.iter().any(|(name, _)| name == "Referer"));
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

    #[test]
    fn same_site_hosts_keeps_cookie_within_subdomains() {
        assert!(same_site_hosts(Some("syosetu.org"), Some("syosetu.org")));
        assert!(same_site_hosts(Some("h.syosetu.org"), Some("syosetu.org")));
        assert!(same_site_hosts(Some("syosetu.org"), Some("h.syosetu.org")));
        assert!(!same_site_hosts(Some("hsyosetu.org"), Some("syosetu.org")));
        assert!(!same_site_hosts(Some("syosetu.org"), Some("example.com")));
        assert!(!same_site_hosts(Some("syosetu.org"), None));
    }
}

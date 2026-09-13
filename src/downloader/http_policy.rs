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

use super::security::{MAX_REDIRECTS, is_safe_header_value, validate_public_url};

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
    cookie: Option<&str>,
    narou: bool,
) -> Result<String> {
    Ok(
        resolve_final_url_with_body(http, rate_limiter, url, cookie, narou)
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
    cookie: Option<&str>,
    narou: bool,
) -> Result<(String, Option<HttpResponse>)> {
    validate_public_url(url).map_err(|e| NarouError::Http(e.to_string()))?;
    let mut current = url::Url::parse(url).map_err(|e| NarouError::Http(e.to_string()))?;
    let mut current_cookie = cookie.map(ToString::to_string);

    for hop in 0..=MAX_REDIRECTS {
        validate_public_url(current.as_str()).map_err(|e| NarouError::Http(e.to_string()))?;
        rate_limiter
            .acquire(&scope_for(host_of(current.as_str()), narou))
            .await?;

        let mut request = HttpRequest::get(current.as_str()).with_redirect(RedirectMode::Manual);
        if let Some(cookie) = current_cookie.as_deref() {
            if !is_safe_header_value(cookie) {
                return Err(NarouError::Http("unsafe Cookie header value".into()));
            }
            request = request.with_header("Cookie", cookie);
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
                current_cookie = None;
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
    cookie: Option<&str>,
    encoding: Option<&str>,
    narou: bool,
) -> Result<String> {
    let response = fetch_bytes(http, rate_limiter, url, cookie, narou).await?;
    Ok(decode_with_encoding(&response.body, encoding))
}

/// Standard GET flow returning raw bytes plus the content type.
pub async fn fetch_bytes(
    http: &dyn HttpClient,
    rate_limiter: &dyn RateLimiter,
    url: &str,
    cookie: Option<&str>,
    narou: bool,
) -> Result<HttpResponse> {
    validate_public_url(url).map_err(|e| NarouError::Http(e.to_string()))?;
    rate_limiter
        .acquire(&scope_for(host_of(url), narou))
        .await?;

    let mut request = HttpRequest::get(url);
    if let Some(cookie) = cookie {
        if !is_safe_header_value(cookie) {
            return Err(NarouError::Http("unsafe Cookie header value".into()));
        }
        request = request.with_header("Cookie", cookie);
    }

    let response = http.send(request).await?;
    ensure_success_response(url, response)
}

fn scope_for(host: String, narou: bool) -> RateLimitScope {
    if narou {
        RateLimitScope::narou(host)
    } else {
        RateLimitScope::site(host)
    }
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

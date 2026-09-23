//! Persisted login cookies.
//!
//! Sites that need authentication are handled as a *fallback*: the downloader
//! sends no login cookie by default, and retries with the stored cookie only
//! when a fetch failed (or returned a login wall). Novels that turned out to
//! need the cookie are marked, so later runs send it up front for those alone.
//!
//! Entries are keyed by request host. The native HTTP client refreshes them
//! from `Set-Cookie` responses so a session survives across runs; the login
//! executable writes the initial value after the user signs in.

use std::collections::BTreeMap;

use crate::error::Result;
use crate::platform::PlatformFuture;

/// Cookie persistence boundary.
pub trait CookieStore: Send + Sync {
    /// Stored `Cookie:` header value for `host`, when present.
    fn load<'a>(&'a self, host: &'a str) -> PlatformFuture<'a, Result<Option<String>>>;
    /// Replace the stored cookies for `host`.
    fn save<'a>(&'a self, host: &'a str, cookie: &'a str) -> PlatformFuture<'a, Result<()>>;
    /// Drop the stored cookies for `host`.
    fn clear<'a>(&'a self, host: &'a str) -> PlatformFuture<'a, Result<()>>;
    /// Every stored `host → cookie` pair, ordered by host.
    fn list(&self) -> PlatformFuture<'_, Result<BTreeMap<String, String>>>;
}

/// Canonical form of a request host used as a credential key.
///
/// Hosts are matched case-insensitively and never carry surrounding space, so
/// `Ncode.Syosetu.com ` and `ncode.syosetu.com` address the same entry.
pub fn normalize_cookie_host(host: &str) -> String {
    host.trim().to_ascii_lowercase()
}

/// Parse a `Cookie:` header value into `name=value` pairs, preserving order.
///
/// Attributes (`Path`, `Domain`, …) are not part of a request header, so a
/// value that carries them is trimmed down to the pairs before the first
/// attribute.
pub fn parse_cookie_header(header: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for part in header.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let Some((name, value)) = part.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || is_cookie_attribute(name) {
            break;
        }
        pairs.push((name.to_string(), value.trim().to_string()));
    }
    pairs
}

fn is_cookie_attribute(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "path" | "domain" | "expires" | "max-age" | "samesite" | "secure" | "httponly" | "version"
    )
}

/// Render pairs back into a `Cookie:` header value.
pub fn format_cookie_header(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Merge the site's static cookie with the stored login cookie.
///
/// The static value carries per-site consent (`over18=yes`) and must survive;
/// the login cookie wins when both define the same name.
pub fn merge_cookie_headers(
    static_cookie: Option<&str>,
    login_cookie: Option<&str>,
) -> Option<String> {
    let mut merged = static_cookie.map(parse_cookie_header).unwrap_or_default();
    if let Some(login) = login_cookie {
        for (name, value) in parse_cookie_header(login) {
            match merged.iter_mut().find(|(existing, _)| *existing == name) {
                Some(entry) => entry.1 = value,
                None => merged.push((name, value)),
            }
        }
    }
    if merged.is_empty() {
        None
    } else {
        Some(format_cookie_header(&merged))
    }
}

/// Apply `Set-Cookie` response headers to a stored cookie value.
///
/// Known names are replaced, new names are appended (a session may gain a
/// cookie after login), and an expired name is removed so signing out clears
/// the stored session.
pub fn apply_set_cookie(stored: &str, set_cookie_values: &[String]) -> String {
    let mut pairs = parse_cookie_header(stored);
    for value in set_cookie_values {
        let Some((name, rest)) = value.split_once('=') else {
            continue;
        };
        let name = name.trim().to_string();
        if name.is_empty() {
            continue;
        }
        let new_value = rest.split(';').next().unwrap_or_default().trim().to_string();
        if set_cookie_expires_immediately(rest) || new_value.is_empty() {
            pairs.retain(|(existing, _)| *existing != name);
            continue;
        }
        match pairs.iter_mut().find(|(existing, _)| *existing == name) {
            Some(entry) => entry.1 = new_value,
            None => pairs.push((name, new_value)),
        }
    }
    format_cookie_header(&pairs)
}

fn set_cookie_expires_immediately(attributes: &str) -> bool {
    for attribute in attributes.split(';').skip(1) {
        let attribute = attribute.trim();
        let Some((name, value)) = attribute.split_once('=') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "max-age" => {
                if value.trim().parse::<i64>().is_ok_and(|seconds| seconds <= 0) {
                    return true;
                }
            }
            // `Expires=Thu, 01 Jan 1970 00:00:00 GMT` and older dates clear it.
            "expires" => {
                let value = value.trim();
                if value.starts_with("Thu, 01 Jan 1970") || value.starts_with("Thu, 01-Jan-1970") {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Keys to consult for a request host, most specific first.
///
/// A cookie set for `.example.com` is sent to `example.com` and to every one of
/// its subdomains, so a lookup for `www.example.com` must also see the entry
/// stored under `example.com`. Candidates keep at least two labels, so a bare
/// public suffix is never consulted.
pub fn cookie_lookup_hosts(host: &str) -> Vec<String> {
    let host = normalize_cookie_host(host);
    let mut keys = vec![host.clone()];
    let mut rest = host.as_str();
    while let Some((_, parent)) = rest.split_once('.') {
        if parent.split('.').count() < 2 {
            break;
        }
        keys.push(parent.to_string());
        rest = parent;
    }
    keys
}

/// Merge stored cookie values, most specific first.
///
/// A name defined by several keys keeps the value of the most specific one,
/// which is what a browser sends (`www.example.com` beats `.example.com`).
pub fn merge_stored_cookies<'a>(values: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for value in values {
        for (name, value) in parse_cookie_header(value) {
            if pairs.iter().any(|(existing, _)| *existing == name) {
                continue;
            }
            pairs.push((name, value));
        }
    }
    (!pairs.is_empty()).then(|| format_cookie_header(&pairs))
}

/// Host used as the cookie key for a URL, without a port.
pub fn cookie_host_for_url(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.rsplit_once('@').map(|(_, host)| host).unwrap_or(host);
    let host = host.split_once(':').map(|(host, _)| host).unwrap_or(host);
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_hosts_walk_up_to_the_parent_domain() {
        assert_eq!(
            cookie_lookup_hosts("www.pixiv.net"),
            vec!["www.pixiv.net", "pixiv.net"]
        );
        assert_eq!(
            cookie_lookup_hosts(" Ncode.Syosetu.Com "),
            vec!["ncode.syosetu.com", "syosetu.com"]
        );
        // A bare public suffix is never a candidate.
        assert_eq!(cookie_lookup_hosts("example.com"), vec!["example.com"]);
        assert_eq!(cookie_lookup_hosts("localhost"), vec!["localhost"]);
    }

    #[test]
    fn merge_keeps_the_most_specific_value() {
        let merged = merge_stored_cookies(["PHPSESSID=sub; a=1", "PHPSESSID=parent; b=2"]).unwrap();
        assert_eq!(merged, "PHPSESSID=sub; a=1; b=2");
        assert_eq!(merge_stored_cookies(Vec::<&str>::new()), None);
        assert_eq!(merge_stored_cookies(["", "; "]), None);
    }

    #[test]
    fn merge_keeps_static_consent_and_prefers_login_values() {
        assert_eq!(
            merge_cookie_headers(Some("over18=yes"), Some("session=abc")).as_deref(),
            Some("over18=yes; session=abc")
        );
        assert_eq!(
            merge_cookie_headers(Some("over18=off"), Some("over18=yes; session=abc")).as_deref(),
            Some("over18=yes; session=abc")
        );
        assert_eq!(merge_cookie_headers(None, None), None);
        assert_eq!(merge_cookie_headers(Some("over18=yes"), None).as_deref(), Some("over18=yes"));
    }

    #[test]
    fn set_cookie_replaces_adds_and_removes_names() {
        let updated = apply_set_cookie(
            "session=old; keep=1",
            &[
                "session=new; Path=/; HttpOnly".to_string(),
                "fresh=2; Path=/".to_string(),
                "keep=; Max-Age=0; Path=/".to_string(),
            ],
        );
        assert_eq!(updated, "session=new; fresh=2");
    }

    #[test]
    fn cookie_host_ignores_port_and_credentials() {
        assert_eq!(
            cookie_host_for_url("https://user@Example.COM:8443/path?q=1").as_deref(),
            Some("example.com")
        );
        assert_eq!(cookie_host_for_url("not a url"), None);
    }

    #[test]
    fn parse_ignores_attributes_and_format_round_trips() {
        let pairs = parse_cookie_header("a=1; b=2; Path=/; HttpOnly");
        assert_eq!(pairs, vec![("a".into(), "1".into()), ("b".into(), "2".into())]);
        assert_eq!(format_cookie_header(&pairs), "a=1; b=2");
    }
}

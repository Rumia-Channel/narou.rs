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

use crate::login::KEY_LEN;

use serde::{Deserialize, Serialize};

use crate::error::{NarouError, Result};
use crate::platform::PlatformFuture;

/// One stored login credential (a `Cookie:` header captured for a site).
///
/// A site may hold several, and their order *is* the order the downloader
/// tries them in: the first entry is sent when a novel is already known to
/// need a login, and the rest are tried when a fetch still looks blocked or
/// incomplete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginCredential {
    /// Identifier this credential is remembered by. A novel records the id of
    /// the credential that made its fetch work, so later runs can send that
    /// one straight away instead of walking the list. Empty on a value written
    /// before ids existed; the store fills one in and saves it back.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Host the cookies were captured from; also where `Set-Cookie` updates
    /// are written back.
    pub host: String,
    /// `Cookie:` header value.
    pub cookie: String,
    /// Optional label shown in the CLI and Web UI ("メイン", "R18用", …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// When the credential was captured or imported (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_at: Option<String>,
}

impl LoginCredential {
    pub fn new(host: impl Into<String>, cookie: impl Into<String>) -> Self {
        Self {
            id: String::new(),
            host: normalize_cookie_host(&host.into()),
            cookie: cookie.into(),
            label: None,
            added_at: None,
        }
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    /// Short form for the CLI and Web UI (the full id stays in storage).
    pub fn short_id(&self) -> &str {
        let end = self.id.len().min(8);
        &self.id[..end]
    }

    pub fn with_label(mut self, label: Option<String>) -> Self {
        self.label = label.filter(|label| !label.trim().is_empty());
        self
    }

    pub fn with_added_at(mut self, added_at: Option<String>) -> Self {
        self.added_at = added_at;
        self
    }

    /// Label for display, falling back to the host.
    pub fn display_name(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.host)
    }

    /// Whether two entries carry the same session (the order list treats a
    /// repeated value as the same credential).
    pub fn same_cookie(&self, other: &Self) -> bool {
        self.cookie == other.cookie
    }
}

/// Encode credentials for inventory storage, which only holds strings.
pub fn encode_credentials(credentials: &[LoginCredential]) -> Result<String> {
    serde_json::to_string(credentials)
        .map_err(|error| crate::error::NarouError::Login(format!("資格情報を保存できません: {error}")))
}

/// Decode a stored value.
///
/// Values written before a site could hold several credentials are a bare
/// `Cookie:` header; those are read as a single entry so an older library keeps
/// working (and is upgraded on the next write).
pub fn decode_credentials(value: &str, host: &str) -> Vec<LoginCredential> {
    let trimmed = value.trim();
    if trimmed.starts_with('[') {
        if let Ok(credentials) = serde_json::from_str::<Vec<LoginCredential>>(trimmed) {
            return credentials;
        }
    }
    if trimmed.is_empty() {
        return Vec::new();
    }
    vec![LoginCredential::new(host, trimmed)]
}

/// Cookie persistence boundary.
pub trait CookieStore: Send + Sync {
    /// Credentials stored for `host`, in the order they should be tried.
    fn load_all<'a>(&'a self, host: &'a str) -> PlatformFuture<'a, Result<Vec<LoginCredential>>>;
    /// Replace every credential for `host` (an empty slice removes the entry).
    fn save_all<'a>(
        &'a self,
        host: &'a str,
        credentials: &'a [LoginCredential],
    ) -> PlatformFuture<'a, Result<()>>;
    /// Drop the stored credentials for `host`.
    fn clear<'a>(&'a self, host: &'a str) -> PlatformFuture<'a, Result<()>>;
    /// Every stored host with its ordered credentials.
    fn list(&self) -> PlatformFuture<'_, Result<BTreeMap<String, Vec<LoginCredential>>>>;
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

/// Give every credential an id, returning whether anything changed.
///
/// Ids are generated on platforms that have a random source; a platform
/// without one (wasm) keeps whatever id came with the data, which the
/// downloader tolerates (it falls back to the first entry).
pub fn assign_credential_ids(stored: &mut BTreeMap<String, Vec<LoginCredential>>) -> bool {
    #[cfg(not(feature = "native-runtime"))]
    {
        let _ = stored;
        false
    }
    #[cfg(feature = "native-runtime")]
    {
        let mut changed = false;
        for credentials in stored.values_mut() {
            for credential in credentials.iter_mut() {
                if credential.id.is_empty()
                    && let Ok(id) = crate::login::new_credential_id()
                {
                    credential.id = id;
                    changed = true;
                }
            }
        }
        changed
    }
}

/// Normalize a credential before it is stored: surrounding space is never
/// meaningful in a `Cookie:` header, and an empty one is not a credential.
pub fn tidy_credentials(credentials: &[LoginCredential]) -> Vec<LoginCredential> {
    credentials
        .iter()
        .filter_map(|credential| {
            let cookie = credential.cookie.trim();
            if cookie.is_empty() {
                return None;
            }
            let mut credential = credential.clone();
            credential.cookie = cookie.to_string();
            #[cfg(feature = "native-runtime")]
            if credential.id.is_empty()
                && let Ok(id) = crate::login::new_credential_id()
            {
                credential.id = id;
            }
            Some(credential)
        })
        .collect()
}

/// 親ドメインも含めたキーから、そのホストで使える資格情報を組み立てる。
pub fn merge_credentials_for(
    stored: &BTreeMap<String, Vec<LoginCredential>>,
    host: &str,
) -> Vec<LoginCredential> {
    let mut credentials: Vec<LoginCredential> = Vec::new();
    for key in cookie_lookup_hosts(host) {
        let Some(entries) = stored.get(&key) else {
            continue;
        };
        for credential in entries {
            if !credentials.iter().any(|seen| seen.same_cookie(credential)) {
                credentials.push(credential.clone());
            }
        }
    }
    credentials
}

/// Decode the at-rest payload of one host map (`value_yaml` of the
/// `login_cookie` inventory row) into credentials per host.
///
/// Values are either `enc:v1:<nonce>:<payload>` (needs `key`) or plaintext
/// written by an older build. Hosts are normalized and entries that decode to
/// nothing are dropped, mirroring the native store.
pub fn decode_stored_credentials(
    payload: &str,
    key: Option<&[u8; KEY_LEN]>,
) -> Result<BTreeMap<String, Vec<LoginCredential>>> {
    if payload.trim().is_empty() || payload.trim() == "{}" {
        return Ok(BTreeMap::new());
    }
    let raw: BTreeMap<String, String> = serde_yaml::from_str(payload)
        .map_err(|error| NarouError::Platform(format!("malformed stored credentials: {error}")))?;
    let mut stored = BTreeMap::new();
    for (host, value) in raw {
        let host = normalize_cookie_host(&host);
        let plain = match crate::login::decrypt_stored_value(key, &host, &value)? {
            Some(plain) => plain,
            None => value,
        };
        let credentials = decode_credentials(&plain, &host);
        if !credentials.is_empty() {
            stored.insert(host, credentials);
        }
    }
    Ok(stored)
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
    #[test]
    fn decodes_a_pinned_at_rest_payload() {
        // native が書いた値 (固定ベクタ) をそのまま読めること = 保存形式の互換。
        let payload = "novel18.syosetu.com: enc:v1:/cz1hgNv95D2AzmkgLEX7Wo0h0k0Ha+b:ffxRux2SuqGy4+2eDjupAP/AY4b3Go41UeMvY6aPvSEwCu21sRljTXThhmHfqN87c6+TwQjUqYivE9CBoVHzvg8LaPJhwHEdR1fUX1GDYiS4PdPn1YDmEPhd9HockmAVcdN9iqr7WLqA85pY9mIhqBvQySz3iVLVqhJMMrbnLsYZm0gEEnnljuI=\n";
        let key = [7u8; 32];
        let stored = decode_stored_credentials(payload, Some(&key)).unwrap();
        let credentials = stored.get("novel18.syosetu.com").expect("host entry");
        assert_eq!(credentials.len(), 1);
        assert_eq!(credentials[0].cookie, "over18=yes;");
        assert_eq!(credentials[0].label.as_deref(), Some("サイト A"));
        assert_eq!(credentials[0].id, "11111111-2222-4333-8444-555555555555");
        assert_eq!(credentials[0].host, "novel18.syosetu.com");

        // 鍵が無いときは黙って空にせずエラーにする。
        assert!(decode_stored_credentials(payload, None).is_err());
    }

    #[test]
    fn decodes_legacy_plaintext_and_merges_parent_domains() {
        let payload = "syosetu.com: \"over18=yes;\"\nnovel18.syosetu.com: \"over18=yes;\"\n";
        let stored = decode_stored_credentials(payload, None).unwrap();
        assert_eq!(stored.len(), 2);
        // 親ドメインの値も引ける (同じ Cookie は 1 件に畳まれる)。
        let merged = merge_credentials_for(&stored, "novel18.syosetu.com");
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].cookie, "over18=yes;");
    }

}

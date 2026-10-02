//! Persisted login cookies.
//!
//! Sites that need authentication are handled as a *fallback*: the downloader
//! sends no login cookie by default, and retries with the stored cookie only
//! when a fetch failed (or returned a login wall). Novels that turned out to
//! need the cookie are marked, so later runs send it up front for those alone.
//!
//! Entries are keyed by site. A login keeps the cookies of every host of that
//! site together and sends their union; the native HTTP client refreshes the
//! hosts that were actually sent from `Set-Cookie` responses so a session
//! survives across runs. The login executable writes the initial value after
//! the user signs in.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::platform::PlatformFuture;

/// One host's cookies inside a login.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCookie {
    /// Host the cookies were captured from (and where `Set-Cookie` writes back).
    pub host: String,
    /// `Cookie:` header value for that host.
    pub cookie: String,
}

/// One login: which site it belongs to, what the user calls it, and the
/// cookies it carries.
///
/// A browser session is not one cookie string but several, one per host the
/// site uses (Pixiv sets `.pixiv.net`, `www.pixiv.net` and
/// `accounts.pixiv.net`), so a login keeps them together and sends the union.
/// A site may hold several logins; their order is the order they are tried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginGroup {
    /// Identifier this login is remembered by. A novel records the id of the
    /// login that made its fetch work. Empty on a value written before ids
    /// existed; the store fills one in and saves it back.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Site the login belongs to — the site definition's domain
    /// (`www.pixiv.net`), used to group the hosts of one session.
    pub site: String,
    /// Name the user gave it ("メインアカウント", "Pixiv1", …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Cookies of this login, one entry per host.
    #[serde(default)]
    pub cookies: Vec<HostCookie>,
    /// When the login was captured or imported (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_at: Option<String>,
}

impl LoginGroup {
    pub fn new(site: impl Into<String>, cookies: Vec<HostCookie>) -> Self {
        Self {
            id: String::new(),
            site: site.into(),
            label: None,
            cookies,
            added_at: None,
        }
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    /// Short form for the CLI and Web UI (the full id stays in storage).
    ///
    /// The id is imported data, so the prefix is cut at a *character* boundary:
    /// slicing by byte length would panic on a non-ASCII id.
    pub fn short_id(&self) -> &str {
        let end = self.id.char_indices().nth(8).map_or(self.id.len(), |(index, _)| index);
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

    /// Name shown in the CLI and Web UI, falling back to the site.
    pub fn display_name(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.site)
    }

    /// Whether two logins carry the same cookies (the list treats a repeated
    /// value as the same login).
    pub fn same_cookies(&self, other: &Self) -> bool {
        self.cookies == other.cookies
    }

    /// Hosts of this login, most specific first.
    pub fn hosts(&self) -> Vec<&str> {
        let mut hosts: Vec<&str> = self.cookies.iter().map(|entry| entry.host.as_str()).collect();
        hosts.sort_by_key(|host| std::cmp::Reverse(host.matches('.').count()));
        hosts
    }

    /// `Cookie:` header to send for this login: every host's cookies, with the
    /// most specific host winning a name it shares with a broader one.
    pub fn merged_cookie(&self) -> String {
        let mut pairs: Vec<(String, String)> = Vec::new();
        for host in self.hosts() {
            let Some(entry) = self.cookies.iter().find(|entry| entry.host == host) else {
                continue;
            };
            for (name, value) in parse_cookie_header(&entry.cookie) {
                match pairs.iter_mut().find(|(existing, _)| *existing == name) {
                    Some(existing) => existing.1 = value,
                    None => pairs.push((name, value)),
                }
            }
        }
        format_cookie_header(&pairs)
    }

    /// Host whose cookies are part of `sent`, which is where a `Set-Cookie`
    /// response came from.
    pub fn host_for_sent(&self, sent: &[(String, String)]) -> Option<&str> {
        self.cookies
            .iter()
            .filter(|entry| {
                let pairs = parse_cookie_header(&entry.cookie);
                !pairs.is_empty()
                    && pairs.iter().all(|(name, value)| {
                        sent.iter()
                            .any(|(sent_name, sent_value)| sent_name == name && sent_value == value)
                    })
            })
            .map(|entry| entry.host.as_str())
            .next()
    }
}

/// Encode login groups for inventory storage, which only holds strings.
pub fn encode_groups(groups: &[LoginGroup]) -> Result<String> {
    serde_json::to_string(groups)
        .map_err(|error| crate::error::NarouError::Login(format!("ログイン情報を保存できません: {error}")))
}

/// Decode a stored value.
///
/// A value written by an older build is a list of per-host credentials; those
/// are folded into one login per site, which is what the newer shape expects.
pub fn decode_groups(value: &str, site: &str) -> Result<DecodedGroups> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(DecodedGroups::Current(Vec::new()));
    }
    if trimmed.starts_with('[') {
        if let Ok(groups) = serde_json::from_str::<Vec<LoginGroup>>(trimmed) {
            return Ok(DecodedGroups::Current(groups));
        }
        if let Ok(legacy) = serde_json::from_str::<Vec<LegacyCredential>>(trimmed) {
            return Ok(DecodedGroups::PerHost(
                legacy
                    .into_iter()
                    .map(|old| old.into_group(site))
                    .collect(),
            ));
        }
    }
    Ok(DecodedGroups::Current(Vec::new()))
}

/// What a stored value turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodedGroups {
    /// Current form: logins already grouped per site.
    Current(Vec<LoginGroup>),
    /// Version 2 form: one credential per host, read to migrate it.
    PerHost(Vec<LoginGroup>),
}

impl DecodedGroups {
    pub fn into_groups(self) -> Vec<LoginGroup> {
        match self {
            DecodedGroups::Current(groups) | DecodedGroups::PerHost(groups) => groups,
        }
    }
}

/// Fold per-host credentials into one login per account.
///
/// Version 2 kept a list per host, and the same position in every list was the
/// same account. Folding by position restores one login carrying every host's
/// cookies, which is what a browser sends.
pub fn fold_per_host_lists(lists: Vec<Vec<LoginGroup>>, site: &str) -> Vec<LoginGroup> {
    let mut merged: Vec<LoginGroup> = Vec::new();
    for list in lists {
        for (index, group) in list.into_iter().enumerate() {
            let mut group = group;
            group.site = site.to_string();
            group.cookies.retain(|entry| !entry.cookie.trim().is_empty());
            if group.cookies.is_empty() {
                continue;
            }
            if index >= merged.len() {
                merged.push(group);
                continue;
            }
            let existing = &mut merged[index];
            for entry in group.cookies {
                match existing
                    .cookies
                    .iter_mut()
                    .find(|current| current.host == entry.host)
                {
                    Some(current) => current.cookie = entry.cookie,
                    None => existing.cookies.push(entry),
                }
            }
            if existing.label.is_none() {
                existing.label = group.label;
            }
            if existing.id.is_empty() {
                existing.id = group.id;
            }
            if existing.added_at.is_none() {
                existing.added_at = group.added_at;
            }
        }
    }
    merged
}

/// The per-host credential a previous build wrote, read only to migrate it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyCredential {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub cookie: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub added_at: Option<String>,
}

impl LegacyCredential {
    pub fn into_group(self, site: &str) -> LoginGroup {
        let host = if self.host.is_empty() {
            site.to_string()
        } else {
            self.host
        };
        LoginGroup {
            id: self.id,
            site: site.to_string(),
            label: self.label,
            cookies: vec![HostCookie {
                host,
                cookie: self.cookie,
            }],
            added_at: self.added_at,
        }
    }
}

/// The site a host belongs to: the site definition that covers it, or the
/// registrable-looking parent domain when no definition matches.
///
/// A parent-domain key (`pixiv.net`) resolves to the definition below it
/// (`www.pixiv.net`) so one browser session stays in one place.
pub fn site_for_host(host: &str) -> String {
    let host = host.trim().to_ascii_lowercase();
    // The Worker cannot read the site definitions from disk; callers with an
    // in-memory list (bundled definitions) use `site_for_host_with`.
    let settings = crate::downloader::site_setting::SiteSetting::load_all()
        .unwrap_or_default();
    site_for_host_with(&settings, &host)
}

/// `site_for_host` over a caller-supplied definition list. Empty behaves like
/// an unloadable set: the registrable-looking parent domain.
pub fn site_for_host_with(
    settings: &[crate::downloader::site_setting::SiteSetting],
    host: &str,
) -> String {
    let host = host.trim().to_ascii_lowercase();
    let domain_of = |setting: &crate::downloader::site_setting::SiteSetting| {
        setting.domain.to_ascii_lowercase()
    };
    if settings.is_empty() {
        return parent_domain(&host);
    }
    // 1. 定義そのもの、その配下、またはその定義が使うホスト。
    if let Some(setting) = settings.iter().find(|setting| {
        let domain = domain_of(setting);
        domain == host
            || host.ends_with(&format!(".{domain}"))
            || domain.ends_with(&format!(".{host}"))
            || crate::platform::cookie_host_for_url(&setting.top_url())
                .is_some_and(|top| top == host || host.ends_with(&format!(".{top}")))
    }) {
        return domain_of(setting);
    }
    // 2. 同じ登録ドメインの兄弟サブドメイン (`accounts.pixiv.net` など)。
    //    定義が 1 つに定まるときだけ寄せる (なろうの ncode / novel18 は別サイト)。
    let base = parent_domain(&host);
    let mut siblings = settings
        .iter()
        .filter(|setting| parent_domain(&domain_of(setting)) == base)
        .map(domain_of);
    if let Some(domain) = siblings.next()
        && siblings.next().is_none()
    {
        return domain;
    }
    base
}

/// Last two labels: what a host's registrable domain looks like.
fn parent_domain(host: &str) -> String {
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() > 2 {
        labels[labels.len() - 2..].join(".")
    } else {
        host.to_string()
    }
}

/// Whether two logins look like two halves of one browser session.
///
/// Sessions of a site span several hosts, so a per-host capture leaves logins
/// whose hosts do not overlap. Cookies of different accounts usually share
/// names (`PHPSESSID`, …), which keeps those apart.
fn fragments_of_one_session(a: &LoginGroup, b: &LoginGroup) -> bool {
    let hosts = |group: &LoginGroup| -> BTreeSet<String> {
        group
            .cookies
            .iter()
            .map(|entry| entry.host.clone())
            .collect()
    };
    if !hosts(a).is_disjoint(&hosts(b)) {
        return false;
    }
    let cookie_names = |group: &LoginGroup| -> BTreeSet<String> {
        group
            .cookies
            .iter()
            .flat_map(|entry| {
                parse_cookie_header(&entry.cookie)
                    .into_iter()
                    .map(|(name, _)| name)
            })
            .collect()
    };
    let a_names = cookie_names(a);
    !a_names.is_empty() && a_names.is_disjoint(&cookie_names(b))
}

impl LoginGroup {
    /// Fold the hosts of the same site into one login.
    ///
    /// Used when an import (or a migrated value) carries one entry per host:
    /// they are one session and belong together.
    pub fn merge_by_site(groups: Vec<LoginGroup>, site: &str) -> Vec<LoginGroup> {
        let mut merged: Vec<LoginGroup> = Vec::new();
        for group in groups {
            let mut group = group;
            group.site = site.to_string();
            group.cookies.retain(|entry| !entry.cookie.trim().is_empty());
            if group.cookies.is_empty() {
                continue;
            }
            match merged.iter_mut().find(|existing| existing.same_cookies(&group)) {
                Some(existing) => {
                    for entry in group.cookies {
                        if let Some(found) = existing.cookies.iter_mut().find(|it| it.host == entry.host)
                        {
                            found.cookie = entry.cookie;
                        } else {
                            existing.cookies.push(entry);
                        }
                    }
                    if existing.label.is_none() {
                        existing.label = group.label;
                    }
                    if existing.id.is_empty() {
                        existing.id = group.id;
                    }
                }
                None => merged.push(group),
            }
        }
        // 1 回の取得が複数のログインに分かれていたものを戻す。
        Self::fold_session_fragments(merged, site)
    }

    /// Fold logins that are fragments of one browser session.
    ///
    /// A previous build wrote one entry per host, so a single capture arrived
    /// as several logins: their host sets are disjoint and no cookie name
    /// collides. Those are one session — the browser sends every host's
    /// cookies together — so they are merged into the first of them.
    pub fn fold_session_fragments(groups: Vec<LoginGroup>, site: &str) -> Vec<LoginGroup> {
        let mut folded: Vec<LoginGroup> = Vec::new();
        for group in groups {
            let mut group = group;
            group.site = site.to_string();
            group.cookies.retain(|entry| !entry.cookie.trim().is_empty());
            if group.cookies.is_empty() {
                continue;
            }
            match folded
                .iter_mut()
                .find(|existing| fragments_of_one_session(existing, &group))
            {
                Some(existing) => {
                    for entry in group.cookies {
                        match existing
                            .cookies
                            .iter_mut()
                            .find(|current| current.host == entry.host)
                        {
                            Some(current) => current.cookie = entry.cookie,
                            None => existing.cookies.push(entry),
                        }
                    }
                    if existing.label.is_none() {
                        existing.label = group.label;
                    }
                    if existing.id.is_empty() {
                        existing.id = group.id;
                    }
                    if existing.added_at.is_none() {
                        existing.added_at = group.added_at;
                    }
                }
                None => folded.push(group),
            }
        }
        folded
    }
}

/// Give every login an id, returning whether anything changed.
///
/// Ids are generated on platforms that have a random source; a platform
/// without one keeps whatever id came with the data, which the downloader
/// tolerates (it falls back to the first entry). A non-ASCII id is dropped
/// back to empty and re-issued where possible — a stored value can carry one
/// from a hand-edited file or an unchecked import.
pub fn assign_group_ids(stored: &mut BTreeMap<String, Vec<LoginGroup>>) -> bool {
    let mut changed = false;
    for groups in stored.values_mut() {
        for group in groups.iter_mut() {
            if !group.id.is_empty() && !group.id.is_ascii() {
                group.id.clear();
                changed = true;
            }
            if group.id.is_empty()
                && let Ok(id) = crate::login::new_credential_id()
            {
                group.id = id;
                changed = true;
            }
        }
    }
    changed
}

/// Normalize logins before they are stored: trim cookies, drop empty ones,
/// assign ids and make sure each carries its site.
pub fn tidy_groups(groups: &[LoginGroup], site: &str) -> Vec<LoginGroup> {
    let mut tidied: Vec<LoginGroup> = Vec::new();
    for group in groups {
        let mut group = group.clone();
        group.site = site.to_string();
        group.cookies = group
            .cookies
            .iter()
            .filter_map(|entry| {
                let cookie = entry.cookie.trim();
                if cookie.is_empty() {
                    return None;
                }
                Some(HostCookie {
                    host: normalize_cookie_host(&entry.host),
                    cookie: cookie.to_string(),
                })
            })
            .collect();
        if group.cookies.is_empty() {
            continue;
        }
        if group.id.is_empty()
            && let Ok(id) = crate::login::new_credential_id()
        {
            group.id = id;
        }
        if tidied.iter().any(|existing| existing.same_cookies(&group)) {
            continue;
        }
        tidied.push(group);
    }
    tidied
}


/// Cookie persistence boundary.
pub trait CookieStore: Send + Sync {
    /// Logins registered for `site`, in the order they should be tried.
    fn load_groups<'a>(&'a self, site: &'a str) -> PlatformFuture<'a, Result<Vec<LoginGroup>>>;
    /// Replace every login for `site` (an empty slice removes the entry).
    fn save_groups<'a>(
        &'a self,
        site: &'a str,
        groups: &'a [LoginGroup],
    ) -> PlatformFuture<'a, Result<()>>;
    /// Drop the stored logins for `site`.
    fn clear<'a>(&'a self, site: &'a str) -> PlatformFuture<'a, Result<()>>;
    /// Every stored site with its ordered logins.
    fn list_groups(&self) -> PlatformFuture<'_, Result<BTreeMap<String, Vec<LoginGroup>>>>;
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

/// 一覧表示用に Cookie を伏せる（名前だけ残す）。
pub fn mask_cookie(cookie: &str) -> String {
    let pairs = parse_cookie_header(cookie);
    let length = cookie.chars().count();
    if pairs.is_empty() {
        return format!("({length} 文字)");
    }
    let names = pairs
        .iter()
        .map(|(name, _)| format!("{name}=…"))
        .collect::<Vec<_>>()
        .join("; ");
    format!("{names} ({} 件, {length} 文字)", pairs.len())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_id_counts_characters_and_never_panics() {
        // 旧実装は byte 境界で切って非 ASCII id で panic した。
        let group = LoginGroup::new("example.com", Vec::new()).with_id("日本語のID");
        assert_eq!(group.short_id(), "日本語のID");
        let long = LoginGroup::new("example.com", Vec::new()).with_id("あいうえおかきくけこ");
        assert_eq!(long.short_id(), "あいうえおかきく");
        assert_eq!(long.short_id().chars().count(), 8);
    }

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
        // native が書いた値 (固定ベクタ、版 2 のホストごと配列) をそのまま
        // 読めること = 保存形式の互換。
        let payload = "novel18.syosetu.com: enc:v1:/cz1hgNv95D2AzmkgLEX7Wo0h0k0Ha+b:ffxRux2SuqGy4+2eDjupAP/AY4b3Go41UeMvY6aPvSEwCu21sRljTXThhmHfqN87c6+TwQjUqYivE9CBoVHzvg8LaPJhwHEdR1fUX1GDYiS4PdPn1YDmEPhd9HockmAVcdN9iqr7WLqA85pY9mIhqBvQySz3iVLVqhJMMrbnLsYZm0gEEnnljuI=\n";
        let key = [7u8; 32];
        let raw: BTreeMap<String, String> = serde_yaml::from_str(payload).unwrap();
        let (site, value) = raw.into_iter().next().unwrap();
        let plain = crate::login::decrypt_stored_value(Some(&key), &site, &value)
            .unwrap()
            .expect("value is encrypted");
        let decoded = decode_groups(&plain, &site).unwrap();
        let DecodedGroups::PerHost(groups) = decoded else {
            panic!("version 2 payload must decode as per-host credentials");
        };
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].cookies[0].cookie, "over18=yes;");
        assert_eq!(groups[0].label.as_deref(), Some("サイト A"));
        assert_eq!(groups[0].id, "11111111-2222-4333-8444-555555555555");
        assert_eq!(groups[0].cookies[0].host, "novel18.syosetu.com");

        // 鍵が無いときは黙って空にせずエラーにする。
        let raw: BTreeMap<String, String> = serde_yaml::from_str(payload).unwrap();
        let (site, value) = raw.into_iter().next().unwrap();
        assert!(crate::login::decrypt_stored_value(None, &site, &value).is_err());
    }

    #[test]
    fn folds_version_two_entries_by_position() {
        // 版 2 はホストごとの並びで、同じ位置が同じアカウント: 1 ファイル =
        // 1 ログインに畳み直す。
        let lists = vec![
            vec![LoginGroup::new(
                "pixiv.net",
                vec![HostCookie {
                    host: "pixiv.net".to_string(),
                    cookie: "PHPSESSID=main".to_string(),
                }],
            )
            .with_id("a")],
            vec![LoginGroup::new(
                "www.pixiv.net",
                vec![HostCookie {
                    host: "www.pixiv.net".to_string(),
                    cookie: "yuid=main".to_string(),
                }],
            )
            .with_id("b")],
        ];
        let folded = fold_per_host_lists(lists, "www.pixiv.net");
        assert_eq!(folded.len(), 1, "1 利用者のホストは 1 ログイン");
        assert_eq!(folded[0].site, "www.pixiv.net");
        assert_eq!(
            folded[0].hosts(),
            vec!["www.pixiv.net", "pixiv.net"],
            "具体的なホストが先"
        );
    }
}

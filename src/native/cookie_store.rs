//! Login cookies persisted through the library inventory.
//!
//! Entries are keyed by request host and stored under the `login_cookie`
//! inventory, so the SQLite backend keeps them in `app_state` and the legacy
//! backend keeps `.narou/login_cookie.yaml`. Switching storage modes migrates
//! them like every other management file.
//!
//! Values are encrypted at rest with the library login key
//! (`.narou/login.key`, or `NAROU_RS_LOGIN_KEY`); the host name is bound into
//! the authentication tag, so a ciphertext cannot be replayed for another host.
//! A plaintext value written by an older build stays readable and is encrypted
//! the next time that host is written.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use crate::db::inventory::{Inventory, InventoryScope};
use crate::error::Result;
use crate::login::crypto::{decrypt_at_rest, encrypt_at_rest};
use crate::native::login_key::LoginKey;
use crate::native::object_store::run_blocking;
use crate::platform::{CookieStore, LoginGroup, PlatformFuture, normalize_cookie_host};

pub const INVENTORY_NAME: &str = "login_cookie";

#[derive(Clone)]
pub struct InventoryCookieStore {
    inventory: Arc<Inventory>,
    key: Arc<OnceLock<Arc<LoginKey>>>,
}

impl InventoryCookieStore {
    pub fn new(inventory: Inventory) -> Self {
        Self {
            inventory: Arc::new(inventory),
            key: Arc::new(OnceLock::new()),
        }
    }

    /// Store for the library the current working directory belongs to.
    pub fn for_current_root() -> Result<Self> {
        Ok(Self::new(Inventory::with_default_root()?))
    }

    /// The library login key, created on first use so a library that never
    /// stores credentials keeps no key file.
    fn key(&self) -> Result<Arc<LoginKey>> {
        if let Some(key) = self.key.get() {
            return Ok(key.clone());
        }
        // `LoginKey::load_or_create` is safe under a race: the loser reads the
        // key the winner wrote, so every process ends up with the same key.
        let key = Arc::new(LoginKey::for_current_library()?);
        let _ = self.key.set(key.clone());
        Ok(self.key.get().cloned().unwrap_or(key))
    }

    fn load_map(&self) -> Result<BTreeMap<String, String>> {
        self.inventory.load(INVENTORY_NAME, InventoryScope::Local)
    }

    /// Every site's logins, decrypting each entry and migrating older shapes.
    ///
    /// Keys were hosts before logins became per-site groups; a value found
    /// under a host is folded into the site that host belongs to and written
    /// back under the site name.
    fn groups(&self) -> Result<BTreeMap<String, Vec<LoginGroup>>> {
        let raw = self.load_map()?;
        if raw.is_empty() {
            return Ok(BTreeMap::new());
        }
        let key = self.key()?;
        let mut stored: BTreeMap<String, Vec<LoginGroup>> = BTreeMap::new();
        let mut legacy_per_site: BTreeMap<String, Vec<Vec<LoginGroup>>> = BTreeMap::new();
        let mut needs_rekey = false;
        for (key_name, value) in raw {
            let plain = match decrypt_at_rest(key.bytes(), &key_name, &value)? {
                Some(cookie) => cookie,
                None => value,
            };
            let site = site_for_key(&key_name);
            if site != key_name {
                needs_rekey = true;
            }
            let Some(decoded) = crate::platform::decode_groups(&plain, &site).ok() else {
                continue;
            };
            match decoded {
                // 版 2 はホストごとに「同じアカウントの並び」を持っていた。
                crate::platform::DecodedGroups::PerHost(groups) => {
                    legacy_per_site.entry(site).or_default().push(groups);
                }
                crate::platform::DecodedGroups::Current(groups) => {
                    if !groups.is_empty() {
                        stored.entry(site).or_default().extend(groups);
                    }
                }
            }
        }
        for (site, lists) in legacy_per_site {
            let folded = crate::platform::fold_per_host_lists(lists, &site);
            stored.entry(site).or_default().extend(folded);
        }
        // 同じサイトのログインが複数のキーから来たら 1 つに畳む。
        let mut stored: BTreeMap<String, Vec<LoginGroup>> = stored
            .into_iter()
            .map(|(site, groups)| {
                let groups = LoginGroup::merge_by_site(groups, &site);
                (site, groups)
            })
            .collect();
        if assign_group_ids(&mut stored) {
            needs_rekey = true;
        }
        if needs_rekey {
            self.save_groups_map(&stored)?;
        }
        Ok(stored)
    }

    /// Encrypt every site entry and write the map back in one inventory write.
    fn save_groups_map(&self, map: &BTreeMap<String, Vec<LoginGroup>>) -> Result<()> {
        if map.values().all(|groups| groups.is_empty()) {
            return self.save_map(&BTreeMap::new());
        }
        let key = self.key()?;
        let mut encrypted = BTreeMap::new();
        for (site, groups) in map {
            if groups.is_empty() {
                continue;
            }
            let encoded = crate::platform::encode_groups(groups)?;
            encrypted.insert(site.clone(), encrypt_at_rest(key.bytes(), site, &encoded)?);
        }
        self.save_map(&encrypted)
    }

    fn save_map(&self, map: &BTreeMap<String, String>) -> Result<()> {
        self.inventory.save(INVENTORY_NAME, InventoryScope::Local, map)
    }

    /// Every stored site with its ordered logins.
    pub fn groups_by_site(&self) -> Result<BTreeMap<String, Vec<LoginGroup>>> {
        self.groups()
    }

    /// One site's logins in trial order.
    pub fn groups_for(&self, site: &str) -> Result<Vec<LoginGroup>> {
        let site = site.trim().to_ascii_lowercase();
        Ok(self.groups()?.get(&site).cloned().unwrap_or_default())
    }

    /// Replace one site's logins (an empty slice removes the entry).
    pub fn save_groups_for(&self, site: &str, groups: &[LoginGroup]) -> Result<()> {
        let mut map = self.groups()?;
        let site = site.trim().to_ascii_lowercase();
        let groups = tidy_groups(groups, &site);
        if groups.is_empty() {
            map.remove(&site);
        } else {
            map.insert(site, groups);
        }
        self.save_groups_map(&map)
    }

    /// Import logins, appending to sites that already have some.
    ///
    /// A login with the same cookies is not added twice. Returns the number of
    /// sites written.
    pub fn merge_groups(
        &self,
        entries: &BTreeMap<String, Vec<LoginGroup>>,
    ) -> Result<usize> {
        let mut map = self.groups()?;
        for (site, groups) in entries {
            let site = site.trim().to_ascii_lowercase();
            let stored = map.entry(site.clone()).or_default();
            for group in tidy_groups(groups, &site) {
                if !stored.iter().any(|existing| existing.same_cookies(&group)) {
                    stored.push(group);
                }
            }
        }
        map.retain(|_, groups| !groups.is_empty());
        let written = map.len();
        self.save_groups_map(&map)?;
        Ok(written)
    }

    /// Import logins, dropping every site that is not part of the import.
    ///
    /// Returns the number of sites written.
    pub fn replace_groups(
        &self,
        entries: &BTreeMap<String, Vec<LoginGroup>>,
    ) -> Result<usize> {
        let mut map: BTreeMap<String, Vec<LoginGroup>> = BTreeMap::new();
        for (site, groups) in entries {
            let site = site.trim().to_ascii_lowercase();
            let groups = tidy_groups(groups, &site);
            if !groups.is_empty() {
                map.insert(site, groups);
            }
        }
        let written = map.len();
        self.save_groups_map(&map)?;
        Ok(written)
    }

    /// Drop one site's logins. Returns whether anything was removed.
    pub fn remove(&self, site: &str) -> Result<bool> {
        let mut map = self.groups()?;
        let removed = map.remove(&site.trim().to_ascii_lowercase()).is_some();
        if removed {
            self.save_groups_map(&map)?;
        }
        Ok(removed)
    }

    /// Where the library login key comes from (creating it when needed).
    pub fn key_source(&self) -> Result<crate::native::login_key::KeySource> {
        Ok(self.key()?.source())
    }

    /// Drop every stored login.
    pub fn clear_all(&self) -> Result<()> {
        self.save_map(&BTreeMap::new())
    }

    /// Whether the stored value for `site` is encrypted (for diagnostics).
    pub fn is_encrypted(&self, site: &str) -> Result<bool> {
        Ok(self
            .load_map()?
            .get(&site.trim().to_ascii_lowercase())
            .is_some_and(|value| crate::login::crypto::is_encrypted_at_rest(value)))
    }
}

/// Site a stored key belongs to.
///
/// Keys are site names today; a key written before that is a host, so it is
/// resolved through the site definitions (falling back to the registrable
/// domain, which is enough to fold `www.pixiv.net` and `pixiv.net` together).
fn site_for_key(key: &str) -> String {
    let key = key.trim().to_ascii_lowercase();
    if let Ok(settings) = crate::downloader::site_setting::SiteSetting::load_all()
        && let Some(setting) = settings.iter().find(|setting| {
            let domain = setting.domain.to_ascii_lowercase();
            // 上位ドメインのキー (`pixiv.net`) は、その配下の定義
            // (`www.pixiv.net`) に属する Cookie が混ざっている。
            domain == key
                || key.ends_with(&format!(".{domain}"))
                || domain.ends_with(&format!(".{key}"))
                || crate::platform::cookie_host_for_url(&setting.top_url())
                    .is_some_and(|host| host == key || key.ends_with(&format!(".{host}")))
        })
    {
        return setting.domain.to_ascii_lowercase();
    }
    // Fall back to the last two labels so hosts of one site stay together.
    let labels: Vec<&str> = key.split('.').collect();
    if labels.len() > 2 {
        labels[labels.len() - 2..].join(".")
    } else {
        key
    }
}

/// Give every login an id, returning whether anything changed.
fn assign_group_ids(stored: &mut BTreeMap<String, Vec<LoginGroup>>) -> bool {
    let mut changed = false;
    for groups in stored.values_mut() {
        for group in groups.iter_mut() {
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
fn tidy_groups(groups: &[LoginGroup], site: &str) -> Vec<LoginGroup> {
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
                Some(crate::platform::HostCookie {
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

impl CookieStore for InventoryCookieStore {
    fn load_groups<'a>(&'a self, site: &'a str) -> PlatformFuture<'a, Result<Vec<LoginGroup>>> {
        let this = self.clone();
        let site = site.to_string();
        run_blocking(move || this.groups_for(&site))
    }

    fn save_groups<'a>(
        &'a self,
        site: &'a str,
        groups: &'a [LoginGroup],
    ) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let site = site.to_string();
        let groups: Vec<LoginGroup> = groups.to_vec();
        run_blocking(move || this.save_groups_for(&site, &groups))
    }

    fn clear<'a>(&'a self, site: &'a str) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let site = site.to_string();
        run_blocking(move || {
            let mut map = this.groups()?;
            map.remove(&site.trim().to_ascii_lowercase());
            this.save_groups_map(&map)
        })
    }

    fn list_groups(&self) -> PlatformFuture<'_, Result<BTreeMap<String, Vec<LoginGroup>>>> {
        let this = self.clone();
        run_blocking(move || this.groups())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::HostCookie;
    use crate::test_support::{legacy_yaml_guard, set_current_dir_for_test};

    fn store_in(temp: &tempfile::TempDir) -> InventoryCookieStore {
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        InventoryCookieStore::for_current_root().unwrap()
    }

    fn group(site: &str, cookies: &[(&str, &str)]) -> LoginGroup {
        LoginGroup::new(
            site,
            cookies
                .iter()
                .map(|(host, cookie)| HostCookie {
                    host: host.to_string(),
                    cookie: cookie.to_string(),
                })
                .collect(),
        )
    }

    #[test]
    fn logins_round_trip_per_site() {
        let _legacy = legacy_yaml_guard();
        let temp = tempfile::tempdir().unwrap();
        let _guard = set_current_dir_for_test(temp.path());
        let store = store_in(&temp);

        assert!(store.groups_for("www.pixiv.net").unwrap().is_empty());
        let main = group(
            "www.pixiv.net",
            &[("pixiv.net", "PHPSESSID=abc"), ("www.pixiv.net", "yuid_b=1")],
        )
        .with_label(Some("Pixiv1".into()));
        store
            .save_groups_for("www.pixiv.net", std::slice::from_ref(&main))
            .unwrap();

        let loaded = store.groups_for("www.pixiv.net").unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].display_name(), "Pixiv1");
        assert_eq!(loaded[0].cookies.len(), 2, "1 ログインに複数ホストの Cookie");
        assert!(!loaded[0].id.is_empty(), "識別子が振られる");
        // ブラウザと同じ送り方: ホストをまたいで 1 本のヘッダにまとめる。
        assert_eq!(
            loaded[0].merged_cookie(),
            "yuid_b=1; PHPSESSID=abc",
            "具体的なホスト (www) を親ドメインより先に送る"
        );
    }

    #[test]
    fn logins_keep_their_trial_order() {
        let _legacy = legacy_yaml_guard();
        let temp = tempfile::tempdir().unwrap();
        let _guard = set_current_dir_for_test(temp.path());
        let store = store_in(&temp);

        let first = group("www.pixiv.net", &[("www.pixiv.net", "PHPSESSID=main")]);
        let second = group("www.pixiv.net", &[("www.pixiv.net", "PHPSESSID=alt")])
            .with_label(Some("サブ".into()));
        store
            .save_groups_for("www.pixiv.net", &[first, second])
            .unwrap();

        let loaded = store.groups_for("www.pixiv.net").unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].cookies[0].cookie, "PHPSESSID=main");
        assert_eq!(loaded[1].display_name(), "サブ");

        store
            .save_groups_for("www.pixiv.net", &[loaded[1].clone(), loaded[0].clone()])
            .unwrap();
        let loaded = store.groups_for("www.pixiv.net").unwrap();
        assert_eq!(loaded[0].display_name(), "サブ");
    }

    #[test]
    fn legacy_host_entries_are_folded_into_their_site() {
        let _legacy = legacy_yaml_guard();
        let temp = tempfile::tempdir().unwrap();
        let _guard = set_current_dir_for_test(temp.path());
        let store = store_in(&temp);

        // 版 2 の形 (ホストごとに 1 本) を書く。暗号化はされていない。
        store
            .save_map(&BTreeMap::from([
                (
                    "pixiv.net".to_string(),
                    r#"[{"id":"g1","host":"pixiv.net","cookie":"PHPSESSID=abc"}]"#.to_string(),
                ),
                (
                    "www.pixiv.net".to_string(),
                    r#"[{"id":"g2","host":"www.pixiv.net","cookie":"yuid_b=1"}]"#.to_string(),
                ),
            ]))
            .unwrap();

        // サイトごとに 1 ログインへまとまる (同じブラウザのセッション)。
        let loaded = store.groups_for("www.pixiv.net").unwrap();
        assert_eq!(loaded.len(), 1, "1 利用者のホストは 1 ログイン: {loaded:?}");
        assert_eq!(loaded[0].site, "www.pixiv.net");
        let hosts: Vec<&str> = loaded[0].hosts();
        assert_eq!(hosts, vec!["www.pixiv.net", "pixiv.net"]);
        assert_eq!(loaded[0].merged_cookie(), "yuid_b=1; PHPSESSID=abc");
        // 親ドメインのキーは残らない。
        assert!(store.groups_for("pixiv.net").unwrap().is_empty());
    }

    #[test]
    fn stored_values_are_encrypted_at_rest() {
        let _legacy = legacy_yaml_guard();
        let temp = tempfile::tempdir().unwrap();
        let _guard = set_current_dir_for_test(temp.path());
        let store = store_in(&temp);

        store
            .save_groups_for("www.pixiv.net", &[group("www.pixiv.net", &[("www.pixiv.net", "session=abc")])])
            .unwrap();

        let raw = store.load_map().unwrap();
        let stored = raw.get("www.pixiv.net").expect("site stored");
        assert!(
            crate::login::crypto::is_encrypted_at_rest(stored),
            "inventory holds ciphertext, got {stored}"
        );
        assert!(!stored.contains("session=abc"), "plaintext leaked into storage");
        assert!(store.is_encrypted("www.pixiv.net").unwrap());
        let loaded = store.groups_for("www.pixiv.net").unwrap();
        assert_eq!(loaded[0].cookies[0].cookie, "session=abc");
    }

    #[test]
    fn import_merges_or_replaces_sites() {
        let _legacy = legacy_yaml_guard();
        let temp = tempfile::tempdir().unwrap();
        let _guard = set_current_dir_for_test(temp.path());
        let store = store_in(&temp);

        store
            .save_groups_for("example.com", &[group("example.com", &[("example.com", "sid=1")])])
            .unwrap();
        let imported = BTreeMap::from([
            (
                "ncode.syosetu.com".to_string(),
                vec![group("ncode.syosetu.com", &[("ncode.syosetu.com", " over18=yes; ses=2 ")])],
            ),
            (
                "www.pixiv.net".to_string(),
                vec![group("www.pixiv.net", &[("pixiv.net", "PHPSESSID=abc")])],
            ),
        ]);

        assert_eq!(store.merge_groups(&imported).unwrap(), 3);
        let listed = store.groups_by_site().unwrap();
        assert_eq!(listed.len(), 3);
        assert_eq!(
            listed["ncode.syosetu.com"][0].cookies[0].cookie,
            "over18=yes; ses=2",
            "前後の空白は落とす"
        );

        assert_eq!(store.replace_groups(&imported).unwrap(), 2);
        let listed = store.groups_by_site().unwrap();
        assert_eq!(listed.len(), 2);
        assert!(!listed.contains_key("example.com"));

        store.clear_all().unwrap();
        assert!(store.groups_by_site().unwrap().is_empty());
    }
}

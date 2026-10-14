//! Login cookies persisted through the library inventory.
//!
//! Entries are keyed by request host and stored under the `login_cookie`
//! inventory, so the SQLite backend keeps them in `app_state` and the legacy
//! backend keeps `.narou/login_cookie.yaml`. Switching storage modes migrates
//! them like every other management file.
//!
//! Values are encrypted at rest with the library login key
//! (`.narou/login.key`, or `NAROU_RS_LOGIN_KEY`); the site name is bound into
//! the authentication tag, so a ciphertext cannot be replayed for another site.
//! A plaintext value written by an older build stays readable and is encrypted
//! the next time the map is written. The decoding, migration and mutation
//! rules are shared with the Worker (`platform::cookie_store`); this type only
//! supplies the inventory and the library key.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use crate::db::inventory::{Inventory, InventoryScope};
use crate::error::Result;
use crate::native::login_key::LoginKey;
use crate::native::object_store::run_blocking;
use crate::platform::cookie_store::{
    DecodedCookieMap, INVENTORY_NAME, compare_save_site_groups, decode_cookie_map,
    encode_cookie_map, merge_site_groups, remove_site_groups, renew_sent_login,
    replace_site_groups, save_site_groups,
};
use crate::platform::{CookieStore, LoginGroup, PlatformFuture};

/// Ordered site logins and the sites encrypted at rest in the same snapshot.
pub type LoginGroupsWithEncryption = (BTreeMap<String, Vec<LoginGroup>>, BTreeSet<String>);

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

    /// Decode and, when necessary, migrate the latest stored map while the
    /// inventory holds its file lock or immediate SQLite transaction.
    fn load_decoded(&self) -> Result<DecodedCookieMap> {
        let mut key = self.key.get().cloned();
        let settings = crate::downloader::site_setting::effective_site_settings();
        loop {
            let decoded = self.inventory.update_yaml(
                INVENTORY_NAME,
                InventoryScope::Local,
                |raw: BTreeMap<String, String>| {
                    if raw.is_empty() {
                        return Ok((raw, Some(DecodedCookieMap::default())));
                    }
                    let Some(key) = &key else {
                        // Resolve the lazy key outside this atomic callback,
                        // then reload; never write from this first snapshot.
                        return Ok((raw, None));
                    };
                    let decoded = decode_cookie_map(&raw, Some(key.bytes()), &settings)?;
                    let next = if decoded.needs_rewrite {
                        encode_cookie_map(&decoded.sites, key.bytes())?
                    } else {
                        raw
                    };
                    Ok((next, Some(decoded)))
                },
            )?;
            if let Some(decoded) = decoded {
                return Ok(decoded);
            }
            key = Some(self.key()?);
        }
    }

    /// Every site's logins, decrypting each entry and migrating older shapes.
    fn groups(&self) -> Result<BTreeMap<String, Vec<LoginGroup>>> {
        Ok(self.load_decoded()?.sites)
    }

    /// Resolve key/settings before locking: the pure callback must not
    /// re-enter SQLite through settings, site definitions or key discovery.
    fn update_groups<T, F>(&self, update: F) -> Result<T>
    where
        F: FnOnce(&mut BTreeMap<String, Vec<LoginGroup>>) -> Result<(T, bool)>,
    {
        let key = self.key()?;
        let settings = crate::downloader::site_setting::effective_site_settings();
        self.inventory.update_yaml(
            INVENTORY_NAME,
            InventoryScope::Local,
            |raw: BTreeMap<String, String>| {
                let mut decoded = decode_cookie_map(&raw, Some(key.bytes()), &settings)?;
                let (result, changed) = update(&mut decoded.sites)?;
                let next = if changed || decoded.needs_rewrite {
                    encode_cookie_map(&decoded.sites, key.bytes())?
                } else {
                    raw
                };
                Ok((next, result))
            },
        )
    }

    /// An explicit whole-map replacement is one write, never a merge with a
    /// snapshot loaded before the inventory's atomic boundary.
    fn save_map(&self, map: BTreeMap<String, String>) -> Result<()> {
        self.inventory.update_yaml(
            INVENTORY_NAME,
            InventoryScope::Local,
            |_current: BTreeMap<String, String>| Ok((map, ())),
        )
    }

    /// Every stored site with its ordered logins.
    pub fn groups_by_site(&self) -> Result<BTreeMap<String, Vec<LoginGroup>>> {
        self.groups()
    }

    /// Every stored site with its logins and per-site at-rest encryption
    /// flags, from one payload read.
    pub fn groups_and_encryption(
        &self,
    ) -> Result<LoginGroupsWithEncryption> {
        let decoded = self.load_decoded()?;
        Ok((decoded.sites, decoded.encrypted))
    }

    /// One site's logins in trial order.
    pub fn groups_for(&self, site: &str) -> Result<Vec<LoginGroup>> {
        let site = site.trim().to_ascii_lowercase();
        Ok(self.groups()?.get(&site).cloned().unwrap_or_default())
    }

    /// Replace one site's logins (an empty slice removes the entry).
    pub fn save_groups_for(&self, site: &str, groups: &[LoginGroup]) -> Result<()> {
        self.update_groups(|map| {
            save_site_groups(map, site, groups);
            Ok(((), true))
        })
    }

    /// Checked replacement for positional actions; a stale site snapshot is
    /// rejected even when it still has the same number of logins.
    pub fn compare_save_groups_for(
        &self,
        site: &str,
        expected: &[LoginGroup],
        replacement: &[LoginGroup],
    ) -> Result<bool> {
        self.update_groups(|map| {
            let saved = compare_save_site_groups(map, site, expected, replacement);
            Ok((saved, saved))
        })
    }

    /// Import logins, appending to sites that already have some.
    ///
    /// A login with the same cookies is not added twice. Returns the number of
    /// sites written.
    pub fn merge_groups(
        &self,
        entries: &BTreeMap<String, Vec<LoginGroup>>,
    ) -> Result<usize> {
        self.update_groups(|map| {
            let written = merge_site_groups(map, entries);
            Ok((written, true))
        })
    }

    /// Import logins, dropping every site that is not part of the import.
    ///
    /// Returns the number of sites written.
    pub fn replace_groups(
        &self,
        entries: &BTreeMap<String, Vec<LoginGroup>>,
    ) -> Result<usize> {
        let map = replace_site_groups(entries);
        let written = map.len();
        let raw = if map.is_empty() {
            BTreeMap::new()
        } else {
            encode_cookie_map(&map, self.key()?.bytes())?
        };
        self.save_map(raw)?;
        Ok(written)
    }

    /// Drop one site's logins. Returns whether anything was removed.
    pub fn remove(&self, site: &str) -> Result<bool> {
        self.update_groups(|map| {
            let removed = remove_site_groups(map, site);
            Ok((removed, removed))
        })
    }

    /// Where the library login key comes from (creating it when needed).
    pub fn key_source(&self) -> Result<crate::native::login_key::KeySource> {
        Ok(self.key()?.source())
    }

    /// Drop every stored login.
    pub fn clear_all(&self) -> Result<()> {
        self.save_map(BTreeMap::new())
    }
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

    fn compare_save_groups<'a>(
        &'a self,
        site: &'a str,
        expected: &'a [LoginGroup],
        replacement: &'a [LoginGroup],
    ) -> PlatformFuture<'a, Result<bool>> {
        let this = self.clone();
        let site = site.to_string();
        let expected = expected.to_vec();
        let replacement = replacement.to_vec();
        run_blocking(move || this.compare_save_groups_for(&site, &expected, &replacement))
    }

    fn clear<'a>(&'a self, site: &'a str) -> PlatformFuture<'a, Result<bool>> {
        let this = self.clone();
        let site = site.to_string();
        run_blocking(move || this.remove(&site))
    }

    fn list_groups(&self) -> PlatformFuture<'_, Result<BTreeMap<String, Vec<LoginGroup>>>> {
        let this = self.clone();
        run_blocking(move || this.groups())
    }

    fn save_all_groups<'a>(
        &'a self,
        groups: &'a BTreeMap<String, Vec<LoginGroup>>,
    ) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let groups = groups.clone();
        run_blocking(move || this.replace_groups(&groups).map(|_| ()))
    }

    fn merge_all_groups<'a>(
        &'a self,
        groups: &'a BTreeMap<String, Vec<LoginGroup>>,
    ) -> PlatformFuture<'a, Result<usize>> {
        let this = self.clone();
        let groups = groups.clone();
        run_blocking(move || this.merge_groups(&groups))
    }

    fn renew_response_cookies<'a>(
        &'a self,
        sent_cookie: &'a str,
        set_cookie: &'a [String],
    ) -> PlatformFuture<'a, Result<bool>> {
        let this = self.clone();
        let sent_cookie = sent_cookie.to_string();
        let set_cookie = set_cookie.to_vec();
        run_blocking(move || this.update_groups(|map| {
            let changed = renew_sent_login(map, &sent_cookie, &set_cookie);
            Ok((changed, changed))
        }))
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
            .save_map(BTreeMap::from([
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
    fn per_host_fragments_fold_into_one_login() {
        let _legacy = legacy_yaml_guard();
        let temp = tempfile::tempdir().unwrap();
        let _guard = set_current_dir_for_test(temp.path());
        let store = store_in(&temp);

        // 版 2 の取り込みでホストごとに分かれていた 1 セッション。
        store
            .save_groups_for(
                "www.pixiv.net",
                &[
                    group("www.pixiv.net", &[("pixiv.net", "PHPSESSID=abc")]),
                    group("www.pixiv.net", &[("www.pixiv.net", "yuid_b=1")]),
                    group("www.pixiv.net", &[("accounts.pixiv.net", "p_ab_id=2")]),
                ],
            )
            .unwrap();

        let loaded = store.groups_for("www.pixiv.net").unwrap();
        assert_eq!(loaded.len(), 1, "1 セッション: {loaded:?}");
        assert_eq!(loaded[0].cookies.len(), 3);
        assert_eq!(loaded[0].merged_cookie(), "yuid_b=1; p_ab_id=2; PHPSESSID=abc");
        assert!(!loaded[0].id.is_empty(), "識別子はどれか 1 つを引き継ぐ");

        // 別アカウントは名前が衝突するので畳まない。
        store
            .save_groups_for(
                "www.pixiv.net",
                &[
                    group("www.pixiv.net", &[("www.pixiv.net", "PHPSESSID=main")]),
                    group("www.pixiv.net", &[("pixiv.net", "PHPSESSID=alt")]),
                ],
            )
            .unwrap();
        let loaded = store.groups_for("www.pixiv.net").unwrap();
        assert_eq!(loaded.len(), 2, "別アカウントは分けたまま: {loaded:?}");
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

        let raw: BTreeMap<String, String> =
            store.inventory.load(INVENTORY_NAME, InventoryScope::Local).unwrap();
        let stored = raw.get("www.pixiv.net").expect("site stored");
        assert!(
            crate::login::crypto::is_encrypted_at_rest(stored),
            "inventory holds ciphertext, got {stored}"
        );
        assert!(!stored.contains("session=abc"), "plaintext leaked into storage");
        let (_, encrypted) = store.groups_and_encryption().unwrap();
        assert!(encrypted.contains("www.pixiv.net"));
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

    fn for_atomic_backends(
        mut test: impl FnMut(&InventoryCookieStore, &InventoryCookieStore),
    ) {
        use crate::native::sqlite::state::{self, StorageMode};
        let _global = crate::test_support::global_state_guard();
        for mode in [StorageMode::Yaml, StorageMode::Sqlite] {
            let temp = tempfile::tempdir().unwrap();
            let _cwd = set_current_dir_for_test(temp.path());
            let narou_dir = temp.path().join(".narou");
            state::write_mode(&narou_dir, mode).unwrap();
            if mode == StorageMode::Sqlite {
                assert!(state::active_for(&narou_dir).is_some());
            }
            let first = store_in(&temp);
            let second = store_in(&temp);
            let key = Arc::new(LoginKey::from_bytes([7; crate::login::KEY_LEN]));
            assert!(first.key.set(key.clone()).is_ok());
            assert!(second.key.set(key).is_ok());
            test(&first, &second);
        }
    }

    #[test]
    fn checked_site_edits_preserve_deletions_and_reject_replacement_accounts() {
        for_atomic_backends(|first, second| {
            first.save_groups_for("a.example", &[group("a.example", &[("a.example", "sid=a")])])
                .unwrap();
            first.save_groups_for("b.example", &[group("b.example", &[("b.example", "sid=b")])])
                .unwrap();
            let expected = second.groups_for("b.example").unwrap();
            first.remove("a.example").unwrap();
            let mut renamed = expected.clone();
            renamed[0].label = Some("renamed".to_string());
            assert!(second.compare_save_groups_for("b.example", &expected, &renamed).unwrap());
            let sites = first.groups_by_site().unwrap();
            assert!(!sites.contains_key("a.example"));
            assert_eq!(sites["b.example"][0].id, expected[0].id);

            let replacement = group("b.example", &[("b.example", "sid=replacement")])
                .with_id("replacement");
            first.save_groups_for("b.example", std::slice::from_ref(&replacement)).unwrap();
            assert!(!second.compare_save_groups_for("b.example", &expected, &renamed).unwrap());
            assert!(!second.compare_save_groups_for("b.example", &expected, &[]).unwrap());
            assert_eq!(first.groups_for("b.example").unwrap(), vec![replacement]);
        });
    }

    #[test]
    fn concurrent_site_delete_rename_renewal_and_import_all_survive() {
        for_atomic_backends(|first, second| {
            for site in ["a.example", "b.example", "c.example"] {
                first.save_groups_for(site, &[group(site, &[(site, &format!("sid={site}"))])])
                    .unwrap();
            }
            let expected = second.groups_for("b.example").unwrap();
            let mut renamed = expected.clone();
            renamed[0].label = Some("renamed".to_string());
            let imported = BTreeMap::from([(
                "d.example".to_string(),
                vec![group("d.example", &[("d.example", "sid=d")])],
            )]);
            let barrier = Arc::new(std::sync::Barrier::new(5));
            std::thread::scope(|scope| {
                let start = barrier.clone();
                let delete = scope.spawn(move || {
                    start.wait();
                    first.remove("a.example").unwrap()
                });
                let start = barrier.clone();
                let rename = scope.spawn(move || {
                    start.wait();
                    second.compare_save_groups_for("b.example", &expected, &renamed).unwrap()
                });
                let start = barrier.clone();
                let renew = scope.spawn(move || {
                    start.wait();
                    futures::executor::block_on(first.renew_response_cookies(
                        "sid=c.example", &["sid=fresh".to_string()],
                    )).unwrap()
                });
                let start = barrier.clone();
                let merge = scope.spawn(move || {
                    start.wait();
                    second.merge_groups(&imported).unwrap();
                });
                barrier.wait();
                assert!(delete.join().unwrap());
                assert!(rename.join().unwrap());
                assert!(renew.join().unwrap());
                merge.join().unwrap();
            });
            let sites = first.groups_by_site().unwrap();
            assert_eq!(sites.len(), 3);
            assert!(!sites.contains_key("a.example"));
            assert_eq!(sites["b.example"][0].label.as_deref(), Some("renamed"));
            assert_eq!(sites["c.example"][0].cookies[0].cookie, "sid=fresh");
            assert_eq!(sites["d.example"][0].cookies[0].cookie, "sid=d");
        });
    }

    #[test]
    fn concurrent_read_migrations_publish_the_same_committed_id() {
        for_atomic_backends(|first, second| {
            let legacy = group("example.com", &[("example.com", "sid=legacy")]);
            first.save_map(BTreeMap::from([(
                "example.com".to_string(),
                crate::platform::cookie_store::encode_groups(&[legacy]).unwrap(),
            )])).unwrap();
            let barrier = Arc::new(std::sync::Barrier::new(3));
            let (one, two) = std::thread::scope(|scope| {
                let start = barrier.clone();
                let one = scope.spawn(move || {
                    start.wait();
                    first.groups_for("example.com").unwrap()
                });
                let start = barrier.clone();
                let two = scope.spawn(move || {
                    start.wait();
                    second.groups_for("example.com").unwrap()
                });
                barrier.wait();
                (one.join().unwrap(), two.join().unwrap())
            });
            assert!(!one[0].id.is_empty());
            assert_eq!(one[0].id, two[0].id);
            assert_eq!(first.groups_for("example.com").unwrap()[0].id, one[0].id);
            let (_, encrypted) = second.groups_and_encryption().unwrap();
            assert!(encrypted.contains("example.com"));
        });
    }

    #[test]
    fn late_response_renewal_preserves_metadata_and_never_restores_removed_logins() {
        for_atomic_backends(|first, second| {
            first.save_groups_for("example.com", &[
                group("example.com", &[("example.com", "sid=sent")]),
                group("example.com", &[("example.com", "sid=other")]),
            ]).unwrap();
            let expected = second.groups_for("example.com").unwrap();
            let mut renamed = expected.clone();
            renamed[0].label = Some("latest name".to_string());
            assert!(second.compare_save_groups_for("example.com", &expected, &renamed).unwrap());
            assert!(futures::executor::block_on(first.renew_response_cookies(
                "sid=sent", &["sid=renewed".to_string()],
            )).unwrap());
            let renewed = second.groups_for("example.com").unwrap();
            assert_eq!(renewed[0].label.as_deref(), Some("latest name"));
            assert_eq!(renewed[0].id, expected[0].id);
            assert_eq!(renewed[1], expected[1]);
            assert!(second.compare_save_groups_for("example.com", &renewed, &renewed[1..]).unwrap());
            assert!(!futures::executor::block_on(first.renew_response_cookies(
                "sid=renewed", &["sid=late".to_string()],
            )).unwrap());
            assert_eq!(second.groups_for("example.com").unwrap(), renewed[1..]);
        });
    }

    #[test]
    fn explicit_replace_and_clear_do_not_decrypt_or_merge_unreadable_values() {
        for_atomic_backends(|first, second| {
            let unreadable = BTreeMap::from([("old.example".to_string(), "enc:v1:invalid".to_string())]);
            first.save_map(unreadable.clone()).unwrap();
            let replacement = BTreeMap::from([(
                "new.example".to_string(),
                vec![group("new.example", &[("new.example", "sid=new")])],
            )]);
            assert_eq!(second.replace_groups(&replacement).unwrap(), 1);
            let sites = first.groups_by_site().unwrap();
            assert_eq!(sites.len(), 1);
            assert!(sites.contains_key("new.example"));
            first.save_map(unreadable).unwrap();
            second.clear_all().unwrap();
            assert!(first.groups_by_site().unwrap().is_empty());
        });
    }
}

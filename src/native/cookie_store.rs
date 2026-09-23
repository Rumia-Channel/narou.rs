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
use crate::platform::{CookieStore, LoginCredential, PlatformFuture, normalize_cookie_host};

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

    /// Read every stored credential, decrypting each site entry.
    ///
    /// The value of one key is a JSON array in trial order; a value written by
    /// an older build is a bare `Cookie:` header and becomes a single entry.
    fn credentials(&self) -> Result<BTreeMap<String, Vec<LoginCredential>>> {
        let raw = self.load_map()?;
        if raw.is_empty() {
            return Ok(BTreeMap::new());
        }
        let key = self.key()?;
        let mut stored = BTreeMap::new();
        for (host, value) in raw {
            let plain = match decrypt_at_rest(key.bytes(), &host, &value)? {
                Some(cookie) => cookie,
                None => value,
            };
            let credentials = crate::platform::decode_credentials(&plain, &host);
            if !credentials.is_empty() {
                stored.insert(host, credentials);
            }
        }
        Ok(stored)
    }

    /// Encrypt every entry and write the map back in one inventory write.
    fn save_credentials(&self, map: &BTreeMap<String, Vec<LoginCredential>>) -> Result<()> {
        if map.values().all(|credentials| credentials.is_empty()) {
            return self.save_map(&BTreeMap::new());
        }
        let key = self.key()?;
        let mut encrypted = BTreeMap::new();
        for (host, credentials) in map {
            if credentials.is_empty() {
                continue;
            }
            let encoded = crate::platform::encode_credentials(credentials)?;
            encrypted.insert(host.clone(), encrypt_at_rest(key.bytes(), host, &encoded)?);
        }
        self.save_map(&encrypted)
    }

    fn save_map(&self, map: &BTreeMap<String, String>) -> Result<()> {
        self.inventory.save(INVENTORY_NAME, InventoryScope::Local, map)
    }

    /// Every stored host with its ordered credentials.
    pub fn credentials_by_host(&self) -> Result<BTreeMap<String, Vec<LoginCredential>>> {
        self.credentials()
    }

    /// One host's credentials in trial order.
    ///
    /// A parent-domain entry (`.pixiv.net`) applies to the subdomain
    /// (`www.pixiv.net`) too, so the most specific key comes first and a value
    /// listed twice is returned once.
    pub fn credentials_for(&self, host: &str) -> Result<Vec<LoginCredential>> {
        let host = normalize_cookie_host(host);
        let stored = self.credentials()?;
        Ok(merge_credentials_for(&stored, &host))
    }

    /// Replace one host's credentials (an empty slice removes the entry).
    pub fn save_credentials_for(
        &self,
        host: &str,
        credentials: &[LoginCredential],
    ) -> Result<()> {
        let mut map = self.credentials()?;
        let host = normalize_cookie_host(host);
        let credentials = tidy(credentials);
        if credentials.is_empty() {
            map.remove(&host);
        } else {
            map.insert(host, credentials);
        }
        self.save_credentials(&map)
    }

    /// Import credentials, appending to hosts that already have some.
    ///
    /// A credential that is already stored (same value) is not added twice.
    /// Returns the number of hosts written.
    pub fn merge_credentials(
        &self,
        entries: &BTreeMap<String, Vec<LoginCredential>>,
    ) -> Result<usize> {
        let mut map = self.credentials()?;
        for (host, credentials) in entries {
            let host = normalize_cookie_host(host);
            let stored = map.entry(host).or_default();
            for credential in tidy(credentials) {
                if !stored.iter().any(|existing| existing.same_cookie(&credential)) {
                    stored.push(credential);
                }
            }
        }
        map.retain(|_, credentials| !credentials.is_empty());
        let written = map.len();
        self.save_credentials(&map)?;
        Ok(written)
    }

    /// Import credentials, dropping every host that is not part of the import.
    ///
    /// Returns the number of hosts written.
    pub fn replace_credentials(
        &self,
        entries: &BTreeMap<String, Vec<LoginCredential>>,
    ) -> Result<usize> {
        let mut map: BTreeMap<String, Vec<LoginCredential>> = BTreeMap::new();
        for (host, credentials) in entries {
            let credentials = tidy(credentials);
            if !credentials.is_empty() {
                map.insert(normalize_cookie_host(host), credentials);
            }
        }
        let written = map.len();
        self.save_credentials(&map)?;
        Ok(written)
    }

    /// Drop one host's credentials. Returns whether anything was removed.
    pub fn remove(&self, host: &str) -> Result<bool> {
        let mut map = self.credentials()?;
        let removed = map.remove(&normalize_cookie_host(host)).is_some();
        if removed {
            self.save_credentials(&map)?;
        }
        Ok(removed)
    }

    /// Where the library login key comes from (creating it when needed).
    pub fn key_source(&self) -> Result<crate::native::login_key::KeySource> {
        Ok(self.key()?.source())
    }

    /// Drop every stored credential.
    pub fn clear_all(&self) -> Result<()> {
        self.save_map(&BTreeMap::new())
    }

    /// Whether the stored value for `host` is encrypted (for diagnostics).
    pub fn is_encrypted(&self, host: &str) -> Result<bool> {
        Ok(self
            .load_map()?
            .get(&normalize_cookie_host(host))
            .is_some_and(|value| crate::login::crypto::is_encrypted_at_rest(value)))
    }
}

/// Normalize a credential before it is stored: surrounding space is never
/// meaningful in a `Cookie:` header, and an empty one is not a credential.
fn tidy(credentials: &[LoginCredential]) -> Vec<LoginCredential> {
    credentials
        .iter()
        .filter_map(|credential| {
            let cookie = credential.cookie.trim();
            if cookie.is_empty() {
                return None;
            }
            let mut credential = credential.clone();
            credential.cookie = cookie.to_string();
            Some(credential)
        })
        .collect()
}

/// 親ドメインも含めたキーから、そのホストで使える資格情報を組み立てる。
fn merge_credentials_for(
    stored: &BTreeMap<String, Vec<LoginCredential>>,
    host: &str,
) -> Vec<LoginCredential> {
    let mut credentials: Vec<LoginCredential> = Vec::new();
    for key in crate::platform::cookie_lookup_hosts(host) {
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

impl CookieStore for InventoryCookieStore {
    fn load_all<'a>(&'a self, host: &'a str) -> PlatformFuture<'a, Result<Vec<LoginCredential>>> {
        let this = self.clone();
        let host = host.to_string();
        run_blocking(move || this.credentials_for(&host))
    }

    fn save_all<'a>(
        &'a self,
        host: &'a str,
        credentials: &'a [LoginCredential],
    ) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let host = host.to_string();
        let credentials: Vec<LoginCredential> = credentials.to_vec();
        run_blocking(move || this.save_credentials_for(&host, &credentials))
    }

    fn clear<'a>(&'a self, host: &'a str) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let host = normalize_cookie_host(host);
        run_blocking(move || {
            let mut map = this.credentials()?;
            map.remove(&host);
            this.save_credentials(&map)
        })
    }

    fn list(&self) -> PlatformFuture<'_, Result<BTreeMap<String, Vec<LoginCredential>>>> {
        let this = self.clone();
        run_blocking(move || this.credentials())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{legacy_yaml_guard, set_current_dir_for_test};

    fn store_in(temp: &tempfile::TempDir) -> InventoryCookieStore {
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        InventoryCookieStore::for_current_root().unwrap()
    }

    #[test]
    fn cookie_store_round_trips_through_the_inventory() {
        let _legacy = legacy_yaml_guard();
        let temp = tempfile::tempdir().unwrap();
        let _guard = set_current_dir_for_test(temp.path());
        let store = store_in(&temp);

        assert!(
            futures::executor::block_on(store.load_all("Example.com"))
                .unwrap()
                .is_empty()
        );
        futures::executor::block_on(store.save_all(
            "Example.com",
            &[LoginCredential::new("example.com", "session=abc")],
        ))
        .unwrap();
        let loaded = futures::executor::block_on(store.load_all("example.com")).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].cookie, "session=abc");
        assert_eq!(
            futures::executor::block_on(store.list()).unwrap().len(),
            1,
            "hosts are stored case-insensitively"
        );

        futures::executor::block_on(store.clear("example.com")).unwrap();
        assert!(
            futures::executor::block_on(store.load_all("example.com"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn credentials_keep_their_trial_order() {
        let _legacy = legacy_yaml_guard();
        let temp = tempfile::tempdir().unwrap();
        let _guard = set_current_dir_for_test(temp.path());
        let store = store_in(&temp);

        let first = LoginCredential::new("www.pixiv.net", "PHPSESSID=main");
        let second = LoginCredential::new("www.pixiv.net", "PHPSESSID=alt").with_label(Some("R18用".into()));
        futures::executor::block_on(store.save_all("www.pixiv.net", &[first, second])).unwrap();

        let loaded = futures::executor::block_on(store.load_all("www.pixiv.net")).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].cookie, "PHPSESSID=main");
        assert_eq!(loaded[1].display_name(), "R18用");

        // 並べ替えは保存順そのもの。
        let reordered = vec![loaded[1].clone(), loaded[0].clone()];
        futures::executor::block_on(store.save_all("www.pixiv.net", &reordered)).unwrap();
        let loaded = futures::executor::block_on(store.load_all("www.pixiv.net")).unwrap();
        assert_eq!(loaded[0].display_name(), "R18用");
        assert_eq!(loaded[1].cookie, "PHPSESSID=main");
    }

    #[test]
    fn legacy_single_values_are_read_as_one_credential() {
        let _legacy = legacy_yaml_guard();
        let temp = tempfile::tempdir().unwrap();
        let _guard = set_current_dir_for_test(temp.path());
        let store = store_in(&temp);

        // 旧形式（host → Cookie 文字列）を直接書く。
        store
            .save_map(&BTreeMap::from([(
                "ncode.syosetu.com".to_string(),
                "over18=yes; ses=1".to_string(),
            )]))
            .unwrap();

        let loaded = futures::executor::block_on(store.load_all("ncode.syosetu.com")).unwrap();
        assert_eq!(loaded.len(), 1, "旧形式は 1 件として読む: {loaded:?}");
        assert_eq!(loaded[0].cookie, "over18=yes; ses=1");
    }

    #[test]
    fn parent_domain_cookies_reach_subdomain_requests() {
        let _legacy = legacy_yaml_guard();
        let temp = tempfile::tempdir().unwrap();
        let _guard = set_current_dir_for_test(temp.path());
        let store = store_in(&temp);

        // Pixiv stores the session on `.pixiv.net`, so the login executable
        // saves it under the parent domain while requests go to `www`.
        futures::executor::block_on(store.save_all(
            "pixiv.net",
            &[LoginCredential::new("pixiv.net", "PHPSESSID=abc; cc1=1")],
        ))
        .unwrap();
        futures::executor::block_on(store.save_all(
            "www.pixiv.net",
            &[LoginCredential::new("www.pixiv.net", "www_only=1")],
        ))
        .unwrap();

        let loaded = futures::executor::block_on(store.load_all("www.pixiv.net")).unwrap();
        let cookies: Vec<&str> = loaded.iter().map(|credential| credential.cookie.as_str()).collect();
        assert_eq!(cookies, vec!["www_only=1", "PHPSESSID=abc; cc1=1"]);
        // An unrelated host must not see the session.
        assert!(
            futures::executor::block_on(store.load_all("example.com"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn stored_values_are_encrypted_at_rest() {
        let _legacy = legacy_yaml_guard();
        let temp = tempfile::tempdir().unwrap();
        let _guard = set_current_dir_for_test(temp.path());
        let store = store_in(&temp);

        futures::executor::block_on(store.save_all(
            "example.com",
            &[LoginCredential::new("example.com", "session=abc")],
        ))
        .unwrap();

        let raw = store.load_map().unwrap();
        let stored = raw.get("example.com").expect("host stored");
        assert!(
            crate::login::crypto::is_encrypted_at_rest(stored),
            "inventory holds ciphertext, got {stored}"
        );
        assert!(!stored.contains("session=abc"), "plaintext leaked into storage");
        assert!(store.is_encrypted("example.com").unwrap());
        let loaded = futures::executor::block_on(store.load_all("example.com")).unwrap();
        assert_eq!(loaded[0].cookie, "session=abc");
    }

    #[test]
    fn plaintext_written_by_an_older_build_stays_readable() {
        let _legacy = legacy_yaml_guard();
        let temp = tempfile::tempdir().unwrap();
        let _guard = set_current_dir_for_test(temp.path());
        let store = store_in(&temp);

        store
            .save_map(&BTreeMap::from([(
                "example.com".to_string(),
                "session=legacy".to_string(),
            )]))
            .unwrap();

        let loaded = futures::executor::block_on(store.load_all("example.com")).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].cookie, "session=legacy");
        assert!(!store.is_encrypted("example.com").unwrap());

        // The next write of that host encrypts it.
        store
            .save_credentials_for(
                "example.com",
                &[LoginCredential::new("example.com", "session=legacy")],
            )
            .unwrap();
        assert!(store.is_encrypted("example.com").unwrap());
    }

    #[test]
    fn import_merges_or_replaces_hosts() {
        let _legacy = legacy_yaml_guard();
        let temp = tempfile::tempdir().unwrap();
        let _guard = set_current_dir_for_test(temp.path());
        let store = store_in(&temp);

        futures::executor::block_on(store.save_all(
            "example.com",
            &[LoginCredential::new("example.com", "sid=1")],
        ))
        .unwrap();
        let imported = BTreeMap::from([
            (
                "Ncode.Syosetu.com".to_string(),
                vec![LoginCredential::new("ncode.syosetu.com", " over18=yes; ses=2 ")],
            ),
            (
                "other.example".to_string(),
                vec![LoginCredential::new("other.example", "sid=3")],
            ),
        ]);

        assert_eq!(store.merge_credentials(&imported).unwrap(), 3);
        let listed = futures::executor::block_on(store.list()).unwrap();
        assert_eq!(listed.len(), 3);
        assert_eq!(
            listed.get("ncode.syosetu.com").map(|credentials| credentials[0].cookie.as_str()),
            Some("over18=yes; ses=2")
        );

        assert_eq!(store.replace_credentials(&imported).unwrap(), 2);
        let listed = futures::executor::block_on(store.list()).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(!listed.contains_key("example.com"));

        store.clear_all().unwrap();
        assert!(futures::executor::block_on(store.list()).unwrap().is_empty());
    }
}

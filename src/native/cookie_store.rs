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
use crate::platform::{CookieStore, PlatformFuture};

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

    /// Read the stored cookies, decrypting every value.
    fn load_plain(&self) -> Result<BTreeMap<String, String>> {
        let raw = self.load_map()?;
        if raw.is_empty() {
            return Ok(raw);
        }
        let key = self.key()?;
        let mut plain = BTreeMap::new();
        for (host, value) in raw {
            let cookie = match decrypt_at_rest(key.bytes(), &host, &value)? {
                Some(cookie) => cookie,
                None => value,
            };
            plain.insert(host, cookie);
        }
        Ok(plain)
    }

    /// Encrypt every value and write the map back in one inventory write.
    fn save_plain(&self, map: &BTreeMap<String, String>) -> Result<()> {
        if map.is_empty() {
            return self.save_map(map);
        }
        let key = self.key()?;
        let mut encrypted = BTreeMap::new();
        for (host, cookie) in map {
            encrypted.insert(host.clone(), encrypt_at_rest(key.bytes(), host, cookie)?);
        }
        self.save_map(&encrypted)
    }

    fn save_map(&self, map: &BTreeMap<String, String>) -> Result<()> {
        self.inventory.save(INVENTORY_NAME, InventoryScope::Local, map)
    }

    /// Import `cookies`, keeping hosts that are not part of the import.
    ///
    /// Returns the number of hosts written.
    pub fn merge(&self, cookies: &BTreeMap<String, String>) -> Result<usize> {
        let mut map = self.load_plain()?;
        for (host, cookie) in cookies {
            map.insert(normalize_host(host), cookie.trim().to_string());
        }
        map.retain(|_, cookie| !cookie.is_empty());
        let written = map.len();
        self.save_plain(&map)?;
        Ok(written)
    }

    /// Import `cookies`, dropping every host that is not part of the import.
    ///
    /// Returns the number of hosts written.
    pub fn replace_all(&self, cookies: &BTreeMap<String, String>) -> Result<usize> {
        let mut map: BTreeMap<String, String> = BTreeMap::new();
        for (host, cookie) in cookies {
            map.insert(normalize_host(host), cookie.trim().to_string());
        }
        map.retain(|_, cookie| !cookie.is_empty());
        let written = map.len();
        self.save_plain(&map)?;
        Ok(written)
    }

    /// Drop every stored credential.
    pub fn clear_all(&self) -> Result<()> {
        self.save_map(&BTreeMap::new())
    }

    /// Whether the stored value for `host` is encrypted (for diagnostics).
    pub fn is_encrypted(&self, host: &str) -> Result<bool> {
        Ok(self
            .load_map()?
            .get(&normalize_host(host))
            .is_some_and(|value| crate::login::crypto::is_encrypted_at_rest(value)))
    }
}

fn normalize_host(host: &str) -> String {
    host.trim().to_ascii_lowercase()
}

impl CookieStore for InventoryCookieStore {
    fn load<'a>(&'a self, host: &'a str) -> PlatformFuture<'a, Result<Option<String>>> {
        let this = self.clone();
        let host = normalize_host(host);
        run_blocking(move || {
            Ok(this
                .load_plain()?
                .get(&host)
                .filter(|cookie| !cookie.is_empty())
                .cloned())
        })
    }

    fn save<'a>(&'a self, host: &'a str, cookie: &'a str) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let host = normalize_host(host);
        let cookie = cookie.to_string();
        run_blocking(move || {
            let mut map = this.load_plain()?;
            if cookie.trim().is_empty() {
                map.remove(&host);
            } else {
                map.insert(host, cookie);
            }
            this.save_plain(&map)
        })
    }

    fn clear<'a>(&'a self, host: &'a str) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let host = normalize_host(host);
        run_blocking(move || {
            let mut map = this.load_plain()?;
            map.remove(&host);
            this.save_plain(&map)
        })
    }

    fn list(&self) -> PlatformFuture<'_, Result<BTreeMap<String, String>>> {
        let this = self.clone();
        run_blocking(move || this.load_plain())
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
            futures::executor::block_on(store.load("Example.com"))
                .unwrap()
                .is_none()
        );
        futures::executor::block_on(store.save("Example.com", "session=abc")).unwrap();
        assert_eq!(
            futures::executor::block_on(store.load("example.com")).unwrap().as_deref(),
            Some("session=abc")
        );
        assert_eq!(
            futures::executor::block_on(store.list()).unwrap().len(),
            1,
            "hosts are stored case-insensitively"
        );

        futures::executor::block_on(store.clear("example.com")).unwrap();
        assert!(
            futures::executor::block_on(store.load("example.com"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn stored_values_are_encrypted_at_rest() {
        let _legacy = legacy_yaml_guard();
        let temp = tempfile::tempdir().unwrap();
        let _guard = set_current_dir_for_test(temp.path());
        let store = store_in(&temp);

        futures::executor::block_on(store.save("example.com", "session=abc")).unwrap();

        let raw = store.load_map().unwrap();
        let stored = raw.get("example.com").expect("host stored");
        assert!(
            crate::login::crypto::is_encrypted_at_rest(stored),
            "inventory holds ciphertext, got {stored}"
        );
        assert!(!stored.contains("session=abc"), "plaintext leaked into storage");
        assert!(store.is_encrypted("example.com").unwrap());
        assert_eq!(
            futures::executor::block_on(store.load("example.com")).unwrap().as_deref(),
            Some("session=abc")
        );
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

        assert_eq!(
            futures::executor::block_on(store.load("example.com")).unwrap().as_deref(),
            Some("session=legacy")
        );
        assert!(!store.is_encrypted("example.com").unwrap());

        // The next write of that host encrypts it.
        futures::executor::block_on(store.save("example.com", "session=legacy")).unwrap();
        assert!(store.is_encrypted("example.com").unwrap());
    }

    #[test]
    fn import_merges_or_replaces_hosts() {
        let _legacy = legacy_yaml_guard();
        let temp = tempfile::tempdir().unwrap();
        let _guard = set_current_dir_for_test(temp.path());
        let store = store_in(&temp);

        futures::executor::block_on(store.save("example.com", "sid=1")).unwrap();
        let imported = BTreeMap::from([
            ("Ncode.Syosetu.com".to_string(), " over18=yes; ses=2 ".to_string()),
            ("other.example".to_string(), "sid=3".to_string()),
        ]);

        assert_eq!(store.merge(&imported).unwrap(), 3);
        let listed = futures::executor::block_on(store.list()).unwrap();
        assert_eq!(listed.len(), 3);
        assert_eq!(listed.get("ncode.syosetu.com").map(String::as_str), Some("over18=yes; ses=2"));
        assert!(listed.values().all(|cookie| !cookie.starts_with(' ')));

        assert_eq!(store.replace_all(&imported).unwrap(), 2);
        let listed = futures::executor::block_on(store.list()).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(!listed.contains_key("example.com"));

        store.clear_all().unwrap();
        assert!(futures::executor::block_on(store.list()).unwrap().is_empty());
    }
}

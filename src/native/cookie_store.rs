//! Login cookies persisted through the library inventory.
//!
//! Entries are keyed by request host and stored under the `login_cookie`
//! inventory, so the SQLite backend keeps them in `app_state` and the legacy
//! backend keeps `.narou/login_cookie.yaml`. Switching storage modes migrates
//! them like every other management file.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::db::inventory::{Inventory, InventoryScope};
use crate::error::Result;
use crate::native::object_store::run_blocking;
use crate::platform::{CookieStore, PlatformFuture};

pub const INVENTORY_NAME: &str = "login_cookie";

#[derive(Clone)]
pub struct InventoryCookieStore {
    inventory: Arc<Inventory>,
}

impl InventoryCookieStore {
    pub fn new(inventory: Inventory) -> Self {
        Self {
            inventory: Arc::new(inventory),
        }
    }

    /// Store for the library the current working directory belongs to.
    pub fn for_current_root() -> Result<Self> {
        Ok(Self::new(Inventory::with_default_root()?))
    }

    fn load_map(&self) -> Result<BTreeMap<String, String>> {
        self.inventory.load(INVENTORY_NAME, InventoryScope::Local)
    }

    fn save_map(&self, map: &BTreeMap<String, String>) -> Result<()> {
        self.inventory.save(INVENTORY_NAME, InventoryScope::Local, map)
    }
}

impl CookieStore for InventoryCookieStore {
    fn load<'a>(&'a self, host: &'a str) -> PlatformFuture<'a, Result<Option<String>>> {
        let this = self.clone();
        let host = host.to_string();
        run_blocking(move || {
            Ok(this
                .load_map()?
                .get(&host.to_ascii_lowercase())
                .filter(|cookie| !cookie.is_empty())
                .cloned())
        })
    }

    fn save<'a>(&'a self, host: &'a str, cookie: &'a str) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let host = host.to_ascii_lowercase();
        let cookie = cookie.to_string();
        run_blocking(move || {
            let mut map = this.load_map()?;
            if cookie.trim().is_empty() {
                map.remove(&host);
            } else {
                map.insert(host, cookie);
            }
            this.save_map(&map)
        })
    }

    fn clear<'a>(&'a self, host: &'a str) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let host = host.to_ascii_lowercase();
        run_blocking(move || {
            let mut map = this.load_map()?;
            map.remove(&host);
            this.save_map(&map)
        })
    }

    fn list(&self) -> PlatformFuture<'_, Result<BTreeMap<String, String>>> {
        let this = self.clone();
        run_blocking(move || this.load_map())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{legacy_yaml_guard, set_current_dir_for_test};

    #[test]
    fn cookie_store_round_trips_through_the_inventory() {
        let _legacy = legacy_yaml_guard();
        let temp = tempfile::tempdir().unwrap();
        let _guard = set_current_dir_for_test(temp.path());
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        let store = InventoryCookieStore::for_current_root().unwrap();

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
}

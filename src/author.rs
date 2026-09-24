//! Authors whose works are tracked.
//!
//! Kept outside the novel database on purpose: an author is a *watch target*,
//! not a novel, and the only things worth remembering are the site it belongs
//! to and the author page itself (`site` + `url`, plus bookkeeping for the
//! list). When `narou update` runs, every entry here is checked and the works
//! that are not in the library yet are downloaded.
//!
//! The store lives in the `author` inventory, so SQLite keeps it in `app_state`
//! and the legacy backend keeps `.narou/author.yaml`.

use std::collections::BTreeMap;

use crate::db::inventory::{Inventory, InventoryScope};
use crate::error::Result;

/// Inventory name and legacy file name (`author.yaml`).
pub const INVENTORY_NAME: &str = "author";

/// One tracked author.
///
/// Nothing but the site and the page: the URL is the identity, and everything
/// else (name, when it was added, when it was last checked) is either
/// derivable or bookkeeping the feature does not need.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AuthorRecord {
    /// Site definition domain the author page belongs to, e.g.
    /// `ncode.syosetu.com`.
    pub site: String,
    /// Author page URL, e.g. `https://mypage.syosetu.com/2842627/`. Unique.
    pub url: String,
}

impl AuthorRecord {
    pub fn new(site: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            site: site.into(),
            url: url.into(),
        }
    }

    /// Whether two entries point at the same page.
    pub fn same_target(&self, other: &Self) -> bool {
        self.url == other.url
    }
}

/// Every tracked author, ordered by site then URL.
pub fn load_authors(inventory: &Inventory) -> Result<Vec<AuthorRecord>> {
    let raw = inventory.load_raw(INVENTORY_NAME, InventoryScope::Local)?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let value: serde_yaml::Value = serde_yaml::from_str(&raw)?;
    let Some(mapping) = value.as_mapping() else {
        return Ok(Vec::new());
    };
    let mut authors = Vec::new();
    for (key, entry) in mapping {
        let Some(url) = key.as_str() else {
            continue;
        };
        // 値はサイト名だけ。旧形式 (site/name/added_at… を持つマップ) も読む。
        let site = match entry {
            serde_yaml::Value::String(site) => site.clone(),
            other => other
                .get("site")
                .and_then(serde_yaml::Value::as_str)
                .unwrap_or_default()
                .to_string(),
        };
        authors.push(AuthorRecord::new(site, url));
    }
    authors.sort_by(|left, right| {
        left.site
            .cmp(&right.site)
            .then_with(|| left.url.cmp(&right.url))
    });
    Ok(authors)
}

/// Replace the tracked authors.
pub fn save_authors(inventory: &Inventory, authors: &[AuthorRecord]) -> Result<()> {
    if authors.is_empty() {
        return inventory.save_raw(INVENTORY_NAME, InventoryScope::Local, "");
    }
    let mut map = serde_yaml::Mapping::new();
    for author in authors {
        map.insert(
            serde_yaml::Value::String(author.url.clone()),
            serde_yaml::Value::String(author.site.clone()),
        );
    }
    let text = serde_yaml::to_string(&serde_yaml::Value::Mapping(map))?;
    inventory.save_raw(INVENTORY_NAME, InventoryScope::Local, &text)
}

/// Convenience: the tracked authors of the current library.
pub fn authors_for_current_root() -> Result<Vec<AuthorRecord>> {
    load_authors(&Inventory::with_default_root()?)
}

/// Add `author` unless the same page is already tracked.
///
/// Returns `true` when the list changed.
pub fn add_author(author: &AuthorRecord) -> Result<bool> {
    let inventory = Inventory::with_default_root()?;
    let mut authors = load_authors(&inventory)?;
    if authors.iter().any(|existing| existing.same_target(author)) {
        return Ok(false);
    }
    authors.push(author.clone());
    save_authors(&inventory, &authors)?;
    Ok(true)
}

/// Drop one tracked author. Returns whether anything was removed.
pub fn remove_author(url: &str) -> Result<bool> {
    let inventory = Inventory::with_default_root()?;
    let mut authors = load_authors(&inventory)?;
    let before = authors.len();
    authors.retain(|author| author.url != url);
    if authors.len() == before {
        return Ok(false);
    }
    save_authors(&inventory, &authors)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{legacy_yaml_guard, set_current_dir_for_test};

    fn library() -> (tempfile::TempDir, crate::test_support::CurrentDirGuard, Inventory) {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".narou")).unwrap();
        // `with_default_root` walks up from the working directory, so the
        // guard has to be in place before the inventory is built.
        let guard = set_current_dir_for_test(temp.path());
        let inventory = Inventory::with_default_root().unwrap();
        (temp, guard, inventory)
    }

    #[test]
    fn authors_round_trip_through_the_inventory() {
        let _legacy = legacy_yaml_guard();
        let (_temp, _guard, inventory) = library();

        assert!(load_authors(&inventory).unwrap().is_empty());
        let author =
            AuthorRecord::new("ncode.syosetu.com", "https://mypage.syosetu.com/2842627/");
        save_authors(&inventory, std::slice::from_ref(&author)).unwrap();

        assert_eq!(load_authors(&inventory).unwrap(), vec![author]);

        // 保存は URL(ユニーク) → サイト名 だけ。余計な情報は持たない。
        let text = inventory
            .load_raw(INVENTORY_NAME, InventoryScope::Local)
            .unwrap();
        assert_eq!(
            text.trim(),
            "https://mypage.syosetu.com/2842627/: ncode.syosetu.com"
        );
    }

    #[test]
    fn a_map_written_by_an_older_build_still_reads() {
        let _legacy = legacy_yaml_guard();
        let (_temp, _guard, inventory) = library();

        inventory
            .save_raw(
                INVENTORY_NAME,
                InventoryScope::Local,
                "https://mypage.syosetu.com/2842627/:\n  site: ncode.syosetu.com\n  name: 作者名\n  last_found: 2\n",
            )
            .unwrap();

        let loaded = load_authors(&inventory).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].site, "ncode.syosetu.com");
        assert_eq!(loaded[0].url, "https://mypage.syosetu.com/2842627/");
    }

    #[test]
    fn adding_the_same_author_twice_is_a_no_op() {
        let _legacy = legacy_yaml_guard();
        let (_temp, _guard, _inventory) = library();

        let author =
            AuthorRecord::new("ncode.syosetu.com", "https://mypage.syosetu.com/2842627/");
        assert!(add_author(&author).unwrap());
        assert!(!add_author(&author).unwrap(), "同じページは二重登録しない");
        assert_eq!(authors_for_current_root().unwrap().len(), 1);

        assert!(remove_author(&author.url).unwrap());
        assert!(!remove_author(&author.url).unwrap());
        assert!(authors_for_current_root().unwrap().is_empty());
    }
}

//! Authors whose works are tracked.
//!
//! Kept outside the novel database on purpose: an author is a *watch target*,
//! not a novel, and the only things worth remembering are the site it belongs
//! to and the author page itself (`site` + `url`). Update checks these targets
//! and downloads works that are not in the library yet.
//!
//! The store lives in the `author` inventory, so SQLite keeps it in `app_state`
//! and the legacy backend keeps `.narou/author.yaml`.

#[cfg(feature = "native-runtime")]
use crate::db::inventory::{Inventory, InventoryScope};
#[cfg(feature = "native-runtime")]
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

/// Decode the native inventory / D1 value, including old richer records.
pub fn authors_from_value(value: &serde_yaml::Value) -> Vec<AuthorRecord> {
    let Some(mapping) = value.as_mapping() else {
        return Vec::new();
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
    authors
}

pub fn authors_to_value(authors: &[AuthorRecord]) -> serde_yaml::Value {
    serde_yaml::Value::Mapping(authors.iter().map(|author| (
        serde_yaml::Value::String(author.url.clone()),
        serde_yaml::Value::String(author.site.clone()),
    )).collect())
}

/// Every tracked author, ordered by site then URL.
#[cfg(feature = "native-runtime")]
pub fn load_authors(inventory: &Inventory) -> Result<Vec<AuthorRecord>> {
    let raw = inventory.load_raw(INVENTORY_NAME, InventoryScope::Local)?;
    if raw.trim().is_empty() { return Ok(Vec::new()); }
    Ok(authors_from_value(&serde_yaml::from_str(&raw)?))
}

/// Replace the tracked authors.
#[cfg(feature = "native-runtime")]
pub fn save_authors(inventory: &Inventory, authors: &[AuthorRecord]) -> Result<()> {
    if authors.is_empty() {
        return inventory.save_raw(INVENTORY_NAME, InventoryScope::Local, "");
    }
    let text = serde_yaml::to_string(&authors_to_value(authors))?;
    inventory.save_raw(INVENTORY_NAME, InventoryScope::Local, &text)
}

/// Convenience: the tracked authors of the current library.
#[cfg(feature = "native-runtime")]
pub fn authors_for_current_root() -> Result<Vec<AuthorRecord>> {
    load_authors(&Inventory::with_default_root()?)
}

/// Add `author` unless the same page is already tracked.
///
/// Returns `true` when the list changed.
#[cfg(feature = "native-runtime")]
pub fn add_author(author: &AuthorRecord) -> Result<bool> {
    add_author_in(&Inventory::with_default_root()?, author)
}

#[cfg(feature = "native-runtime")]
pub fn add_author_in(inventory: &Inventory, author: &AuthorRecord) -> Result<bool> {
    inventory.update_yaml(INVENTORY_NAME, InventoryScope::Local, |mut current: serde_yaml::Value| {
        let changed = !authors_from_value(&current).iter().any(|entry| entry.same_target(author));
        if changed {
            if !current.is_mapping() { current = serde_yaml::Value::Mapping(Default::default()); }
            current.as_mapping_mut().unwrap().insert(
                serde_yaml::Value::String(author.url.clone()), serde_yaml::Value::String(author.site.clone()),
            );
        }
        Ok((current, changed))
    })
}

/// Drop one tracked author. Returns whether anything was removed.
#[cfg(feature = "native-runtime")]
pub fn remove_author(url: &str) -> Result<bool> {
    remove_author_in(&Inventory::with_default_root()?, url)
}

#[cfg(feature = "native-runtime")]
pub fn remove_author_in(inventory: &Inventory, url: &str) -> Result<bool> {
    inventory.update_yaml(INVENTORY_NAME, InventoryScope::Local, |mut current: serde_yaml::Value| {
        let changed = current.as_mapping_mut().is_some_and(|map| map.remove(serde_yaml::Value::String(url.to_string())).is_some());
        Ok((current, changed))
    })
}

#[cfg(all(test, feature = "native-runtime"))]
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

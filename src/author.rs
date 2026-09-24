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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AuthorRecord {
    /// Site definition domain the author page belongs to, e.g.
    /// `ncode.syosetu.com`.
    pub site: String,
    /// Author page URL, e.g. `https://mypage.syosetu.com/2842627/`.
    pub url: String,
    /// Display name, when the site definition or the user provided one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// When the author was added (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_at: Option<String>,
    /// When the page was last checked (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checked_at: Option<String>,
    /// Works found by the last check that were not in the library yet.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub last_found: usize,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

impl AuthorRecord {
    pub fn new(site: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            site: site.into(),
            url: url.into(),
            name: None,
            added_at: None,
            last_checked_at: None,
            last_found: 0,
        }
    }

    pub fn with_name(mut self, name: Option<String>) -> Self {
        self.name = name.filter(|name| !name.trim().is_empty());
        self
    }

    pub fn with_added_at(mut self, added_at: Option<String>) -> Self {
        self.added_at = added_at;
        self
    }

    /// Label for the CLI and Web UI, falling back to the URL.
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.url)
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
        match serde_yaml::from_value::<AuthorRecord>(entry.clone()) {
            Ok(author) => authors.push(author),
            // A URL key with an unusable body keeps the site alone, so a hand
            // edited file still tracks something instead of silently dropping.
            Err(_) => {
                authors.push(AuthorRecord {
                    site: String::new(),
                    url: url.to_string(),
                    name: None,
                    added_at: None,
                    last_checked_at: None,
                    last_found: 0,
                });
            }
        }
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
        inventory.save_raw(INVENTORY_NAME, InventoryScope::Local, "")
    } else {
        let mut map = serde_yaml::Mapping::new();
        for author in authors {
            map.insert(
                serde_yaml::Value::String(author.url.clone()),
                serde_yaml::to_value(author)?,
            );
        }
        let text = serde_yaml::to_string(&serde_yaml::Value::Mapping(map))?;
        inventory.save_raw(INVENTORY_NAME, InventoryScope::Local, &text)
    }
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

/// Record the outcome of a check for `url`.
pub fn mark_checked(
    inventory: &Inventory,
    url: &str,
    checked_at: &str,
    found: usize,
    name: Option<&str>,
) -> Result<bool> {
    let mut authors = load_authors(inventory)?;
    let mut changed = false;
    for author in authors.iter_mut() {
        if author.url != url {
            continue;
        }
        author.last_checked_at = Some(checked_at.to_string());
        author.last_found = found;
        if let Some(name) = name.filter(|name| !name.trim().is_empty()) {
            author.name = Some(name.to_string());
        }
        changed = true;
    }
    if changed {
        save_authors(inventory, &authors)?;
    }
    Ok(changed)
}

/// Keyed view used by callers that prefer lookups by URL.
pub fn authors_by_url(authors: &[AuthorRecord]) -> BTreeMap<String, AuthorRecord> {
    authors
        .iter()
        .map(|author| (author.url.clone(), author.clone()))
        .collect()
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
        let mut author = AuthorRecord::new(
            "ncode.syosetu.com",
            "https://mypage.syosetu.com/2842627/",
        )
        .with_name(Some("作者名".into()));
        author.added_at = Some("2026-09-24T00:00:00+09:00".into());
        save_authors(&inventory, std::slice::from_ref(&author)).unwrap();

        let loaded = load_authors(&inventory).unwrap();
        assert_eq!(loaded, vec![author]);

        // The record holds the site and the page; nothing about novels.
        let text = inventory
            .load_raw(INVENTORY_NAME, InventoryScope::Local)
            .unwrap();
        assert!(text.contains("ncode.syosetu.com"), "got {text}");
        assert!(text.contains("mypage.syosetu.com/2842627"), "got {text}");
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

    #[test]
    fn checking_an_author_updates_its_bookkeeping() {
        let _legacy = legacy_yaml_guard();
        let (_temp, _guard, inventory) = library();

        let author =
            AuthorRecord::new("ncode.syosetu.com", "https://mypage.syosetu.com/2842627/");
        add_author(&author).unwrap();
        assert!(
            mark_checked(
                &inventory,
                &author.url,
                "2026-09-24T10:00:00+09:00",
                2,
                Some("作者名")
            )
            .unwrap()
        );

        let loaded = authors_for_current_root().unwrap();
        assert_eq!(loaded[0].last_found, 2);
        assert_eq!(loaded[0].last_checked_at.as_deref(), Some("2026-09-24T10:00:00+09:00"));
        assert_eq!(loaded[0].name.as_deref(), Some("作者名"));
    }
}

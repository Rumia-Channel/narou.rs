//! Recover freeze state scattered across `freeze.yaml.imported-*` fragments.
//!
//! Older builds wrote `.narou/freeze.yaml` directly on every `freeze` run even
//! though SQLite owned the state. Each launch then imported that fragment —
//! replacing the stored payload — and renamed it to
//! `freeze.yaml.imported-<unix_ts>`, so the frozen novels of the previous
//! payload survived only in those fragments (see the freeze section of
//! `AGENTS.md` / issue #35).
//!
//! The current code no longer produces fragments and imports them as a union,
//! but libraries that already went through the old builds still hold their
//! lost ids in the `.imported-*` files. [`plan`] unions them and reports which
//! ids are missing from the stored payload; [`apply`] writes that union back
//! through the inventory, which also refreshes `frozen_novels` in SQLite mode.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::db::inventory::{Inventory, InventoryScope};
use crate::error::Result;

/// File-name prefix of the fragments the import leaves behind.
pub const IMPORTED_PREFIX: &str = "freeze.yaml.imported-";

/// What a repair run would change.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FreezeRepairPlan {
    /// Number of `freeze.yaml.imported-*` files found.
    pub files: usize,
    /// Files that could not be read or parsed (left untouched).
    pub unreadable: usize,
    /// Frozen ids in the fragments that the stored payload is missing.
    pub missing_ids: Vec<i64>,
}

impl FreezeRepairPlan {
    pub fn is_empty(&self) -> bool {
        self.missing_ids.is_empty()
    }
}

/// `freeze.yaml.imported-*` files under `.narou/`, in stable (name) order.
pub fn imported_freeze_files(narou_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(narou_dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(IMPORTED_PREFIX))
        })
        .collect();
    files.sort();
    files
}

/// Union of the frozen ids recorded in the fragment files. Ruby's
/// `Inventory.load("freeze")` (and this port) treats the *presence* of a key as
/// frozen, so only the ids matter here.
fn union_frozen_ids(narou_dir: &Path) -> (usize, usize, Vec<i64>) {
    let files = imported_freeze_files(narou_dir);
    let mut unreadable = 0;
    let mut ids: BTreeSet<i64> = BTreeSet::new();
    for path in &files {
        match std::fs::read_to_string(path)
            .ok()
            .and_then(|content| serde_yaml::from_str::<HashMap<i64, serde_yaml::Value>>(&content).ok())
        {
            Some(frozen) => ids.extend(frozen.into_keys()),
            None => unreadable += 1,
        }
    }
    (files.len(), unreadable, ids.into_iter().collect())
}

/// Frozen ids currently stored in the inventory.
fn stored_frozen_ids(inventory: &Inventory) -> Result<BTreeSet<i64>> {
    let frozen: HashMap<i64, serde_yaml::Value> = inventory.load("freeze", InventoryScope::Local)?;
    Ok(frozen.into_keys().collect())
}

/// Compare the fragments with the stored freeze state.
pub fn plan(inventory: &Inventory) -> Result<FreezeRepairPlan> {
    let narou_dir = inventory.root_dir().join(".narou");
    let (files, unreadable, union) = union_frozen_ids(&narou_dir);
    let stored = stored_frozen_ids(inventory)?;
    Ok(FreezeRepairPlan {
        files,
        unreadable,
        missing_ids: union
            .into_iter()
            .filter(|id| !stored.contains(id))
            .collect(),
    })
}

/// Freeze every id in `missing_ids` (union write; never unfreezes anything).
pub fn apply(inventory: &Inventory, missing_ids: &[i64]) -> Result<()> {
    if missing_ids.is_empty() {
        return Ok(());
    }
    let ids = missing_ids.to_vec();
    inventory.update_yaml::<(), HashMap<i64, serde_yaml::Value>, _>(
        "freeze",
        InventoryScope::Local,
        |mut frozen| {
            for id in ids {
                frozen.insert(id, serde_yaml::Value::Bool(true));
            }
            Ok((frozen, ()))
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn union_reads_every_fragment_and_ignores_foreign_files() {
        let temp = tempfile::tempdir().unwrap();
        let narou_dir = temp.path().join(".narou");
        write(&narou_dir.join("freeze.yaml.imported-100"), "1: true\n");
        write(
            &narou_dir.join("freeze.yaml.imported-200"),
            "2: true\n3: true\n",
        );
        write(&narou_dir.join("alias.yaml.imported-300"), "x: y\n");
        write(&narou_dir.join("freeze.yaml"), "9: true\n");

        let (files, unreadable, ids) = union_frozen_ids(&narou_dir);
        assert_eq!(files, 2, "only freeze.yaml.imported-* counts");
        assert_eq!(unreadable, 0);
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn unreadable_fragments_are_counted_not_fatal() {
        let temp = tempfile::tempdir().unwrap();
        let narou_dir = temp.path().join(".narou");
        write(&narou_dir.join("freeze.yaml.imported-100"), "1: true\n");
        write(&narou_dir.join("freeze.yaml.imported-200"), "- not: a map\n");

        let (files, unreadable, ids) = union_frozen_ids(&narou_dir);
        assert_eq!(files, 2);
        assert_eq!(unreadable, 1);
        assert_eq!(ids, vec![1]);
    }

    #[test]
    fn plan_reports_only_ids_missing_from_the_stored_payload() {
        let temp = tempfile::tempdir().unwrap();
        let narou_dir = temp.path().join(".narou");
        std::fs::create_dir_all(&narou_dir).unwrap();
        let inventory = Inventory::new(temp.path().to_path_buf());
        inventory
            .update_yaml::<(), HashMap<i64, serde_yaml::Value>, _>(
                "freeze",
                InventoryScope::Local,
                |mut frozen| {
                    frozen.insert(2, serde_yaml::Value::Bool(true));
                    Ok((frozen, ()))
                },
            )
            .unwrap();
        write(&narou_dir.join("freeze.yaml.imported-100"), "1: true\n");
        write(&narou_dir.join("freeze.yaml.imported-200"), "2: true\n");

        let repair = plan(&inventory).unwrap();
        assert_eq!(repair.files, 2);
        assert_eq!(repair.unreadable, 0);
        assert_eq!(repair.missing_ids, vec![1]);

        apply(&inventory, &repair.missing_ids).unwrap();
        let repaired: HashMap<i64, serde_yaml::Value> =
            inventory.load("freeze", InventoryScope::Local).unwrap();
        let mut ids: Vec<i64> = repaired.into_keys().collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2]);

        // Idempotent: a second run has nothing left to do.
        assert!(plan(&inventory).unwrap().is_empty());
    }
}

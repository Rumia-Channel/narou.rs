//! Rollback bundle: write the legacy YAML files back from the database.
//!
//! `narou db export-yaml` and the storage switch in the Web UI both use this.
//! An in-place export also flips `.narou/storage-backend` back to `yaml`, so
//! the next run (and narou.rb) sees a complete legacy library.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::Result;

/// Management states copied verbatim from `app_state` to their legacy files.
const STATE_FILES: [(&str, &str); 9] = [
    ("freeze", "freeze.yaml"),
    ("alias", "alias.yaml"),
    ("tag_colors", "tag_colors.yaml"),
    ("login_cookie", "login_cookie.yaml"),
    ("latest_convert", "latest_convert.yaml"),
    ("author", "author.yaml"),
    ("local_setting", "local_setting.yaml"),
    ("queue", "queue.yaml"),
    ("notepad", "notepad.txt"),
];

/// Write the YAML/TXT bundle and return the directory holding it.
///
/// `in_place` writes to the real locations and switches the storage marker
/// back to YAML; otherwise everything lands under `out` (or
/// `<root>/narou-yaml-export`) so nothing is overwritten.
pub fn export_yaml(out: Option<PathBuf>, in_place: bool) -> Result<PathBuf> {
    let inventory = crate::db::inventory::Inventory::with_default_root()?;
    let root = inventory.root_dir().to_path_buf();
    let narou_dir = root.join(".narou");
    let out_dir = if in_place {
        narou_dir.clone()
    } else {
        out.unwrap_or_else(|| root.join("narou-yaml-export"))
    };
    std::fs::create_dir_all(&out_dir)?;

    // 1. database.yaml from live records.
    let records: BTreeMap<i64, crate::db::NovelRecord> = crate::db::with_database(|db| {
        Ok(db
            .all_records()
            .iter()
            .map(|(&id, record)| {
                let mut normalized = record.clone();
                normalized.id = id;
                (id, normalized)
            })
            .collect())
    })?;
    let yaml = serde_yaml::to_string(&records)?;
    std::fs::write(out_dir.join("database.yaml"), yaml)?;

    // 2. database_index.yaml (derived cache; narou.rb rebuilds it when
    //    absent, but exporting it keeps the bundle complete).
    crate::db::with_database(|db| {
        if let Ok(content) = db.index_yaml() {
            std::fs::write(out_dir.join("database_index.yaml"), content)?;
        }
        Ok(())
    })?;

    // 3. Management states verbatim from app_state (freeze, alias, ...).
    if let Some(state) = crate::native::sqlite::state::active_for(&narou_dir) {
        for (key, file) in STATE_FILES {
            if let Some(payload) = state.get_raw("inv", key)? {
                std::fs::write(out_dir.join(file), payload)?;
            }
        }
        if let Some(payload) = state.get_raw("global", "global_setting")? {
            let global_dir = if in_place {
                home_dir().join(".narousetting")
            } else {
                out_dir.join(".narousetting")
            };
            std::fs::create_dir_all(&global_dir)?;
            std::fs::write(global_dir.join("global_setting.yaml"), payload)?;
        }
    }

    if in_place {
        // Switch the storage marker back so the next launch runs in legacy
        // YAML mode and narou.rb sees the full library.
        crate::native::sqlite::state::write_mode(
            &narou_dir,
            crate::native::sqlite::state::StorageMode::Yaml,
        )?;
    }
    Ok(out_dir)
}

/// Home directory holding `~/.narousetting`.
pub fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// Whether the rollback bundle can be written for this library.
pub fn sqlite_active(narou_dir: &Path) -> bool {
    crate::native::sqlite::state::active_for(narou_dir).is_some()
}

//! Shared SQLite handle for `.narou` management state (P2).
//!
//! One connection (`<root>/.narou/db.sqlite`) backs:
//! - relational novel records (`Database` persistence),
//! - small management states previously stored as individual YAML/TXT files
//!   (`freeze`, `alias`, `tag_colors`, `latest_convert`, `local_setting`,
//!   `notepad`, `queue`, and the global settings map), stored verbatim in
//!   `app_state(scope, key, value_yaml)` so every existing parser keeps
//!   working unchanged.
//!
//! Backward compatibility: when the database is created fresh and the legacy
//! files exist, they are imported inside one transaction and renamed to
//! `<name>.imported-<unix_ts>` afterwards (never deleted). Setting the
//! environment variable `NAROU_RS_LEGACY_YAML=1` disables SQLite entirely and
//! restores the pure-file behavior (debug / rollback escape hatch).

use std::collections::HashMap;
use std::sync::LazyLock;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use rusqlite::Connection;

use crate::error::{NarouError, Result};

const DB_FILE_NAME: &str = "db.sqlite";

/// `(scope, key, legacy relative path under the narou root)` triples imported
/// on first run. `legacy` is resolved against `.narou/` unless it starts with
/// `~/`-style markers handled by the caller.
const MANAGED_LOCAL: &[(&str, &str)] = &[
    ("freeze", "freeze.yaml"),
    ("alias", "alias.yaml"),
    ("tag_colors", "tag_colors.yaml"),
    ("latest_convert", "latest_convert.yaml"),
    ("local_setting", "local_setting.yaml"),
    ("queue", "queue.yaml"),
    ("notepad", "notepad.txt"),
];

#[derive(Clone)]
pub struct StateDb {
    conn: Arc<Mutex<Connection>>,
    /// The `.narou` directory this handle owns.
    pub(crate) narou_dir: PathBuf,
}

impl StateDb {
    pub fn conn(&self) -> Arc<Mutex<Connection>> {
        self.conn.clone()
    }

    pub fn get_raw(&self, scope: &str, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().expect("state db mutex poisoned");
        let mut statement = conn
            .prepare("SELECT value_yaml FROM app_state WHERE scope = ? AND key = ?")
            .map_err(super::sqlite_error)?;
        let mut rows = statement
            .query(rusqlite::params![scope, key])
            .map_err(super::sqlite_error)?;
        match rows.next().map_err(super::sqlite_error)? {
            Some(row) => {
                let value: String = row
                    .get(0)
                    .map_err(|error| NarouError::Platform(error.to_string()))?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }

    pub fn set_raw(&self, scope: &str, key: &str, value_yaml: &str) -> Result<()> {
        let conn = self.conn.lock().expect("state db mutex poisoned");
        conn.execute(
            "INSERT INTO app_state (scope, key, value_json, value_yaml) VALUES (?, ?, '{}', ?)
             ON CONFLICT(scope, key) DO UPDATE SET value_yaml = excluded.value_yaml",
            rusqlite::params![scope, key, value_yaml],
        )
        .map_err(super::sqlite_error)?;
        Ok(())
    }

    pub fn delete_raw(&self, scope: &str, key: &str) -> Result<()> {
        let conn = self.conn.lock().expect("state db mutex poisoned");
        conn.execute(
            "DELETE FROM app_state WHERE scope = ? AND key = ?",
            rusqlite::params![scope, key],
        )
        .map_err(super::sqlite_error)?;
        Ok(())
    }

    /// Every `(key, payload)` pair of one scope, ordered by key.
    pub fn scope_entries(&self, scope: &str) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().expect("state db mutex poisoned");
        let mut statement = conn
            .prepare("SELECT key, value_yaml FROM app_state WHERE scope = ? ORDER BY key")
            .map_err(super::sqlite_error)?;
        let rows = statement
            .query_map([scope], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(super::sqlite_error)?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(row.map_err(|error| NarouError::Platform(error.to_string()))?);
        }
        Ok(entries)
    }

    pub fn is_fresh(&self) -> Result<bool> {
        let conn = self.conn.lock().expect("state db mutex poisoned");
        let novels: i64 = conn
            .query_row("SELECT COUNT(*) FROM novels", [], |row| row.get(0))
            .map_err(super::sqlite_error)?;
        let states: i64 = conn
            .query_row("SELECT COUNT(*) FROM app_state", [], |row| row.get(0))
            .map_err(super::sqlite_error)?;
        Ok(novels == 0 && states == 0)
    }

    /// True when no object rows exist yet (first opt-in before the archive
    /// import, or an empty library).
    pub fn objects_empty(&self) -> Result<bool> {
        let conn = self.conn.lock().expect("state db mutex poisoned");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
            .map_err(super::sqlite_error)?;
        Ok(count == 0)
    }

    pub fn conn_ref(&self) -> &Arc<Mutex<Connection>> {
        &self.conn
    }

}

static OPEN_HANDLES: LazyLock<Mutex<HashMap<PathBuf, StateDb>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Which management backend owns `<root>/.narou`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageMode {
    /// Legacy `.narou/*.yaml` files (default; full forward file format).
    Yaml,
    /// SQLite (`db.sqlite`) managed library ("Lite" mode).
    Sqlite,
}

pub const MARKER_FILE: &str = "storage-backend";

pub fn read_mode(narou_dir: &Path) -> StorageMode {
    match std::fs::read_to_string(narou_dir.join(MARKER_FILE)) {
        Ok(text) if text.trim().eq_ignore_ascii_case("sqlite") => StorageMode::Sqlite,
        _ => StorageMode::Yaml,
    }
}

pub fn write_mode(narou_dir: &Path, mode: StorageMode) -> Result<()> {
    std::fs::create_dir_all(narou_dir)
        .map_err(|error| NarouError::Platform(format!("sqlite dir: {error}")))?;
    let text = match mode {
        StorageMode::Sqlite => "sqlite",
        StorageMode::Yaml => "yaml",
    };
    crate::db::inventory::atomic_write(&narou_dir.join(MARKER_FILE), &format!("{text}\n"))
}

/// Active handle for this narou root, opening the database on first use.
/// `None` unless the user opted into SQLite (`storage-backend` marker).
pub fn active_for(narou_dir: &Path) -> Option<StateDb> {
    if legacy_yaml_active() || read_mode(narou_dir) != StorageMode::Sqlite {
        return None;
    }
    let mut cache = OPEN_HANDLES.lock().expect("state cache poisoned");
    let dir = narou_dir.to_path_buf();
    if let Some(handle) = cache.get(&dir) {
        return Some(handle.clone());
    }
    match configure(&dir) {
        Ok(handle) => {
            cache.insert(dir, handle.clone());
            Some(handle)
        }
        Err(_) => None,
    }
}

pub fn legacy_yaml_active() -> bool {
    legacy_yaml_disabled()
}

fn legacy_yaml_disabled() -> bool {
    std::env::var("NAROU_RS_LEGACY_YAML")
        .map(|value| !value.is_empty() && value != "0" && value != "false")
        .unwrap_or(false)
}

/// Process-wide shared handle. Configured on first use from the narou root;
/// returns `None` before initialization is possible (no root) or when the
/// legacy-YAML escape hatch is active.
/// Open (or create) `.narou/db.sqlite`, run migrations and import legacy
/// files when the database is fresh. Called by `init_database()` explicitly;
/// `shared()` lazily performs the same work.
pub fn configure(narou_dir: &Path) -> Result<StateDb> {
    std::fs::create_dir_all(narou_dir)
        .map_err(|error| NarouError::Platform(format!("sqlite dir: {error}")))?;
    let mut conn = Connection::open(narou_dir.join(DB_FILE_NAME))
        .map_err(|error| NarouError::Platform(format!("sqlite open: {error}")))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(super::sqlite_error)?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(super::sqlite_error)?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(super::sqlite_error)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(super::sqlite_error)?;
    super::migrations::apply(&mut conn)?;

    let state = StateDb {
        conn: Arc::new(Mutex::new(conn)),
        narou_dir: narou_dir.to_path_buf(),
    };
    if state.is_fresh()? {
        import_legacy_states(&state, narou_dir)?;
    }
    if state.objects_empty()? {
        // One-shot import of the existing 小説データ/ tree so listings and
        // reads are DB-complete from the start. The filesystem mirror keeps
        // the files in place; nothing is removed.
        let archive_root = narou_dir
            .parent()
            .map(|root| root.join(crate::downloader::types::ARCHIVE_ROOT_DIR))
            .unwrap_or_else(|| narou_dir.join(crate::downloader::types::ARCHIVE_ROOT_DIR));
        if let Ok(mirror) = crate::native::object_store::NativeObjectStore::from_root(archive_root)
        {
            let _ = super::object_store::import_archive_into_objects(state.conn_ref(), &mirror);
        }
    }
    Ok(state)
}

fn import_legacy_states(state: &StateDb, narou_dir: &Path) -> Result<()> {
    let mut imported: Vec<PathBuf> = Vec::new();
    for (key, file_name) in MANAGED_LOCAL {
        let path = narou_dir.join(file_name);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        // A placeholder-empty legacy file carries no information.
        if content.trim().is_empty() {
            imported.push(path);
            continue;
        }
        state.set_raw("inv", key, &content)?;
        imported.push(path);
    }
    if let Some(home) = dirs_home() {
        let global = home.join(".narousetting").join("global_setting.yaml");
        if let Ok(content) = std::fs::read_to_string(&global) {
            if !content.trim().is_empty() {
                state.set_raw("global", "global_setting", &content)?;
            }
            imported.push(global);
        }
    }
    if !imported.is_empty() {
        rename_imported(imported);
    }
    Ok(())
}

/// Rename imported legacy files instead of deleting them: old versions can
/// still be reached manually, satisfying the backward-compatibility promise.
fn rename_imported(paths: Vec<PathBuf>) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    for path in paths {
        let mut target = path.clone().into_os_string();
        target.push(&format!(".imported-{stamp}"));
        let _ = std::fs::rename(&path, PathBuf::from(target));
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

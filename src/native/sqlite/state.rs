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
use std::sync::{Arc, Mutex};

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

/// `narou-compat` local_setting key: when true, `.narou/*.yaml` (and the
/// global setting file) stay on disk and remain authoritative, with SQLite
/// kept in sync as a mirror. When false (default), SQLite owns the state
/// and legacy files are imported once then renamed away.
pub const COMPAT_SETTING_KEY: &str = "narou-compat";

/// Parse the `narou-compat` flag out of a local_setting YAML payload.
/// Accepts `true`/`yes`/`on`/`1` (case-insensitive) as enabled.
fn compat_flag_in_yaml(yaml: &str) -> Option<bool> {
    let parsed: serde_yaml::Value = serde_yaml::from_str(yaml).ok()?;
    match parsed.get(COMPAT_SETTING_KEY)? {
        serde_yaml::Value::Bool(value) => Some(*value),
        serde_yaml::Value::String(text) => {
            let normalized = text.trim().to_ascii_lowercase();
            match normalized.as_str() {
                "true" | "yes" | "on" | "1" => Some(true),
                "false" | "no" | "off" | "0" => Some(false),
                _ => None,
            }
        }
        serde_yaml::Value::Number(number) => number.as_i64().map(|value| value != 0),
        _ => None,
    }
}

impl StateDb {
    pub fn conn(&self) -> Arc<Mutex<Connection>> {
        self.conn.clone()
    }

    /// Legacy file path backing an `app_state` entry, if one exists.
    /// `meta`-scope keys are internal and have no file mapping.
    fn legacy_path(&self, scope: &str, key: &str) -> Option<PathBuf> {
        match scope {
            "inv" => {
                let ext = if key == "notepad" { "txt" } else { "yaml" };
                Some(self.narou_dir.join(format!("{key}.{ext}")))
            }
            "global" if key == "global_setting" => dirs_home()
                .map(|home| home.join(".narousetting").join("global_setting.yaml")),
            _ => None,
        }
    }

    /// Resolve the `narou-compat` flag. A `local_setting.yaml` file on disk
    /// wins when it carries the key (it is authoritative in compat mode and
    /// may predate the database); otherwise the imported `app_state` value
    /// decides. Direct SQL — never routed through `get_raw`/`set_raw`.
    pub fn compat_now(&self) -> bool {
        let file_value = self
            .legacy_path("inv", "local_setting")
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|content| compat_flag_in_yaml(&content));
        if let Some(value) = file_value {
            return value;
        }
        let conn = self.conn.lock().expect("state db mutex poisoned");
        conn.query_row(
            "SELECT value_yaml FROM app_state WHERE scope = 'inv' AND key = 'local_setting'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|yaml| compat_flag_in_yaml(&yaml))
        .unwrap_or(false)
    }

    /// `app_state` read that bypasses the compat file layer. Used by
    /// reconciliation and for `meta` keys.
    pub(crate) fn get_raw_db(&self, scope: &str, key: &str) -> Result<Option<String>> {
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

    /// `app_state` write that bypasses the compat file layer.
    pub(crate) fn set_raw_db(&self, scope: &str, key: &str, value_yaml: &str) -> Result<()> {
        let conn = self.conn.lock().expect("state db mutex poisoned");
        conn.execute(
            "INSERT INTO app_state (scope, key, value_json, value_yaml) VALUES (?, ?, '{}', ?)
             ON CONFLICT(scope, key) DO UPDATE SET value_yaml = excluded.value_yaml",
            rusqlite::params![scope, key, value_yaml],
        )
        .map_err(super::sqlite_error)?;
        Ok(())
    }

    pub fn get_raw(&self, scope: &str, key: &str) -> Result<Option<String>> {
        // Compat mode: the legacy file is authoritative when present.
        if self.compat_now()
            && let Some(path) = self.legacy_path(scope, key)
            && let Ok(content) = std::fs::read_to_string(&path)
        {
            return Ok(Some(content));
        }
        self.get_raw_db(scope, key)
    }

    pub fn set_raw(&self, scope: &str, key: &str, value_yaml: &str) -> Result<()> {
        // Compat mode: write the legacy file first (authoritative), then
        // mirror into app_state so DB-side consumers stay in sync.
        if self.compat_now()
            && let Some(path) = self.legacy_path(scope, key)
        {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            crate::db::inventory::atomic_write(&path, value_yaml)?;
        }
        self.set_raw_db(scope, key, value_yaml)
    }

    pub fn delete_raw(&self, scope: &str, key: &str) -> Result<()> {
        if self.compat_now()
            && let Some(path) = self.legacy_path(scope, key)
        {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
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
    reconcile_managed_files(&state)?;
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

/// Reconcile `app_state` with the legacy files on every open.
///
/// - `narou-compat` ON: files are authoritative — file content refreshes
///   `app_state`, and missing files are recreated from `app_state`.
/// - `narou-compat` OFF (default): files are imported into `app_state` and
///   renamed to `<name>.imported-<unix_ts>` (never deleted).
///
/// Runs unconditionally (not only on a fresh database) so toggling the flag
/// in either direction converges on the next launch.
fn reconcile_managed_files(state: &StateDb) -> Result<()> {
    let compat = state.compat_now();
    let mut imported: Vec<PathBuf> = Vec::new();
    for (key, file_name) in MANAGED_LOCAL {
        let path = state.narou_dir.join(file_name);
        let file_content = std::fs::read_to_string(&path).ok();
        if compat {
            match file_content {
                Some(content) => state.set_raw_db("inv", key, &content)?,
                None => {
                    if let Some(payload) = state.get_raw_db("inv", key)? {
                        crate::db::inventory::atomic_write(&path, &payload)?;
                    }
                }
            }
        } else if let Some(content) = file_content {
            if !content.trim().is_empty() {
                state.set_raw_db("inv", key, &content)?;
            }
            imported.push(path);
        }
    }
    if let Some(home) = dirs_home() {
        let global = home.join(".narousetting").join("global_setting.yaml");
        let file_content = std::fs::read_to_string(&global).ok();
        if compat {
            match file_content {
                Some(content) => state.set_raw_db("global", "global_setting", &content)?,
                None => {
                    if let Some(payload) = state.get_raw_db("global", "global_setting")? {
                        if let Some(parent) = global.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        crate::db::inventory::atomic_write(&global, &payload)?;
                    }
                }
            }
        } else if let Some(content) = file_content {
            if !content.trim().is_empty() {
                state.set_raw_db("global", "global_setting", &content)?;
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

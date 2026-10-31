//! Shared SQLite handle for `.narou` management state (P2).
//!
//! One connection (`<root>/.narou/db.sqlite`) backs:
//! - relational novel records (`Database` persistence),
//! - small management states previously stored as individual YAML/TXT files
//!   (see `crate::db::state_inventory`: `freeze`, `alias`, `tag_colors`,
//!   `latest_convert`, `local_setting`, `login_cookie`, `author`, `notepad`,
//!   `queue`), stored verbatim in `app_state(scope, key, value_yaml)` so every
//!   existing parser keeps working unchanged. The global settings map stays a
//!   plain file.
//!
//! Backward compatibility: when the database is created fresh and the legacy
//! files exist, they are imported inside one transaction and renamed to
//! `<name>.imported-<unix_ts>` afterwards (never deleted). Setting the
//! environment variable `NAROU_RS_LEGACY_YAML=1` disables SQLite entirely and
//! restores the pure-file behavior (debug / rollback escape hatch).

use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use rusqlite::Connection;

use crate::db::state_inventory;
use crate::error::{NarouError, Result};

const DB_FILE_NAME: &str = "db.sqlite";

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

    /// Serialize authority changes with native readers and writers, including
    /// other processes. Always take this before YAML locks and the connection.
    pub(crate) fn with_compat_lock<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        crate::db::inventory::with_exclusive_file_lock(
            &self.narou_dir.join("compat-state"), operation,
        )
    }

    /// Legacy file backing an `app_state` entry, when that state has one
    /// (`crate::db::state_inventory` owns the list). Bookkeeping rows — the
    /// site-definition import marker, `meta` keys — have no file: they are
    /// `app_state`-only and never surface as YAML a user could edit.
    fn legacy_path(&self, scope: &str, key: &str) -> Option<PathBuf> {
        state_inventory::legacy_path(scope, key, &self.narou_dir)
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
        let conn = self.conn.lock();
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
        let conn = self.conn.lock();
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

    /// Remember both the pending projection and any existing owned projection
    /// before publishing. A second interrupted export must not forget the first.
    pub(crate) fn remember_export_file(&self, file: &str, path: &Path, content: &str) -> Result<()> {
        use sha2::Digest;
        use std::io::Read;
        let key = format!("export_file_sha:{file}");
        let known: [Option<String>; 2] = self.get_raw_db("meta", &key)?
            .map(|raw| serde_yaml::from_str(&raw)).transpose()?.unwrap_or_default();
        let mut previous = None;
        if known.iter().any(Option::is_some) {
            let existing = match std::fs::File::open(path) {
                Ok(file) => Some(file),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            if let Some(mut existing) = existing {
                let mut digest = sha2::Sha256::new();
                let mut buffer = [0u8; 8192];
                loop {
                    let count = existing.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    digest.update(&buffer[..count]);
                }
                let hash = hex::encode(digest.finalize());
                if known.iter().flatten().any(|expected| *expected == hash) {
                    previous = Some(hash);
                }
            }
        }
        let next = hex::encode(sha2::Sha256::digest(content.as_bytes()));
        if previous.as_ref() == Some(&next) {
            previous = None;
        }
        self.set_raw_db("meta", &key, &serde_yaml::to_string(&[Some(next), previous])?)
    }

    /// Finalize a successful projection; prior/intermediate hashes are only
    /// retained while publication is incomplete. Caller owns any root/file lock.
    pub(crate) fn finish_export_file(&self, file: &str, content: &str) -> Result<()> {
        use sha2::Digest;
        let hash = hex::encode(sha2::Sha256::digest(content.as_bytes()));
        self.set_raw_db("meta", &format!("export_file_sha:{file}"),
            &serde_yaml::to_string(&[Some(hash), None])?)
    }

    pub(crate) fn is_export_file(&self, file: &str, content: &str) -> Result<bool> {
        use sha2::Digest;
        let Some(raw) = self.get_raw_db("meta", &format!("export_file_sha:{file}"))? else {
            return Ok(false);
        };
        let expected: [Option<String>; 2] = serde_yaml::from_str(&raw)?;
        let hash = hex::encode(sha2::Sha256::digest(content.as_bytes()));
        Ok(expected.iter().flatten().any(|expected| *expected == hash))
    }

    /// `app_state` write that bypasses the compat file layer.
    ///
    /// The freeze payload additionally maintains the relational projection
    /// (`frozen_novels`) so the shared status search expression keeps agreeing
    /// with the inventory.
    pub(crate) fn set_raw_db(&self, scope: &str, key: &str, value_yaml: &str) -> Result<()> {
        let mut conn = self.conn.lock();
        let transaction = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(super::sqlite_error)?;
        transaction.execute(
            "INSERT INTO app_state (scope, key, value_json, value_yaml) VALUES (?, ?, '{}', ?)
             ON CONFLICT(scope, key) DO UPDATE SET value_json = excluded.value_json, value_yaml = excluded.value_yaml",
            rusqlite::params![scope, key, value_yaml],
        ).map_err(super::sqlite_error)?;
        if scope == "inv" && key == "freeze" {
            Self::project_frozen_novels(&transaction, value_yaml)?;
        }
        transaction.commit().map_err(super::sqlite_error)
    }

    /// Project the freeze payload onto `frozen_novels`.
    ///
    /// The payload is authoritative for native reads, but the shared status
    /// expression (`sql/status_search_expression.sql`, which the Worker D1
    /// adapter also uses with `frozen_novels` as its store) queries the table.
    /// An unparseable legacy payload is retained without changing the
    /// projection; the next valid freeze write repairs it.
    fn project_frozen_novels(conn: &rusqlite::Transaction<'_>, value_yaml: &str) -> Result<()> {
        let ids = frozen_ids_from_payload(value_yaml);
        let Some(ids) = ids else {
            eprintln!(
                "warning: could not parse the freeze state; frozen_novels was left unchanged"
            );
            return Ok(());
        };
        super::repository::sync_frozen_novels(conn, &ids)
    }

    /// Re-derive `frozen_novels` from the stored freeze payload.
    ///
    /// Cheap and idempotent. Called once the novel records are available: the
    /// first `reconcile_managed_files` run imports the freeze payload before
    /// `database.yaml` has populated `novels`, so that projection has nothing
    /// to attach to, and later launches do not revisit the renamed-away file.
    pub fn resync_frozen_novels(&self) -> Result<()> {
        use rusqlite::OptionalExtension;
        let mut conn = self.conn.lock();
        let transaction = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(super::sqlite_error)?;
        let payload = transaction.query_row(
            "SELECT value_yaml FROM app_state WHERE scope = 'inv' AND key = 'freeze'",
            [],
            |row| row.get::<_, String>(0),
        ).optional().map_err(super::sqlite_error)?;
        if let Some(payload) = payload {
            Self::project_frozen_novels(&transaction, &payload)?;
        }
        transaction.commit().map_err(super::sqlite_error)
    }

    pub fn get_raw(&self, scope: &str, key: &str) -> Result<Option<String>> {
        self.with_compat_lock(|| self.get_raw_unlocked(scope, key))
    }

    /// Read the current authority while the caller holds `with_compat_lock`.
    /// Used to keep an export snapshot and its publication in the same lock.
    pub(crate) fn get_raw_unlocked(&self, scope: &str, key: &str) -> Result<Option<String>> {
        if self.compat_now()
            && let Some(path) = self.legacy_path(scope, key)
            && let Some(content) = read_legacy_file(&path)?
        {
            return Ok(Some(content));
        }
        self.get_raw_db(scope, key)
    }

    /// Read, update and project the same authoritative snapshot. The closure
    /// must not access the database again. A flag change also transfers every
    /// managed state before exposing the new authority to another writer.
    pub fn update_raw<T, F>(&self, scope: &str, key: &str, update: F) -> Result<(String, T)>
    where
        F: FnOnce(&str) -> Result<(String, T)>,
    {
        self.with_compat_lock(|| {
            let compat = self.compat_now();
            let path = self.legacy_path(scope, key);
            let paths: Vec<PathBuf> = if scope == "inv" && key == "local_setting" {
                state_inventory::import_export_states().map(|(_, file)| file)
                    .chain(["database.yaml", "database_index.yaml"])
                    .map(|file| self.narou_dir.join(file)).collect()
            } else {
                path.iter().cloned().collect()
            };
            with_state_file_locks(&paths, || {
                let mut conn = self.conn.lock();
                let transaction = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(super::sqlite_error)?;
                let file_raw = if compat {
                    path.as_ref().map(|path| read_legacy_file(path)).transpose()?.flatten()
                } else {
                    None
                };
                let raw = file_raw.or(read_state_row(&transaction, scope, key)?).unwrap_or_default();
                let (content, result) = update(&raw)?;
                let next_compat = if scope == "inv" && key == "local_setting" {
                    compat_flag_in_yaml(&content).unwrap_or(false)
                } else {
                    compat
                };
                if compat != next_compat {
                    let mut mirrors = Vec::new();
                    for (state_key, file) in state_inventory::import_export_states() {
                        let path = self.narou_dir.join(file);
                        let before = read_legacy_file(&path)?;
                        let stored = read_state_row(&transaction, "inv", state_key)?;
                        let next = if state_key == "local_setting" {
                            Some(content.clone())
                        } else if compat {
                            before.clone().or(stored)
                        } else {
                            stored.or(before.clone())
                        };
                        mirrors.push(CompatMirror { key: state_key, file, path, before, next });
                    }
                    // Relational records are a separate authority, never an
                    // Inventory app_state row. Transfer them under the same
                    // cutover locks, and derive the index from that exact map.
                    let records_path = self.narou_dir.join("database.yaml");
                    let records_before = read_legacy_file(&records_path)?;
                    let mut records = super::bulk::load_all_records(&transaction)?;
                    // Settings/author CLI adapters can run before Database has
                    // performed its initial import. Match that constructor's
                    // provenance decision rather than seeding empty/stale SQL
                    // over an as-yet authoritative legacy/rollback snapshot.
                    let pending_import = if !compat && let Some(raw) = &records_before {
                        use sha2::Digest;
                        let hash = hex::encode(sha2::Sha256::digest(raw.as_bytes()));
                        let exported = read_state_row(&transaction, "meta", "export_file_sha:database.yaml")?;
                        let known: [Option<String>; 2] = exported.as_ref()
                            .map(|raw| serde_yaml::from_str(raw)).transpose()?.unwrap_or_default();
                        let owned = known.iter().flatten().any(|known| *known == hash);
                        let stored_sha = read_state_row(&transaction, "meta", "database_yaml_sha")?;
                        !raw.trim().is_empty() && !owned && if exported.is_some() {
                            true
                        } else if let Some(stored_sha) = stored_sha {
                            stored_sha != hash
                        } else {
                            records.is_empty()
                        }
                    } else {
                        false
                    };
                    if (compat || pending_import) && let Some(raw) = &records_before {
                        records = if raw.trim().is_empty() { BTreeMap::new() }
                            else { serde_yaml::from_str(raw)? };
                    }
                    for (&id, record) in &mut records { record.id = id; }
                    let records_yaml = serde_yaml::to_string(&records)?;
                    let mut index = crate::db::index_store::IndexStore::new();
                    index.reconcile(&records.iter().map(|(&id, record)| (id, record.clone())).collect());
                    let index_path = self.narou_dir.join("database_index.yaml");
                    mirrors.push(CompatMirror {
                        key: "database", file: "database.yaml", path: records_path,
                        before: records_before, next: Some(records_yaml.clone()),
                    });
                    mirrors.push(CompatMirror {
                        key: "database_index", file: "database_index.yaml", path: index_path.clone(),
                        before: read_legacy_file(&index_path)?, next: Some(index.to_yaml()?),
                    });
                    if compat || pending_import {
                        super::bulk::replace_records_conn(&transaction, &records)?;
                    }
                    use sha2::Digest;
                    let source_yaml = if compat {
                        mirrors.iter().find(|mirror| mirror.key == "database")
                            .and_then(|mirror| mirror.before.as_deref()).unwrap_or(&records_yaml)
                    } else {
                        &records_yaml
                    };
                    write_state_row(&transaction, "meta", "database_yaml_sha",
                        &hex::encode(sha2::Sha256::digest(source_yaml.as_bytes())))?;
                    // Commit ownership BEFORE publishing any file. If seeding
                    // fails or the process stops, startup must not import an
                    // unowned partial mirror over later SQLite updates. On
                    // disable, first durably synchronize the OLD file authority:
                    // even an interruption after the false flag is published
                    // cannot discard external author/freeze edits as stale.
                    for mirror in &mirrors {
                        remember_transition_file(&transaction, mirror)?;
                        if (compat || pending_import) && mirror.key != "local_setting" && mirror.inventory_state()
                            && let Some(payload) = &mirror.next
                        {
                            write_state_row(&transaction, "inv", mirror.key, payload)?;
                        }
                    }
                    transaction.commit().map_err(super::sqlite_error)?;
                    let mut published = Vec::new();
                    let publication = (|| {
                        let transaction = conn
                            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                            .map_err(super::sqlite_error)?;
                        for mirror in &mirrors {
                            if mirror.inventory_state() && let Some(payload) = &mirror.next {
                                write_state_row(&transaction, "inv", mirror.key, payload)?;
                            }
                        }
                        // Publish the mode switch last. Other native clients
                        // remain excluded by compat-state.lock until commit.
                        for mirror in mirrors.iter().filter(|m| m.key != "local_setting")
                            .chain(mirrors.iter().filter(|m| m.key == "local_setting"))
                        {
                            if (next_compat || mirror.key == "local_setting")
                                && mirror.next != mirror.before
                                && let Some(payload) = &mirror.next
                            {
                                published.push(mirror);
                                crate::db::inventory::atomic_write_locked(&mirror.path, payload)?;
                            }
                        }
                        for mirror in &mirrors {
                            let actual = if next_compat || mirror.key == "local_setting" {
                                mirror.next.as_ref()
                            } else {
                                mirror.before.as_ref()
                            };
                            if let Some(actual) = actual {
                                finish_file_row(&transaction, mirror.file, actual)?;
                            }
                        }
                        transaction.commit().map_err(super::sqlite_error)
                    })();
                    if let Err(error) = publication {
                        restore_compat_files(&published).map_err(|rollback| NarouError::Database(
                            format!("compatibility transition failed: {error}; restoring files also failed: {rollback}")
                        ))?;
                        return Err(error);
                    }
                } else {
                    // Even an unchanged file preimage may be newer than its DB
                    // mirror; always synchronize the relational consumers.
                    write_state_row(&transaction, scope, key, &content)?;
                    if compat && let Some(path) = &path {
                        crate::db::inventory::atomic_write_locked(path, &content)?;
                        if let Some(file) = path.file_name().and_then(|file| file.to_str()) {
                            finish_file_row(&transaction, file, &content)?;
                        }
                    }
                    transaction.commit().map_err(super::sqlite_error)?;
                }
                Ok((content, result))
            })
        })
    }

    pub fn set_raw(&self, scope: &str, key: &str, value_yaml: &str) -> Result<()> {
        self.update_raw(scope, key, |_| Ok((value_yaml.to_string(), ())))?;
        Ok(())
    }

    pub fn delete_raw(&self, scope: &str, key: &str) -> Result<()> {
        if scope == "inv" && key == "local_setting" {
            // Removing the flag is also a transfer back to SQLite authority.
            self.set_raw(scope, key, "{}\n")?;
        }
        self.with_compat_lock(|| {
            let path = self.legacy_path(scope, key);
            let paths: Vec<PathBuf> = path.iter().cloned().collect();
            with_state_file_locks(&paths, || {
                if self.compat_now() && let Some(path) = &path {
                    match std::fs::remove_file(path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                let mut conn = self.conn.lock();
                let transaction = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(super::sqlite_error)?;
                transaction.execute(
                    "DELETE FROM app_state WHERE scope = ? AND key = ?",
                    rusqlite::params![scope, key],
                ).map_err(super::sqlite_error)?;
                if scope == "inv" && key == "freeze" {
                    Self::project_frozen_novels(&transaction, "{}\n")?;
                }
                transaction.commit().map_err(super::sqlite_error)
            })
        })
    }


    /// Every `(key, payload)` pair of one scope, ordered by key.
    pub fn scope_entries(&self, scope: &str) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock();
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
        let conn = self.conn.lock();
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
        let conn = self.conn.lock();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
            .map_err(super::sqlite_error)?;
        Ok(count == 0)
    }

    pub fn conn_ref(&self) -> &Arc<Mutex<Connection>> {
        &self.conn
    }

    /// 呼び出し側のトランザクション内で管理状態を読む。
    ///
    /// 小説の保存と同じトランザクションで連番表も確定したいときに使う
    /// (別トランザクションにすると、作品と順序の確定がずれる)。
    pub fn read_state_row_conn(
        &self,
        tx: &rusqlite::Transaction<'_>,
        scope: &str,
        key: &str,
    ) -> Result<Option<String>> {
        read_state_row(tx, scope, key)
    }

    /// 呼び出し側のトランザクション内で管理状態を書く。
    pub fn write_state_row_conn(
        &self,
        tx: &rusqlite::Transaction<'_>,
        scope: &str,
        key: &str,
        content: &str,
    ) -> Result<()> {
        write_state_row(tx, scope, key, content)
    }

    /// 互換モードの鏡ファイルの SHA を記録する。
    ///
    /// 取り込み (`state::configure`) はこの SHA を見て file を取り込むか
    /// 判断するので、DB と鏡を同じトランザクションで更新する。
    pub fn finish_file_row_conn(
        &self,
        tx: &rusqlite::Transaction<'_>,
        file: &str,
        content: &str,
    ) -> Result<()> {
        finish_file_row(tx, file, content)
    }

}

struct CompatMirror {
    key: &'static str,
    file: &'static str,
    path: PathBuf,
    before: Option<String>,
    next: Option<String>,
}

impl CompatMirror {
    fn inventory_state(&self) -> bool {
        !matches!(self.key, "database" | "database_index")
    }
}

fn read_legacy_file(path: &Path) -> Result<Option<String>> {
    match crate::db::inventory::ensure_yaml_size_limit(path) {
        Ok(()) => {}
        Err(NarouError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_state_row(conn: &Connection, scope: &str, key: &str) -> Result<Option<String>> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT value_yaml FROM app_state WHERE scope = ? AND key = ?",
        rusqlite::params![scope, key], |row| row.get(0),
    ).optional().map_err(super::sqlite_error)
}

fn write_state_row(transaction: &rusqlite::Transaction<'_>, scope: &str, key: &str, content: &str) -> Result<()> {
    transaction.execute(
        "INSERT INTO app_state (scope, key, value_json, value_yaml) VALUES (?, ?, '{}', ?)
         ON CONFLICT(scope, key) DO UPDATE SET value_json = excluded.value_json, value_yaml = excluded.value_yaml",
        rusqlite::params![scope, key, content],
    ).map_err(super::sqlite_error)?;
    if scope == "inv" && key == "freeze" {
        StateDb::project_frozen_novels(transaction, content)?;
    }
    Ok(())
}

fn with_state_file_locks<T>(paths: &[PathBuf], operation: impl FnOnce() -> Result<T>) -> Result<T> {
    if let Some((path, rest)) = paths.split_first() {
        crate::db::inventory::with_exclusive_file_lock(path, || with_state_file_locks(rest, operation))
    } else {
        operation()
    }
}

fn remember_transition_file(transaction: &rusqlite::Transaction<'_>, mirror: &CompatMirror) -> Result<()> {
    use sha2::Digest;
    let hash = |payload: &String| hex::encode(sha2::Sha256::digest(payload.as_bytes()));
    let next = mirror.next.as_ref().map(hash);
    let previous = mirror.before.as_ref().map(hash).filter(|hash| Some(hash) != next.as_ref());
    write_state_row(transaction, "meta", &format!("export_file_sha:{}", mirror.file),
        &serde_yaml::to_string(&[next, previous])?)
}

fn finish_file_row(transaction: &rusqlite::Transaction<'_>, file: &str, content: &str) -> Result<()> {
    use sha2::Digest;
    let hash = hex::encode(sha2::Sha256::digest(content.as_bytes()));
    write_state_row(transaction, "meta", &format!("export_file_sha:{file}"),
        &serde_yaml::to_string(&[Some(hash), None])?)
}

fn restore_compat_files(published: &[&CompatMirror]) -> Result<()> {
    let mut first_error = None;
    for mirror in published.iter().rev() {
        let restored = (|| {
            if read_legacy_file(&mirror.path)? == mirror.before {
                return Ok(());
            }
            match &mirror.before {
                Some(payload) => crate::db::inventory::atomic_write_locked(&mirror.path, payload),
                None => match std::fs::remove_file(&mirror.path) {
                    Ok(()) => Ok(crate::compat::fsync_parent_dir(&mirror.path)?),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(error) => Err(error.into()),
                },
            }
        })();
        if let Err(error) = restored {
            first_error.get_or_insert(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
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

/// 実ファイルのミラーを書くか (`sqlite.mirror-files`、既定 true)。
///
/// false のときは `objects`/`object_chunks` だけが保存先になり、`小説データ/`
/// へは書かない (SORAHOST のような容量の厳しい環境向け)。読み出しは
/// SQLite だけで完結する。環境変数 `NAROU_RS_MIRROR_FILES=0` でも切れる。
pub fn mirror_files_enabled() -> bool {
    match std::env::var("NAROU_RS_MIRROR_FILES") {
        Ok(value) => crate::setting_core::flag_enabled(&value),
        Err(_) => crate::compat::load_local_setting_bool_or("sqlite.mirror-files", true),
    }
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
    let mut cache = OPEN_HANDLES.lock();
    let dir = narou_dir.to_path_buf();
    if let Some(handle) = cache.get(&dir) {
        return Some(handle.clone());
    }
    match configure(&dir) {
        Ok(handle) => {
            cache.insert(dir, handle.clone());
            Some(handle)
        }
        // SQLite was opted into (storage-backend marker) but the database
        // could not be opened — keep the legacy-YAML fallback, but surface
        // the cause instead of silently degrading.
        Err(error) => {
            eprintln!(
                "warning: sqlite storage unavailable for {}: {error}; falling back to YAML file storage",
                dir.display()
            );
            None
        }
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

/// SQLite のデータベースと `-wal` / `-shm` を所有者だけが読める権限へ締める。
///
/// `.narou/db.sqlite` には資格情報 (`login_cookie`) や作者・凍結などの管理
/// データが平文で入るため、同一ホストの他ユーザーから読めないようにする。
/// WAL と SHM にも同じ内容が載るので、存在すれば併せて 0600 にする。
/// Windows には Unix のファイルモードが無いので何もしない。
///
/// 権限変更は best effort: 読み取り専用の DB やファイルモードを変えられない
/// ファイルシステムでも、開けなくしてはいけないので失敗は警告に留める。
#[cfg(unix)]
fn restrict_sqlite_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut candidates = vec![path.to_path_buf()];
    for suffix in ["-wal", "-shm"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        candidates.push(PathBuf::from(name));
    }
    for candidate in candidates {
        match std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o600)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => eprintln!(
                "warning: could not restrict SQLite file permissions for {}: {error}",
                candidate.display()
            ),
        }
    }
}

/// Windows には Unix のファイルモードが無いので、権限操作をしない。
#[cfg(not(unix))]
fn restrict_sqlite_permissions(_path: &Path) {}

/// 挿絵の重複除去プール (`illustrations/<sha256>`) を使うか。
///
/// 挿絵の保存先が S3 (`s3.asset-backend=s3`) のとき真。保存先がローカルの
/// ときは従来どおり小説ごとの `挿絵/` 配置を使う。storage-backend の
/// YAML / SQLite は問わない (S3 上ではどちらも同じ共通プールに置ける)。
pub fn illustration_dedup_enabled() -> bool {
    crate::native::s3::illustrations_in_s3()
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
    let database = narou_dir.join(DB_FILE_NAME);
    let mut conn = Connection::open(&database)
        .map_err(|error| NarouError::Platform(format!("sqlite open: {error}")))?;
    // 既存 DB (と前回の WAL/SHM) は開いた直後に締める。
    restrict_sqlite_permissions(&database);
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
    state.with_compat_lock(|| {
        let paths: Vec<PathBuf> = state_inventory::import_export_states()
            .map(|(_, file)| state.narou_dir.join(file)).collect();
        with_state_file_locks(&paths, || reconcile_managed_files(&state))
    })?;
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
    // migration や初回取込で新しく作られた -wal / -shm もここで締め直す。
    restrict_sqlite_permissions(&database);
    Ok(state)
}

/// Reconcile `app_state` with the legacy files on every open.
///
/// - `narou-compat` ON: files are authoritative — file content refreshes
///   `app_state`, and missing files are recreated from `app_state`.
/// - `narou-compat` OFF (default): files are imported into `app_state` and
///   renamed to `<name>.imported-<unix_ts>` (never deleted). `freeze` is
///   imported as a union with the stored payload so a stale fragment file
///   cannot unfreeze other novels (see [`merge_freeze_payload`]).
///   Unchanged rollback projections are retired without importing, so a
///   failed export cannot overwrite later acknowledged SQLite writes.
///
/// Runs unconditionally (not only on a fresh database) so toggling the flag
/// in either direction converges on the next launch.
fn reconcile_managed_files(state: &StateDb) -> Result<()> {
    let compat = state.compat_now();
    let mut imported: Vec<PathBuf> = Vec::new();
    for (key, file_name) in state_inventory::import_export_states() {
        let path = state.narou_dir.join(file_name);
        // Rollback/export may have been blocked by a directory at this name.
        // It is not a legacy payload to import. Real-file read errors still
        // propagate, and compat enable remains strict because it must seed it.
        if !compat && let Ok(metadata) = std::fs::metadata(&path) && !metadata.is_file() {
            continue;
        }
        let file_content = read_legacy_file(&path)?;
        if !compat
            && let Some(content) = &file_content
            && state.is_export_file(file_name, content)?
        {
            if key == "local_setting"
                && let Some(stored) = state.get_raw_db("inv", key)?
                && compat_flag_in_yaml(&stored) == Some(true)
            {
                // A false flag published just before an interrupted commit
                // still selects SQLite on restart. Preserve newer settings,
                // but do not let retiring this owned file resurrect DB=true.
                let mut settings: HashMap<String, serde_yaml::Value> = serde_yaml::from_str(&stored)?;
                settings.insert(COMPAT_SETTING_KEY.into(), serde_yaml::Value::Bool(false));
                state.set_raw_db("inv", key, &serde_yaml::to_string(&settings)?)?;
            }
            imported.push(path);
            continue;
        }
        if compat {
            match file_content {
                Some(content) => state.set_raw_db("inv", key, &content)?,
                None => {
                    if let Some(payload) = state.get_raw_db("inv", key)? {
                        crate::db::inventory::atomic_write_locked(&path, &payload)?;
                    }
                }
            }
        } else if let Some(content) = file_content {
            if !content.trim().is_empty() {
                let payload = if key == "freeze" {
                    merge_freeze_payload(state.get_raw_db("inv", key)?.as_deref(), &content)
                } else {
                    content
                };
                state.set_raw_db("inv", key, &payload)?;
            }
            imported.push(path);
        }
    }
    if !imported.is_empty() {
        rename_imported(imported);
    }
    Ok(())
}

/// Frozen novel ids in a freeze payload (`<id>: true` entries), ascending.
/// `None` when the payload is not a mapping; callers decide whether that is
/// fatal (the SQLite projection skips it, the importer keeps it verbatim).
fn frozen_ids_from_payload(value_yaml: &str) -> Option<Vec<i64>> {
    if value_yaml.trim().is_empty() {
        return Some(Vec::new());
    }
    let parsed: HashMap<i64, serde_yaml::Value> = serde_yaml::from_str(value_yaml).ok()?;
    let mut ids: Vec<i64> = parsed.into_keys().collect();
    ids.sort_unstable();
    Some(ids)
}

/// Union a stray legacy `freeze.yaml` with the payload already in `app_state`.
///
/// Builds before the freeze-state fix wrote a fresh fragment on every `freeze`
/// run (the previous file had already been imported and renamed away), so
/// importing such a file verbatim drops every other frozen novel. A non-empty
/// freeze file only ever *adds* frozen ids — unfreezing happens through
/// `narou freeze --off` or the Web UI, never by editing the file — therefore
/// the import is a union. Falls back to the file content verbatim when either
/// side is not a mapping, preserving the pre-existing behavior.
fn merge_freeze_payload(existing: Option<&str>, incoming: &str) -> String {
    let parsed_incoming: Option<HashMap<i64, serde_yaml::Value>> = if incoming.trim().is_empty() {
        None
    } else {
        serde_yaml::from_str(incoming).ok()
    };
    let Some(mut merged) = parsed_incoming else {
        return incoming.to_string();
    };
    if let Some(existing) = existing
        && let Ok(previous) = serde_yaml::from_str::<HashMap<i64, serde_yaml::Value>>(existing)
    {
        for (id, value) in previous {
            merged.entry(id).or_insert(value);
        }
    }
    let ordered: BTreeMap<i64, serde_yaml::Value> = merged.into_iter().collect();
    crate::db::inventory::serialize_yaml_content(&ordered)
        .unwrap_or_else(|_| incoming.to_string())
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
        target.push(format!(".imported-{stamp}"));
        let _ = std::fs::rename(&path, PathBuf::from(target));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn sqlite_database_and_sidecars_are_owner_only_on_unix() {
        let temp = tempfile::tempdir().unwrap();
        let narou_dir = temp.path().join(".narou");
        let state = configure(&narou_dir).unwrap();
        // WAL / SHM を実際に存在させるため 1 回書く。
        state
            .set_raw_db("meta", "permission-check", "1\n")
            .unwrap();
        for name in [DB_FILE_NAME, "db.sqlite-wal", "db.sqlite-shm"] {
            let path = narou_dir.join(name);
            assert!(path.exists(), "{name} should exist");
            assert_eq!(mode(&path), 0o600, "{name} must be owner-only");
        }
        drop(state);
    }

    #[test]
    fn permission_restriction_ignores_missing_sidecars() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("db.sqlite");
        std::fs::write(&database, b"").unwrap();
        std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o644)).unwrap();

        // -wal / -shm が無くてもエラーにせず、ある分だけ締める。
        restrict_sqlite_permissions(&database);
        assert_eq!(mode(&database), 0o600);
    }
}

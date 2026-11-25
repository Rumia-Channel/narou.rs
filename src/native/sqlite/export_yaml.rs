//! Rollback bundle: write the legacy YAML files back from the database.
//!
//! `narou db export-yaml` and the storage switch in the Web UI both use this.
//! An in-place export also flips `.narou/storage-backend` back to `yaml`, so
//! the next run (and narou.rb) sees a complete legacy library.
//!
//! # What is written
//!
//! In SQLite mode the database — not `小説データ/` — is authoritative, and
//! `sqlite.mirror-files=false` keeps no files there at all, so a rollback has
//! to render the stored objects back into the narou.rb layout before the
//! marker flips:
//!
//! - `.narou/database.yaml` and `database_index.yaml` from the live records,
//! - the managed states (`freeze.yaml`, `alias.yaml`, …) verbatim from
//!   `app_state`, plus `global_setting.yaml` in its own directory (it stays a
//!   plain file in both modes),
//! - every stored object: `novels/…` under `小説データ/…`, user site
//!   definitions (`webnovel/<name>.yaml`) into the library `webnovel/` folder
//!   (where YAML mode reads them from — the object mirror under
//!   `小説データ/.objects/webnovel/` is never read by narou.rb), and any other
//!   namespace into `小説データ/.objects/…` like the filesystem store does,
//! - illustrations that the per-novel index references but that only exist in
//!   S3 (`s3.asset-backend=s3`; the dedup pool is not listable as a logical
//!   key). They are read back through the configured illustration backend so
//!   the rolled-back library still resolves `挿絵/<file>`.
//!
//! Files that exist on disk without a stored row (pre-import files, or writes
//! that bypassed the store) are part of the storage fallback and are exported
//! as well; nothing outside those paths is touched, so unrelated user files in
//! the library survive unchanged.
//!
//! 本文 and friends are rendered from the object store, which is the
//! authoritative working set (`novel_sections` only mirrors it). A library
//! that is already in YAML mode has nothing to render — the export then writes
//! only `database.yaml` and `database_index.yaml`.
//!
//! Without `--in-place` the same content lands in one bundle directory
//! (default `narou-yaml-export/`): the `.narou/` states at its top level (the
//! historical `--out` layout), the content under `<out>/小説データ/…`, the
//! definitions under `<out>/webnovel/` and the global setting under
//! `<out>/.narousetting/`. The storage marker is never touched in that mode.
//!
//! # Failure policy
//!
//! An in-place export is a guarded compare-and-swap cutover. One coherent
//! snapshot (records, managed states, object/gate/generation rows and the
//! live filesystem stamp set) is captured under the compat authority lock;
//! bodies are streamed into a private staging directory; finalization
//! re-validates every captured fingerprint, publishes the files and — inside
//! a single Immediate transaction that also retires the SQL namespace gates —
//! flips the marker. A concurrent update anywhere in the window fails the
//! compare and leaves SQLite authoritative instead of being silently lost.
//! A read, decode (CRC-32) or write error aborts the export the same way:
//! a corrupt database, an unreadable object or an unreachable S3
//! illustration must not be reported as a completed rollback.
//! In-place management files are atomically published with their SHA-256
//! provenance recorded first. SQLite startup retires unchanged export
//! projections without reimporting them, preserving writes made after a
//! failed rollback; genuinely edited files remain legacy import sources.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::StreamExt;

use crate::db::state_inventory;
use crate::error::{NarouError, Result};
use crate::native::object_store::{FsGate, NativeObjectStore};
use crate::platform::{
    AssetStore, AssetStream, ObjectKey, ObjectPrefix, ObjectStore,
};

use super::state::StorageMode;

/// Directory a non-in-place export defaults to (under the library root).
const DEFAULT_EXPORT_DIR: &str = "narou-yaml-export";

/// Illustration index every novel directory may carry; its entries name the
/// illustration files the section bodies refer to.
const ILLUSTRATION_CACHE_FILE: &str = ".illustration_cache.yaml";

/// Listing page size for the SQLite object walk.
const OBJECT_PAGE: usize = 1024;

static EXPORT_IN_PROGRESS: AtomicUsize = AtomicUsize::new(0);

/// True while at least one SQLite -> YAML export/cutover is active.
pub(crate) fn export_in_progress() -> bool {
    EXPORT_IN_PROGRESS.load(Ordering::Acquire) > 0
}

struct ExportInProgressGuard;

impl ExportInProgressGuard {
    fn enter() -> Self {
        EXPORT_IN_PROGRESS.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

impl Drop for ExportInProgressGuard {
    fn drop(&mut self) {
        EXPORT_IN_PROGRESS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Write the YAML/TXT bundle and return the directory holding it.
///
/// `in_place` writes to the real locations and switches the storage marker
/// back to YAML; otherwise everything lands under `out` (or
/// `<root>/narou-yaml-export`) so nothing is overwritten.
pub fn export_yaml(out: Option<PathBuf>, in_place: bool) -> Result<PathBuf> {
    run_export(export_yaml_async(out, in_place))
}

/// Async entry point. The Web switch handler already runs on a runtime and
/// awaits this directly; [`export_yaml`] drives it for synchronous callers.
pub async fn export_yaml_async(out: Option<PathBuf>, in_place: bool) -> Result<PathBuf> {
    let inventory = crate::db::inventory::Inventory::with_default_root()?;
    let root = inventory.root_dir().to_path_buf();
    let narou_dir = root.join(".narou");
    // Heal any interrupted earlier cutover before reading the mode: this is
    // the only YAML-side entrypoint that can reach the reconciler.
    reconcile_cutover_intent(&narou_dir)?;
    // Validate the authoritative backend before writing any rollback artifact.
    let sqlite_state = match crate::native::sqlite::state::read_mode(&narou_dir) {
        StorageMode::Sqlite => Some(crate::native::sqlite::state::active_for(&narou_dir).ok_or_else(|| {
            NarouError::Database(format!(
                "storage-backend は sqlite ですが {} を開けませんでした (YAML への書き出しを中止しました)",
                narou_dir.join("db.sqlite").display()
            ))
        })?),
        StorageMode::Yaml => None,
    };
    let out_dir = if in_place {
        narou_dir.clone()
    } else {
        out.unwrap_or_else(|| root.join(DEFAULT_EXPORT_DIR))
    };
    std::fs::create_dir_all(&out_dir)?;
    let layout = ExportLayout::new(&root, out_dir.clone(), in_place)?;

    let Some(state) = sqlite_state else {
        // YAML is already authoritative: only the two management files are
        // rendered, straight from the live `Database` cache.
        let (records_yaml, index_yaml) = crate::db::with_database(|db| {
            let mut records: std::collections::BTreeMap<i64, _> = db
                .all_records()
                .iter()
                .map(|(&id, record)| (id, record.clone()))
                .collect();
            for (&id, record) in &mut records {
                record.id = id;
            }
            let mut index = crate::db::index_store::IndexStore::new();
            index.reconcile(&records.iter().map(|(&id, record)| (id, record.clone())).collect());
            Ok((serde_yaml::to_string(&records)?, index.to_yaml()?))
        })?;
        write_state_file(None, &out_dir, "database.yaml", &records_yaml)?;
        write_state_file(None, &out_dir, "database_index.yaml", &index_yaml)?;
        return Ok(out_dir);
    };

    let export_state = (in_place || comparable(&out_dir) == comparable(&narou_dir))
        .then_some(state.clone());

    let _export_guard = ExportInProgressGuard::enter();

    if in_place {
        let narou_dir_for_sweep = narou_dir.clone();
        crate::native::object_store::run_blocking(move || sweep_stale_staging(&narou_dir_for_sweep)).await?;
    }

    // Phase 1 — coherent capture. The compat authority lock serializes with
    // record/state writers, and one deferred transaction pins the SQL side,
    // so every published byte comes from the same consistent snapshot.
    let snapshot = {
        let state = state.clone();
        let narou_dir = narou_dir.clone();
        let library_archive = layout.library_archive.clone();
        crate::native::object_store::run_blocking(move || {
            state.with_compat_lock(|| {
                CutoverSnapshot::capture(&state, &narou_dir, &library_archive)
            })
        })
        .await?
    };

    // Phase 2 — stream bodies. In place they are prepared privately (one
    // staging directory inside `.narou`, renamed into position only after the
    // finalization re-validates the snapshot); bundle exports write their
    // final paths directly. The primary connection mutex is never held or
    // re-locked across this asynchronous payload IO.
    let staging_dir = in_place.then(|| staging_dir(&narou_dir));
    let staged = export_objects(&state, &layout, &root, staging_dir.as_deref()).await;
    let staged = match staged {
        Ok(staged) => staged,
        Err(error) => {
            if let Some(dir) = &staging_dir {
                let _ = std::fs::remove_dir_all(dir);
            }
            return Err(error);
        }
    };

    // Phase 3 — guarded finalization / bundle publication.
    let result = {
        let state = state.clone();
        let narou_dir = narou_dir.clone();
        let layout = layout.clone();
        let out_dir_for_phase3 = out_dir.clone();
        crate::native::object_store::run_blocking(move || {
            let result = if in_place {
                finalize_in_place(
                    &state,
                    &narou_dir,
                    &layout,
                    &snapshot,
                    &staged,
                    export_state.as_ref(),
                )
            } else {
                publish_metadata(&out_dir_for_phase3, &layout, &snapshot, export_state.as_ref())
            };
            if let Some(dir) = staging_dir {
                let _ = std::fs::remove_dir_all(dir);
            }
            result
        })
        .await
    };
    result.map(|()| out_dir)
}

/// One deterministic fingerprint over the rows a rollback publishes or
/// transfers authority for. Content `generation` is a random per-write nonce,
/// so even a write later reverted byte-for-byte still breaks the compare —
/// exactly what a compare-and-swap cutover needs.
fn sql_fingerprint(conn: &rusqlite::Connection) -> Result<String> {
    use sha2::Digest;
    let mut hash = sha2::Sha256::new();
    let mut feed = |part: &str| {
        hash.update(part.as_bytes());
        hash.update([0u8]);
    };
    let table_exists = |conn: &rusqlite::Connection, name: &str| -> Result<bool> {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
            [name],
            |row| row.get(0),
        )
        .map_err(super::sqlite_error)
    };
    let mut walk = |conn: &rusqlite::Connection, sql: &str| -> Result<()> {
        let mut statement = conn.prepare(sql).map_err(super::sqlite_error)?;
        let mut rows = statement.query([]).map_err(super::sqlite_error)?;
        while let Some(row) = rows.next().map_err(super::sqlite_error)? {
            let value: String = row.get(0).map_err(super::sqlite_error)?;
            feed(&value);
        }
        Ok(())
    };
    if table_exists(conn, "objects")? {
        walk(
            conn,
            "SELECT object_key||char(0)||size||char(0)||generation FROM objects ORDER BY object_key",
        )?;
    }
    for (table, sql) in [
        ("transfer_object_refs", "SELECT object_key||char(0)||blob_key||char(0)||blob_generation||char(0)||size||char(0)||sha256||char(0)||backend FROM transfer_object_refs ORDER BY object_key"),
        ("novel_access_gates", "SELECT prefix||char(0)||epoch||char(0)||closed||char(0)||coalesce(owner,'')||char(0)||content_revision||char(0)||origin_url_fold FROM novel_access_gates ORDER BY prefix"),
        ("novel_gate_bindings", "SELECT novel_id||char(0)||prefix FROM novel_gate_bindings ORDER BY novel_id"),
        ("novel_revisions", "SELECT novel_id||char(0)||record_revision||char(0)||content_revision FROM novel_revisions ORDER BY novel_id"),
    ] {
        if table_exists(conn, table)? {
            walk(conn, sql)?;
        }
    }
    Ok(hex::encode(hash.finalize()))
}

/// `(size, mtime-nanos)` of one live archive/definition file. Export targets
/// keep no `objects` row (the filesystem fallback), so a mid-export edit to
/// them can only be detected by comparing the file stamp before the staged
/// body replaces it.
type FileStamp = (u64, i128);

fn file_stamp(path: &Path) -> Result<Option<FileStamp>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() {
        return Ok(None);
    }
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|span| span.as_nanos() as i128)
        .unwrap_or(-1);
    Ok(Some((metadata.len(), mtime)))
}

/// Paths the fingerprint must never see: the live transfer namespace (gate
/// files, lock files, stage ledgers, sessions — they mutate under every lock
/// we take) except committed `.narou-transfer/.refs/` markers (those are
/// published remote-alias metadata and cutover targets), `.narou-staging/`
/// stage payloads, per-file `.lock` siblings, and the export's own
/// temporary/staging names.
fn fingerprint_ignored(relative: &str) -> bool {
    if relative
        .strip_prefix(".objects/")
        .is_some_and(|rest| rest.starts_with(crate::native::object_store::REMOTE_REFERENCE_PREFIX))
    {
        return false;
    }
    let mut parts = relative.split('/');
    if parts.any(|part| part == ".narou-transfer" || part == ".narou-staging") {
        return true;
    }
    let name = relative.rsplit('/').next().unwrap_or(relative);
    name.ends_with(".lock") || name.starts_with(".narou-export-")
}

fn is_internal_archive_dir(relative: &str) -> bool {
    relative == ".objects/.narou-staging"
        || relative.starts_with(".objects/.narou-staging/")
        || relative
            .strip_prefix(".objects/.narou-transfer/")
            .is_some_and(|rest| rest != ".refs" && !rest.starts_with(".refs/"))
}

fn fingerprint_ignored_dir(relative: &str) -> bool {
    if is_internal_archive_dir(relative) {
        return true;
    }
    relative != ".objects/.narou-transfer"
        && !relative.starts_with(".objects/.narou-transfer/")
        && relative
            .split('/')
            .any(|part| part == ".narou-transfer" || part == ".narou-staging")
}

fn fingerprint_tree(base: &Path, prefix: &str, out: &mut BTreeMap<String, Option<FileStamp>>) -> Result<()> {
    if !base.is_dir() {
        return Ok(());
    }
    let canonical_base = crate::db::paths::ensure_within_archive_root(base, base)?;
    let mut files = Vec::new();
    NativeObjectStore::recursive_files_filtered(
        &canonical_base,
        &canonical_base,
        &mut files,
        &mut |safe_root, path, is_dir| {
            let Ok(relative) = path.strip_prefix(safe_root) else {
                return false;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            if is_dir {
                return fingerprint_ignored_dir(&relative);
            }
            fingerprint_ignored(&relative)
        },
    )?;
    for path in files {
        let Ok(relative) = path.strip_prefix(&canonical_base) else { continue };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if fingerprint_ignored(&relative) {
            continue;
        }
        out.insert(format!("{prefix}{relative}"), file_stamp(&path)?);
    }
    Ok(())
}

/// Filesystem side of the export, keyed by library-root-relative names (the
/// same names staged renames are checked against): every live file under the
/// archive root and the `webnovel/` definition directory, plus the exact
/// `.narou`/`~/.narousetting` files publication overwrites. Those singles are
/// stamped by name rather than by tree walk so `db.sqlite`, lock files and
/// staging residue — all of which legitimately change during an export —
/// never enter the compare.
fn collect_file_stamps(root: &Path, narou_dir: &Path, library_archive: &Path) -> Result<BTreeMap<String, Option<FileStamp>>> {
    let mut map = BTreeMap::new();
    fingerprint_tree(
        library_archive,
        &format!("{}/", crate::downloader::types::ARCHIVE_ROOT_DIR),
        &mut map,
    )?;
    fingerprint_tree(&root.join("webnovel"), "webnovel/", &mut map)?;
    for file in ["database.yaml", "database_index.yaml"] {
        map.insert(format!(".narou/{file}"), file_stamp(&narou_dir.join(file))?);
    }
    for (_key, file) in state_inventory::import_export_states() {
        map.insert(format!(".narou/{file}"), file_stamp(&narou_dir.join(file))?);
    }
    map.insert(
        ".narousetting/global_setting.yaml".to_string(),
        file_stamp(&crate::db::inventory::global_setting_dir(root).join("global_setting.yaml"))?,
    );
    Ok(map)
}

/// The coherent point-in-time the whole cutover is validated against.
struct CutoverSnapshot {
    /// Serialized records for `database.yaml`.
    records_yaml: String,
    /// Reconciled index for `database_index.yaml` (derived, not compared).
    index_yaml: String,
    /// `(state key, file name, payload)` for every managed import/export state.
    states: Vec<(String, &'static str, Option<String>)>,
    /// `global/global_setting` payload (stays a plain file in both modes).
    global_setting: Option<String>,
    /// Deterministic hash of the published SQL tables.
    sql_fingerprint: String,
    /// Live fallback/mirror files: `root-relative path -> stamp`.
    fs_fingerprint: BTreeMap<String, Option<FileStamp>>,
}

impl CutoverSnapshot {
    /// Capture under the compat authority lock. The records file is the
    /// authority in `narou-compat` mode; otherwise `app_state`/relational
    /// rows are. Reads share one deferred transaction so the SQL parts are a
    /// single consistent snapshot; the connection mutex is released before
    /// the filesystem walk.
    fn capture(
        state: &super::state::StateDb,
        narou_dir: &Path,
        library_archive: &Path,
    ) -> Result<Self> {
        let mut conn = state.conn_ref().lock();
        let tx = conn.transaction().map_err(super::sqlite_error)?;
        let compat = state.compat_now_with_connection(&tx);

        let source = narou_dir.join("database.yaml");
        let file_raw = if compat {
            match crate::db::inventory::ensure_yaml_size_limit(&source) {
                Ok(()) => Some(std::fs::read_to_string(&source)?),
                Err(NarouError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        let mut records: std::collections::BTreeMap<i64, _> = if let Some(raw) = file_raw {
            if raw.trim().is_empty() {
                std::collections::BTreeMap::new()
            } else {
                serde_yaml::from_str(&raw)?
            }
        } else {
            super::bulk::load_all_records(&tx)?
        };
        for (&id, record) in &mut records {
            record.id = id;
        }
        let records_yaml = serde_yaml::to_string(&records)?;
        let mut index = crate::db::index_store::IndexStore::new();
        index.reconcile(&records.into_iter().collect());
        let index_yaml = index.to_yaml()?;

        let mut states = Vec::new();
        for (key, file) in state_inventory::import_export_states() {
            let payload = if compat
                && let Some(path) = state_inventory::legacy_path("inv", key, narou_dir)
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                Some(content)
            } else {
                state.read_state_row_conn(&tx, "inv", key)?
            };
            states.push((key.to_string(), file, payload));
        }
        let global_setting = state.read_state_row_conn(&tx, "global", "global_setting")?;
        let sql_fingerprint = sql_fingerprint(&tx)?;
        drop(tx);
        drop(conn);

        let root = narou_dir.parent().unwrap_or(narou_dir).to_path_buf();
        let fs_fingerprint = collect_file_stamps(&root, narou_dir, library_archive)?;
        Ok(Self {
            records_yaml,
            index_yaml,
            states,
            global_setting,
            sql_fingerprint,
            fs_fingerprint,
        })
    }

    /// Re-derive every compared piece inside `tx` and fail the cutover on the
    /// first difference — the snapshot another writer committed over can
    /// never be selected by the marker flip.
    fn validate(
        &self,
        state: &super::state::StateDb,
        tx: &rusqlite::Transaction<'_>,
        narou_dir: &Path,
    ) -> Result<()> {
        let changed = |what: &str| {
            NarouError::Database(format!(
                "書き出し中に{what}が更新されたため切り替えを中止しました (SQLite 管理を継続します)"
            ))
        };
        let compat = state.compat_now_with_connection(tx);
        let source = narou_dir.join("database.yaml");
        let file_raw = if compat {
            match crate::db::inventory::ensure_yaml_size_limit(&source) {
                Ok(()) => Some(std::fs::read_to_string(&source)?),
                Err(NarouError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        let mut records: std::collections::BTreeMap<i64, _> = if let Some(raw) = file_raw {
            if raw.trim().is_empty() {
                std::collections::BTreeMap::new()
            } else {
                serde_yaml::from_str(&raw)?
            }
        } else {
            super::bulk::load_all_records(tx)?
        };
        for (&id, record) in &mut records {
            record.id = id;
        }
        if serde_yaml::to_string(&records)? != self.records_yaml {
            return Err(changed("小説レコード"));
        }
        for (key, _, payload) in &self.states {
            let current = if compat
                && let Some(path) = state_inventory::legacy_path("inv", key, narou_dir)
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                Some(content)
            } else {
                state.read_state_row_conn(tx, "inv", key.as_str())?
            };
            if &current != payload {
                return Err(changed("管理状態"));
            }
        }
        if state.read_state_row_conn(tx, "global", "global_setting")? != self.global_setting {
            return Err(changed("グローバル設定"));
        }
        if sql_fingerprint(tx)? != self.sql_fingerprint {
            return Err(changed("保存オブジェクト"));
        }
        Ok(())
    }
}

/// Write `database.yaml`, `database_index.yaml`, the managed states and the
/// global setting into the export directory with SHA-256 provenance.
fn publish_metadata(
    out_dir: &Path,
    layout: &ExportLayout,
    snapshot: &CutoverSnapshot,
    export_state: Option<&super::state::StateDb>,
) -> Result<()> {
    write_state_file(export_state, out_dir, "database.yaml", &snapshot.records_yaml)?;
    write_state_file(export_state, out_dir, "database_index.yaml", &snapshot.index_yaml)?;
    for (_, file, payload) in &snapshot.states {
        if let Some(payload) = payload {
            write_state_file(export_state, out_dir, file, payload)?;
        }
    }
    // Global settings are shared, file-only state; do not hold the library
    // authority lock while taking their independent file lock.
    if let Some(payload) = &snapshot.global_setting {
        let global_dir = layout.global_setting_dir();
        std::fs::create_dir_all(&global_dir)?;
        crate::db::inventory::atomic_write(&global_dir.join("global_setting.yaml"), payload)?;
    }
    Ok(())
}

/// `.narou`-local directory the in-place export stages object bodies into.
/// Same filesystem as every target, so publication is a rename.
fn staging_dir(narou_dir: &Path) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|span| span.as_nanos())
        .unwrap_or(0);
    narou_dir.join(format!(".export-staging-{}-{nonce}", std::process::id()))
}

/// Staged location of one export target: the library-root-relative path
/// mirrored under the staging root.
fn staged_path(staging: &Path, library_root: &Path, target: &Path) -> Result<PathBuf> {
    let canonical_root = comparable(library_root);
    let canonical_target = comparable(target);
    let relative = canonical_target
        .strip_prefix(&canonical_root)
        .map_err(|_| {
            NarouError::Database(format!(
                "書き出し先がライブラリの外にあります: {}",
                target.display()
            ))
        })?;
    Ok(staging.join(relative))
}

/// Remove staging directories an interrupted earlier run left behind. Their
/// contents are uncommitted copies, never the only copy of anything.
fn sweep_stale_staging(narou_dir: &Path) -> Result<()> {
    let entries = match std::fs::read_dir(narou_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(".export-staging-") && entry.path().is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

/// Guarded in-place finalization: re-validate the snapshot, publish metadata,
/// move staged bodies into position, then — inside one Immediate transaction
/// — journal the durable `yaml_pending` intent, retire every SQL gate and
/// fsync the flipped marker before COMMIT. The intent row is the crash
/// evidence: it exists committed only when the flipped marker was already
/// durable, so a restart can always tell "committed but unfinished" apart
/// from "rolled back" (see reconcile_cutover_intent).
fn finalize_in_place(
    state: &super::state::StateDb,
    narou_dir: &Path,
    layout: &ExportLayout,
    snapshot: &CutoverSnapshot,
    staged: &[(PathBuf, PathBuf)],
    export_state: Option<&super::state::StateDb>,
) -> Result<()> {
    // `SqliteObjectStore::new` resolves `sqlite.mirror-files` through
    // `Inventory`, which routes `local_setting` back through
    // `StateDb::get_raw` — itself a `with_compat_lock` acquisition. Building
    // the store inside the compat lock therefore deadlocks on the same
    // process mutex. Adoption only ever needs the shared connection and the
    // mirror root, so construct both before the guarded region opens.
    let mirror = NativeObjectStore::from_root(layout.library_archive.clone())?;
    let store = super::object_store::SqliteObjectStore::new(state.conn_ref().clone(), mirror.clone());
    state.with_compat_lock(|| {
        // Validation 1: metadata and SQL fingerprint under one read
        // transaction. The compat lock excludes record/state writers; object
        // and gate writers are fenced by the Immediate transaction below.
        {
            let mut conn = state.conn_ref().lock();
            let tx = conn.transaction().map_err(super::sqlite_error)?;
            snapshot.validate(state, &tx, narou_dir)?;
        }
        // The filesystem half changes only outside SQL, so compare it before
        // the publishes/renames below (and per-file again at rename time).
        let root = narou_dir.parent().unwrap_or(narou_dir).to_path_buf();
        if collect_file_stamps(&root, narou_dir, &layout.library_archive)? != snapshot.fs_fingerprint {
            return Err(NarouError::Database(
                "書き出し中に実ファイルが更新されたため切り替えを中止しました (SQLite 管理を継続します)"
                    .to_string(),
            ));
        }
        publish_metadata(narou_dir, layout, snapshot, export_state)?;

        // Move staged bodies into place, each under its target's file lock
        // with the captured stamp re-checked so a fallback file edited
        // mid-export is never silently overwritten.
        let mut synced_parents = BTreeSet::new();
        for (staged_path, target) in staged {
            let canonical_target = comparable(target);
            let canonical_root = comparable(&root);
            let relative = canonical_target
                .strip_prefix(&canonical_root)
                .map(|path| path.to_string_lossy().replace('\\', "/"))
                .map_err(|_| {
                    NarouError::Database(format!(
                        "書き出し先がライブラリの外にあります: {}",
                        target.display()
                    ))
                })?;
            crate::db::inventory::with_exclusive_file_lock(target, || {
                if file_stamp(target)? != snapshot.fs_fingerprint.get(&relative).copied().flatten() {
                    return Err(NarouError::Database(format!(
                        "書き出し中に {relative} が更新されたため切り替えを中止しました (SQLite 管理を継続します)"
                    )));
                }
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::rename(staged_path, target)?;
                Ok(())
            })?;
            if let Some(parent) = target.parent() {
                synced_parents.insert(parent.to_path_buf());
            }
        }
        for parent in &synced_parents {
            crate::compat::fsync_dir(parent)?;
        }

        let prefixes: Vec<String> = {
            let conn = state.conn_ref().lock();
            let mut statement = conn
                .prepare("SELECT prefix FROM novel_access_gates ORDER BY prefix")
                .map_err(super::sqlite_error)?;
            let mut rows = statement.query([]).map_err(super::sqlite_error)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(super::sqlite_error)? {
                out.push(row.get(0).map_err(super::sqlite_error)?);
            }
            out
        };
        let intent_path = narou_dir.join(CUTOVER_INTENT_FILE);
        // Durable in-flight marker: written before the barrier transaction so
        // a process exit at any later point is unambiguous on restart
        // (reconcile_cutover_intent). Removed only after convergence.
        crate::db::inventory::atomic_write(&intent_path, "sqlite->yaml cutover in flight\n")?;

        let mut committed = false;
        let cutover = mirror.with_cutover_lock(|| -> Result<()> {
            let mut conn = state.conn_ref().lock();
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(super::sqlite_error)?;
            // Durable commit evidence: `yaml_pending` exists committed only
            // when the flipped marker was already fsync'd (it is written
            // after the marker below and before COMMIT).
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS cutover_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL) WITHOUT ROWID;",
            )
            .map_err(super::sqlite_error)?;
            // Validation 2 under the write barrier: nothing committed since
            // validation 1 may pass the flip.
            if sql_fingerprint(&tx)? != snapshot.sql_fingerprint {
                return Err(NarouError::Database(
                    "書き出し中に保存オブジェクトが更新されたため切り替えを中止しました (SQLite 管理を継続します)"
                        .to_string(),
                ));
            }
            let mut adopted: Vec<(ObjectPrefix, FsGate)> = Vec::new();
            for prefix_key in &prefixes {
                let prefix = ObjectPrefix::new(prefix_key.clone())?;
                if let Some(gate) = store
                    .adopt_sql_gate_in_tx_nosync(&tx, &prefix)
                    .map_err(|error| {
                        NarouError::Platform(format!("FS gate adoption failed for {prefix_key}: {error}"))
                    })?
                {
                    adopted.push((prefix, gate));
                }
            }
            if !prefixes.is_empty() {
                mirror.sync_pending_gate_dir()?;
            }
            // The marker flips inside the write barrier so the first SQLite
            // commit that can land after it is ordered after YAML authority.
            // write_mode is atomic+fsync'd: after this line the selector file
            // durably reads yaml even if the process dies before COMMIT, so
            // the intent file + pending row are what startup reconciliation
            // uses to tell a committed flip from a rolled-back one.
            crate::native::sqlite::state::write_mode(narou_dir, StorageMode::Yaml)?;
            tx.execute(
                "INSERT OR REPLACE INTO cutover_meta(key,value) VALUES('yaml_pending','1')",
                [],
            )
            .map_err(super::sqlite_error)?;
            tx.commit().map_err(super::sqlite_error)?;
            committed = true;
            if !adopted.is_empty() {
                // Mass adoption: one seqlock transition for the whole batch
                // instead of an exclusive lock-and-fsync cycle per gate file
                // (issue #58; 1200-file cutovers hit Windows handle contention
                // and stalling otherwise). The seqlock stays odd until the
                // batch finishes, so the scan never caches a half-written gate
                // directory.
                let batch = crate::native::object_store::begin_gate_revision_batch(store.archive_root())?;
                for (prefix, gate) in &adopted {
                    store.install_adopted_gate_batch(prefix, gate, &conn, &batch).map_err(|error| {
                        NarouError::Platform(format!(
                            "FS gate finalization failed for {}: {error}",
                            prefix.as_ref()
                        ))
                    })?;
                }
                mirror.sync_gate_dir()?;
                batch.finish()?;
                for (prefix, _) in &adopted {
                    mirror.remove_pending_gate_nosync(prefix).map_err(|error| {
                        NarouError::Platform(format!(
                            "FS gate finalization failed for {}: {error}",
                            prefix.as_ref()
                        ))
                    })?;
                }
                mirror.sync_pending_gate_dir()?;
            }
            // Converged: discharge the in-flight intent evidence (best-effort;
            // the reconciler removes it too if this process dies first).
            let _ = std::fs::remove_file(&intent_path);
            let _ = conn.execute("DELETE FROM cutover_meta WHERE key='yaml_pending'", []);
            Ok(())
        });
        if let Err(error) = cutover {
            if !committed {
                // The barrier tx rolled back, but the fsync'd marker may
                // already name yaml: restore the SQLite selector, drain any
                // `.gate.pending` journals the rolled-back tx staged
                // (provenance-carrying journals fail closed in the drain),
                // discharge the never-committed row — and only then remove
                // the intent file, so a crash mid-unwind is reconcilable.
                if crate::native::sqlite::state::read_mode(narou_dir) == StorageMode::Yaml {
                    let _ = crate::native::sqlite::state::write_mode(narou_dir, StorageMode::Sqlite);
                }
                {
                    // Resumes committed journals and drops unproven staged
                    // ones; best-effort — the restart reconciler repeats it.
                    let _ = super::object_store::reconcile_pending_gate_journals(
                        &mirror,
                        state.conn_ref(),
                    );
                    let guard = state.conn_ref().lock();
                    let _ = guard.execute("DELETE FROM cutover_meta WHERE key='yaml_pending'", []);
                }
                let _ = std::fs::remove_file(&intent_path);
            }
            // committed: the flip is durable — marker=yaml is the correct
            // selector and the intent evidence stays for the reconciler to
            // finish journal canonicalization on the next entrypoint.
            return Err(error);
        }
        Ok(())
    })
}

/// Durable intent file naming an in-flight SQLite->YAML flip. It is created
/// before the barrier transaction and removed only after the marker flip is
/// fully converged; its presence is what makes a crashed flip distinguishable
/// from an ordinary YAML-mode library (which never has this file).
const CUTOVER_INTENT_FILE: &str = ".cutover-yaml-intent";

/// Startup reconciliation for an interrupted in-place cutover. `configure`
/// and `NativeStore::for_narou_root` run it before the store variant /
/// namespace-gate authority is chosen; `export_yaml_async` runs it before
/// re-validating mode so a re-run heals the same state. It opens the
/// database itself so it works under either marker.
///
/// Evidence protocol (all durable):
/// - `.narou/.cutover-yaml-intent` exists only while a flip is in flight.
/// - `cutover_meta.yaml_pending` commits only inside the barrier
///   transaction, after the fsync'd `write_mode(Yaml)` call — so a committed
///   row proves the tx (gate retirement + marker) committed.
/// - `.gate.pending` journals prove a committed adoption whose FS gate
///   canonicalization did not finish.
///
/// Matrix (intent present):
/// - marker=yaml + pending row: committed flip, unfinished finalization —
///   resume leftover `.gate.pending` journals via the shared sweep, then
///   discharge the intent evidence.
/// - marker=yaml + no pending row: the marker was written but the tx rolled
///   back — restore the SQLite selector (the published YAML files are
///   ignored under SQLite; re-running the switch redoes them) and drain
///   unproven staged journals, failing closed on provenance conflicts.
/// - marker=sqlite: rolled back or already restored — converge leftover
///   journals like the sweep, discharge stale intent evidence, drop the
///   intent file last.
///
/// No intent file -> no-op, so converged and never-switched libraries are
/// untouched.
pub(crate) fn reconcile_cutover_intent(narou_dir: &Path) -> Result<()> {
    let intent_path = narou_dir.join(CUTOVER_INTENT_FILE);
    if !intent_path.is_file() {
        return Ok(());
    }
    let db_path = narou_dir.join("db.sqlite");
    if !db_path.is_file() {
        let _ = std::fs::remove_file(&intent_path);
        return Ok(());
    }
    let conn = rusqlite::Connection::open(&db_path).map_err(super::sqlite_error)?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    let pending_row = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='cutover_meta')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(super::sqlite_error)?
        && conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM cutover_meta WHERE key='yaml_pending')",
                [],
                |row| row.get(0),
            )
            .map_err(super::sqlite_error)?;
    let marker_yaml = crate::native::sqlite::state::read_mode(narou_dir) == StorageMode::Yaml;
    if marker_yaml && pending_row {
        // Committed flip: finish any journaled FS-gate installations and
        // resume recorded handoffs that never reached a journal (crash
        // between COMMIT and the journal write), then discharge the intent
        // evidence. Same routine the gate sweep runs.
        let archive_root = narou_dir
            .parent()
            .map(|root| root.join(crate::downloader::types::ARCHIVE_ROOT_DIR))
            .unwrap_or_else(|| narou_dir.join(crate::downloader::types::ARCHIVE_ROOT_DIR));
        let mirror = NativeObjectStore::from_root(archive_root)?;
        let shared = Arc::new(parking_lot::Mutex::new(conn));
        super::object_store::reconcile_pending_gate_journals(&mirror, &shared)?;
        let guard = shared.lock();
        let _ = guard.execute("DELETE FROM cutover_meta WHERE key='yaml_pending'", []);
        drop(guard);
        let _ = std::fs::remove_file(&intent_path);
    } else if marker_yaml {
        // Marker flipped but the barrier tx rolled back: SQL authority was
        // never retired — restore the original selector. Journals staged
        // inside that rolled-back transaction carry no provenance and must
        // not shadow the live SQL rows on the next run; any journal that
        // DOES have recorded provenance fails closed in the rollback.
        crate::native::sqlite::state::write_mode(narou_dir, StorageMode::Sqlite)?;
        let archive_root = narou_dir
            .parent()
            .map(|root| root.join(crate::downloader::types::ARCHIVE_ROOT_DIR))
            .unwrap_or_else(|| narou_dir.join(crate::downloader::types::ARCHIVE_ROOT_DIR));
        let mirror = NativeObjectStore::from_root(archive_root)?;
        super::object_store::rollback_unproven_pending_journals(&mirror, Some(&conn))?;
        let _ = std::fs::remove_file(&intent_path);
    } else {
        // Rolled back or already restored: converge leftover journals the
        // same way the gate sweep does (committed resumes, unproven drops),
        // discharge stale intent evidence, then drop the intent file last.
        let archive_root = narou_dir
            .parent()
            .map(|root| root.join(crate::downloader::types::ARCHIVE_ROOT_DIR))
            .unwrap_or_else(|| narou_dir.join(crate::downloader::types::ARCHIVE_ROOT_DIR));
        let mirror = NativeObjectStore::from_root(archive_root)?;
        let shared = Arc::new(parking_lot::Mutex::new(conn));
        super::object_store::reconcile_pending_gate_journals(&mirror, &shared)?;
        let guard = shared.lock();
        let _ = guard.execute("DELETE FROM cutover_meta WHERE key='yaml_pending'", []);
        drop(guard);
        let _ = std::fs::remove_file(&intent_path);
    }
    Ok(())
}

fn write_state_file(
    state: Option<&super::state::StateDb>,
    directory: &Path,
    file: &str,
    content: &str,
) -> Result<()> {
    let path = directory.join(file);
    crate::db::inventory::with_exclusive_file_lock(&path, || {
        if let Some(state) = state {
            state.remember_export_file(file, &path, content)?;
        }
        crate::db::inventory::atomic_write_locked(&path, content)?;
        if let Some(state) = state {
            state.finish_export_file(file, content)?;
        }
        Ok(())
    })
}

/// Where the exported halves go: the real library (`in_place`) or one bundle
/// directory.
#[derive(Clone)]
struct ExportLayout {
    /// The library's real `小説データ/`: reads (and the filesystem fallback)
    /// come from here even when the export itself writes into a bundle.
    library_archive: PathBuf,
    /// Where user site definitions are written.
    definitions_dir: PathBuf,
    /// `~/.narousetting` (in place) or the bundle's `.narousetting`.
    global_setting_dir: PathBuf,
    /// Filesystem store rooted at the export's archive root; `resolve_path`
    /// maps `novels/…` onto the legacy layout and every other namespace under
    /// `.objects/`.
    write_root: NativeObjectStore,
}

impl ExportLayout {
    fn new(root: &Path, out_dir: PathBuf, in_place: bool) -> Result<Self> {
        let library_archive = root.join(crate::downloader::types::ARCHIVE_ROOT_DIR);
        let archive_root = if in_place {
            library_archive.clone()
        } else {
            // A bundle inside `小説データ/` would be re-imported as library
            // content on the next start; refuse instead of corrupting both.
            if comparable(&out_dir).starts_with(comparable(&library_archive)) {
                return Err(NarouError::Database(format!(
                    "出力先が {} の中にあります: {}",
                    crate::downloader::types::ARCHIVE_ROOT_DIR,
                    out_dir.display()
                )));
            }
            out_dir.join(crate::downloader::types::ARCHIVE_ROOT_DIR)
        };
        Ok(Self {
            library_archive,
            definitions_dir: if in_place {
                root.join("webnovel")
            } else {
                out_dir.join("webnovel")
            },
            global_setting_dir: if in_place {
                crate::db::inventory::global_setting_dir(root)
            } else {
                out_dir.join(".narousetting")
            },
            write_root: NativeObjectStore::from_root(archive_root)?,
        })
    }

    fn global_setting_dir(&self) -> PathBuf {
        self.global_setting_dir.clone()
    }

    /// File a logical key is written back to.
    fn target_path(&self, key: &ObjectKey) -> Result<PathBuf> {
        // `webnovel/<name>.yaml` is the one namespace YAML mode does not read
        // from the archive tree: site definitions live next to the library.
        if let Some(name) = key.as_ref().strip_prefix("webnovel/")
            && !name.contains('/')
            && name.ends_with(".yaml")
        {
            return Ok(self.definitions_dir.join(name));
        }
        self.write_root.resolve_path(key)
    }
}

/// Resolve the nearest existing ancestor before comparing a new output path.
/// Windows canonicalization adds a verbatim prefix, so mixing an existing
/// root with a missing target's lexical spelling would reject valid children.
fn comparable(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() { path.to_path_buf() } else {
        std::env::current_dir().map(|cwd| cwd.join(path)).unwrap_or_else(|_| path.to_path_buf())
    };
    let mut ancestor = absolute.as_path();
    loop {
        if let Ok(mut canonical) = std::fs::canonicalize(ancestor) {
            let suffix = absolute.strip_prefix(ancestor).expect("ancestor of the same output path");
            if !suffix.as_os_str().is_empty() { canonical.push(suffix); }
            return canonical;
        }
        let Some(parent) = ancestor.parent() else { return absolute; };
        ancestor = parent;
    }
}

/// Render every stored object and the illustrations the per-novel indexes
/// reference back into the export layout.
async fn export_objects(
    state: &crate::native::sqlite::state::StateDb,
    layout: &ExportLayout,
    library_root: &Path,
    staging: Option<&Path>,
) -> Result<Vec<(PathBuf, PathBuf)>> {
    let library_mirror = NativeObjectStore::from_root(layout.library_archive.clone())?;
    let store =
        super::object_store::SqliteObjectStore::new(state.conn_ref().clone(), library_mirror);
    let (mut staged, indexed, seen) =
        write_stored_objects(&store, layout, library_root, staging).await?;
    let backend = illustration_backend()?;
    staged.extend(
        write_indexed_illustrations(
            &store,
            layout,
            library_root,
            staging,
            &seen,
            indexed,
            backend.as_deref(),
        )
        .await?,
    );
    Ok(staged)
}

/// The configured illustration backend, or `None` when illustrations live in
/// the local store.
///
/// An S3 illustration backend that cannot be built must not silently produce a
/// bundle without the illustrations it holds (fail closed), but a local-only
/// library never touches S3 at all.
fn illustration_backend() -> Result<Option<Arc<dyn AssetStore>>> {
    if !crate::native::s3::illustrations_in_s3() {
        return Ok(None);
    }
    let Some(store) = crate::native::s3::illustration_store()? else {
        return Ok(None);
    };
    let store: Arc<dyn AssetStore> = store;
    Ok(Some(store))
}

/// Write every listed object to its legacy path and collect the per-novel
/// illustration indexes (used for the S3-only files).
///
/// Reserved namespaces (`.narou-staging/`, `.narou-transfer/`) are not user
/// content: they carry live stage data and would otherwise land on top of
/// the transfer gate files `resolve_path` maps them to. Under `staging` each
/// body is streamed into that private directory and the `(staged, target)`
/// pair is collected for the atomic finalization rename; without it bodies
/// are written to their final paths directly.
#[allow(clippy::type_complexity)]
async fn write_stored_objects(
    store: &super::object_store::SqliteObjectStore,
    layout: &ExportLayout,
    library_root: &Path,
    staging: Option<&Path>,
) -> Result<(
    Vec<(PathBuf, PathBuf)>,
    Vec<(ObjectKey, Vec<String>)>,
    BTreeSet<String>,
)> {
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut indexes: Vec<(ObjectKey, Vec<String>)> = Vec::new();

    // Phase A: SQLite objects paged by primary key (zero filesystem walks).
    let mut cursor: Option<String> = None;
    loop {
        let page = crate::native::object_store::run_blocking({
            let store = store.clone();
            let cursor = cursor.clone();
            move || store.list_sqlite_objects_page(cursor.as_deref(), OBJECT_PAGE)
        })
        .await?;
        let rows = page.objects;
        let next_cursor = page.next_cursor;
        let mut page_entries: Vec<(ObjectKey, u64, PathBuf)> = Vec::with_capacity(rows.len());
        for (key, size) in rows {
            if !seen.insert(key.as_ref().to_string()) {
                continue;
            }
            // Remote-reference markers (.narou-transfer/.refs/<logical>) are
            // published metadata the FS/YAML backend reads verbatim — they
            // must cross the cutover. Every other reserved namespace
            // (sessions, stage ledgers, locks, .data blobs) stays out.
            if crate::platform::is_reserved_key(&key)
                && !crate::native::object_store::is_remote_reference_key(&key)
            {
                continue;
            }
            if let Some(prefix) = index_prefix(&key) {
                let Some(payload) = store.read_small(&key).await? else {
                    return Err(NarouError::Database(format!(
                        "挿絵索引を読み出せません: {key}"
                    )));
                };
                let index: crate::illustration_store::IllustrationIndex =
                    serde_yaml::from_slice(&payload).map_err(|error| {
                        NarouError::Database(format!("挿絵索引 {key} を読めません: {error}"))
                    })?;
                indexes.push((
                    ObjectKey::try_new(prefix.to_string())?,
                    index.filenames().into_iter().collect(),
                ));
            }
            let target = layout.target_path(&key)?;
            page_entries.push((key, size, target));
        }

        if let Some(staging_dir) = staging {
            let verdicts = if page_entries.is_empty() {
                Vec::new()
            } else {
                crate::native::object_store::run_blocking({
                    let store = store.clone();
                    let items = page_entries.clone();
                    move || store.check_inline_targets_batch(&items)
                })
                .await?
            };
            for ((key, size, target), verdict) in page_entries.into_iter().zip(verdicts) {
                match verdict {
                    Some(true) => continue,
                    Some(false) => {}
                    None => match asset_matches_existing_file(store, &key, Some(size), &target).await? {
                        Some(true) => continue,
                        Some(false) => {}
                        None => {
                            return Err(NarouError::Database(format!(
                                "書き出すべきオブジェクトを読み出せません: {key}"
                            )));
                        }
                    },
                }
                let path = staged_path(staging_dir, library_root, &target)?;
                if !write_asset(store, &key, &path).await? {
                    return Err(NarouError::Database(format!(
                        "書き出すべきオブジェクトを読み出せません: {key}"
                    )));
                }
                staged.push((path, target));
            }
        } else {
            for (key, _, target) in page_entries {
                if !write_asset(store, &key, &target).await? {
                    return Err(NarouError::Database(format!(
                        "書き出すべきオブジェクトを読み出せません: {key}"
                    )));
                }
            }
        }

        match next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }

    // Phase B: single O(N) filesystem scan for pre-import / un-imported fallback files.
    if layout.library_archive.is_dir() {
        let files = crate::native::object_store::run_blocking({
            let archive = layout.library_archive.clone();
            move || {
                let mut files = Vec::new();
                NativeObjectStore::recursive_files_filtered(
                    &archive,
                    &archive,
                    &mut files,
                    &mut |safe_root, path, is_dir| {
                        let Ok(relative) = path.strip_prefix(safe_root) else {
                            return false;
                        };
                        let relative = relative.to_string_lossy().replace('\\', "/");
                        if is_dir {
                            return is_internal_archive_dir(&relative);
                        }
                        if fingerprint_ignored(&relative) {
                            return true;
                        }
                        let name = relative.rsplit('/').next().unwrap_or(&relative);
                        if name.starts_with(".narou-asset-") {
                            return true;
                        }
                        relative.starts_with(".objects/")
                            && name.starts_with(crate::native::object_store::ATOMIC_OBJECT_TEMP_PREFIX)
                    },
                )?;
                files.sort();
                Ok(files)
            }
        })
        .await?;

        for path in files {
            let Some(key) =
                crate::native::object_store::logical_key_for_native_path(&layout.library_archive, &path)
            else {
                continue;
            };
            if !seen.insert(key.as_ref().to_string()) {
                continue;
            }
            if crate::platform::is_reserved_key(&key)
                && !crate::native::object_store::is_remote_reference_key(&key)
            {
                continue;
            }
            if let Some(prefix) = index_prefix(&key) {
                let payload = std::fs::read(&path)?;
                let index: crate::illustration_store::IllustrationIndex =
                    serde_yaml::from_slice(&payload).map_err(|error| {
                        NarouError::Database(format!("挿絵索引 {key} を読めません: {error}"))
                    })?;
                indexes.push((
                    ObjectKey::try_new(prefix.to_string())?,
                    index.filenames().into_iter().collect(),
                ));
            }
            let target = layout.target_path(&key)?;
            if staging.is_some() && comparable(&target) == comparable(&path) {
                continue;
            }
            if staging.is_some()
                && matches!(
                    asset_matches_existing_file(store, &key, None, &target).await?,
                    Some(true)
                )
            {
                continue;
            }
            let dest = match staging {
                Some(dir) => staged_path(dir, library_root, &target)?,
                None => target.clone(),
            };
            if !write_asset(store, &key, &dest).await? {
                if crate::native::object_store::is_remote_reference_key(&key) {
                    let bytes = std::fs::read(&path)?;
                    let stream = Box::pin(futures::stream::once(async move { Ok(bytes) }));
                    write_stream_to_path(stream, &dest).await?;
                } else {
                    return Err(NarouError::Database(format!(
                        "書き出すべきオブジェクトを読み出せません: {key}"
                    )));
                }
            }
            if staging.is_some() {
                staged.push((dest, target));
            }
        }
    }

    Ok((staged, indexes, seen))
}

/// Illustrations the index names but the listing does not carry.
///
/// With `s3.asset-backend=s3` the dedup pool keeps the bytes under a physical
/// name the logical listing hides, so the index is the only reference. A name
/// with no file in the local store is read from the illustration backend; a
/// name with no file anywhere is skipped (the index is metadata and does not
/// guarantee the blob), while a failed read aborts the export.
async fn write_indexed_illustrations(
    store: &super::object_store::SqliteObjectStore,
    layout: &ExportLayout,
    library_root: &Path,
    staging: Option<&Path>,
    seen: &BTreeSet<String>,
    indexes: Vec<(ObjectKey, Vec<String>)>,
    backend: Option<&dyn AssetStore>,
) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (prefix, names) in indexes {
        for name in names {
            let key = crate::platform::illustration_key(&prefix, &name)?;
            if seen.contains(key.as_ref()) {
                continue;
            }
            let target = layout.target_path(&key)?;
            let path = match staging {
                Some(dir) => staged_path(dir, library_root, &target)?,
                None => target.clone(),
            };
            if staging.is_some()
                && matches!(
                    asset_matches_existing_file(store, &key, None, &target).await?,
                    Some(true)
                )
            {
                continue;
            }
            if write_asset(store, &key, &path).await? {
                if staging.is_some() {
                    staged.push((path, target));
                }
                continue;
            }
            let Some(backend) = backend else { continue };
            if staging.is_some()
                && matches!(
                    asset_matches_existing_file(backend, &key, None, &target).await?,
                    Some(true)
                )
            {
                continue;
            }
            if write_asset(backend, &key, &path).await? && staging.is_some() {
                staged.push((path, target));
            }
        }
    }
    Ok(staged)
}

/// `<novel prefix>` of a `.illustration_cache.yaml` object key.
fn index_prefix(key: &ObjectKey) -> Option<&str> {
    let prefix = key.as_ref().strip_suffix(ILLUSTRATION_CACHE_FILE)?;
    let prefix = prefix.strip_suffix('/')?;
    (!prefix.is_empty()).then_some(prefix)
}

/// Verify whether `target` already exists with identical decoded bytes (and
/// full CRC-32 validation of the stored stream). Returns `Ok(None)` when the
/// store has no object for `key`.
async fn asset_matches_existing_file(
    store: &dyn AssetStore,
    key: &ObjectKey,
    expected_size: Option<u64>,
    target: &Path,
) -> Result<Option<bool>> {
    let Ok(meta) = std::fs::metadata(target) else {
        return Ok(Some(false));
    };
    if !meta.is_file() || expected_size.is_some_and(|size| meta.len() != size) {
        return Ok(Some(false));
    }
    let Some(mut stream) = store
        .read_stream(key)
        .await
        .map_err(|error| export_error(key, error))?
    else {
        return Ok(None);
    };
    let mut file = match std::fs::File::open(target) {
        Ok(file) => file,
        Err(_) => return Ok(Some(false)),
    };
    use std::io::Read;
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| export_error(key, error))?;
        buf.resize(chunk.len(), 0);
        if file.read_exact(&mut buf).is_err() || buf != chunk {
            return Ok(Some(false));
        }
    }
    let mut tail = [0u8; 1];
    Ok(Some(matches!(file.read(&mut tail), Ok(0))))
}

/// Stream one stored object into `path`, replacing any previous file
/// atomically. Returns `false` when the store does not know the key.
async fn write_asset(store: &dyn AssetStore, key: &ObjectKey, path: &Path) -> Result<bool> {
    let Some(stream) = store
        .read_stream(key)
        .await
        .map_err(|error| export_error(key, error))?
    else {
        return Ok(false);
    };
    write_stream_to_path(stream, path)
        .await
        .map_err(|error| export_error(key, error))?;
    Ok(true)
}

/// Name the object an export failure came from: the CLI and the Web switch
/// both surface this message, and "some payload is corrupt" is not actionable.
fn export_error(key: &ObjectKey, error: NarouError) -> NarouError {
    match error {
        NarouError::Database(message) => NarouError::Database(format!("{key}: {message}")),
        other => NarouError::Database(format!("{key}: {other}")),
    }
}

/// Drain one asset stream into a temporary file next to `path` and move it
/// into place, so an interrupted write never leaves a half-written library
/// file behind (the chunks are never held in memory as a whole).
async fn write_stream_to_path(mut stream: AssetStream, path: &Path) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        NarouError::Database(format!("書き出し先に親ディレクトリがありません: {}", path.display()))
    })?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".narou-export-")
        .tempfile_in(parent)?;
    while let Some(chunk) = stream.next().await {
        temporary.write_all(&chunk?)?;
    }
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// Drive the async export from synchronous callers. A caller that already runs
/// on a runtime (the Web handler) gets a dedicated thread, so the blocking
/// object walk never stalls the runtime's worker threads.
fn run_export<T>(future: impl Future<Output = Result<T>> + Send) -> Result<T>
where
    T: Send,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        return std::thread::scope(|scope| {
            let worker = std::thread::Builder::new()
                .name("yaml-export".to_string())
                .spawn_scoped(scope, move || run_export(future))
                .map_err(|error| NarouError::Platform(error.to_string()))?;
            worker
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        });
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| NarouError::Platform(error.to_string()))?;
    runtime.block_on(future)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::mocks::MemoryObjectStore;
    use std::sync::Arc;

    #[test]
    fn interrupted_second_publication_preserves_later_acknowledged_state() {
        let temp = tempfile::tempdir().unwrap();
        let managed = temp.path().join(".narou");
        let state = super::super::state::configure(&managed).unwrap();
        state.set_raw("inv", "notepad", "before").unwrap();
        write_state_file(Some(&state), &managed, "notepad.txt", "before").unwrap();
        state.set_raw("inv", "notepad", "after").unwrap();

        // A second attempt commits its provenance, then stops before replacing
        // the first projection (process interruption or failed atomic publish).
        state.remember_export_file("notepad.txt", &managed.join("notepad.txt"), "second").unwrap();
        let reopened = super::super::state::configure(&managed).unwrap();
        assert_eq!(reopened.get_raw("inv", "notepad").unwrap().as_deref(), Some("after"));
    }

    fn temp_layout(in_place: bool, dir: &Path) -> ExportLayout {
        let root = dir.join("library");
        std::fs::create_dir_all(&root).unwrap();
        ExportLayout::new(&root, dir.join("bundle"), in_place).unwrap()
    }

    fn root_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("library")
    }

    fn memory_store(layout: &ExportLayout) -> super::super::object_store::SqliteObjectStore {
        let conn = {
            let mut conn = rusqlite::Connection::open_in_memory().unwrap();
            crate::native::sqlite::migrations::apply(&mut conn).unwrap();
            conn
        };
        let mirror = NativeObjectStore::from_root(layout.library_archive.clone()).unwrap();
        super::super::object_store::SqliteObjectStore::with_mirror_files(
            Arc::new(parking_lot::Mutex::new(conn)),
            mirror,
            false,
        )
    }

    /// Every stored namespace reaches its legacy path: content under the
    /// archive root, definitions next to the library, other namespaces under
    /// `.objects/`.
    #[test]
    fn objects_render_into_the_legacy_layout() {
        let dir = tempfile::tempdir().unwrap();
        let layout = temp_layout(true, dir.path());
        let archive = layout.write_root.root();
        assert_eq!(
            layout
                .target_path(&ObjectKey::try_new("novels/site/title/toc.yaml").unwrap())
                .unwrap(),
            archive.join("site").join("title").join("toc.yaml")
        );
        assert_eq!(
            layout
                .target_path(&ObjectKey::try_new("webnovel/example.com.yaml").unwrap())
                .unwrap(),
            dir.path().join("library").join("webnovel").join("example.com.yaml")
        );
        assert_eq!(
            layout
                .target_path(&ObjectKey::try_new("generated/epub/book.epub").unwrap())
                .unwrap(),
            archive.join(".objects").join("generated").join("epub").join("book.epub")
        );
        // A nested `webnovel/…` key is not a definition file: it stays in the
        // archive tree instead of leaking subdirectories into `webnovel/`.
        assert_eq!(
            layout
                .target_path(&ObjectKey::try_new("webnovel/挿絵/0001.png").unwrap())
                .unwrap(),
            archive.join(".objects").join("webnovel").join("挿絵").join("0001.png")
        );
    }

    /// A bundle inside `小説データ/` would be imported as content on the next
    /// start, so the export refuses it.
    #[test]
    fn bundle_inside_the_archive_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("library");
        let inside = root.join(crate::downloader::types::ARCHIVE_ROOT_DIR).join("export");
        std::fs::create_dir_all(&inside).unwrap();
        let error = ExportLayout::new(&root, inside, false).err().expect("unsafe export is refused");
        assert!(error.to_string().contains("出力先"), "{error}");
    }

    /// The object walk writes database rows and filesystem-only files into the
    /// legacy layout and reports the index contents for the S3 pass.
    #[test]
    fn stored_objects_are_rendered_to_the_legacy_layout() {
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let layout = temp_layout(true, dir.path());
            let archive = layout.write_root.root().to_path_buf();
            let store = memory_store(&layout);
            let keys = crate::platform::NovelObjectKeys::new("site", "title", false).unwrap();

            ObjectStore::write_small(&store, &keys.toc(), b"toc\n".to_vec())
                .await
                .unwrap();
            ObjectStore::write_small(&store, &keys.setting(), b"ini\n".to_vec())
                .await
                .unwrap();
            ObjectStore::write_small(
                &store,
                &keys.illustration_cache(),
                b"sources:\n  https://example.com/0002.png: 0002.png\n".to_vec(),
            )
            .await
            .unwrap();
            // A file written outside the store (pre-import / legacy writer).
            let legacy = archive.join("site/title/挿絵/0001.png");
            std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            std::fs::write(&legacy, b"legacy").unwrap();

            let (staged, indexes, seen) = write_stored_objects(&store, &layout, &root_path(&dir), None)
                .await
                .unwrap();
            assert!(staged.is_empty(), "no staging directory means direct writes");
            assert_eq!(
                std::fs::read(archive.join("site/title/toc.yaml")).unwrap(),
                b"toc\n"
            );
            assert_eq!(
                std::fs::read(archive.join("site/title/setting.ini")).unwrap(),
                b"ini\n"
            );
            assert_eq!(std::fs::read(&legacy).unwrap(), b"legacy");
            assert_eq!(indexes.len(), 1);
            assert_eq!(indexes[0].0.as_ref(), "novels/site/title");
            assert_eq!(indexes[0].1, vec!["0002.png".to_string()]);
            assert!(seen.contains("novels/site/title/挿絵/0001.png"));
        });
    }

    /// Illustrations named by the index but absent from the local store are
    /// materialized from the illustration backend; names with no file anywhere
    /// are skipped instead of failing the rollback.
    #[test]
    fn indexed_illustrations_are_materialized_from_the_backend() {
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let layout = temp_layout(true, dir.path());
            let archive = layout.write_root.root().to_path_buf();
            let store = memory_store(&layout);
            let prefix = ObjectKey::try_new("novels/site/title").unwrap();
            let pooled = crate::platform::illustration_key(&prefix, "0002.png").unwrap();
            let backend = MemoryObjectStore::new();
            crate::platform::AssetStore::write_stream(
                &backend,
                &pooled,
                Box::pin(futures::stream::iter(vec![Ok(b"image".to_vec())])),
            )
            .await
            .unwrap();

            let indexes = vec![(
                prefix.clone(),
                vec!["0002.png".to_string(), "upstream.png".to_string()],
            )];
            let backend_ref: &dyn AssetStore = &backend;
            let staged = write_indexed_illustrations(
                &store,
                &layout,
                &root_path(&dir),
                None,
                &BTreeSet::new(),
                indexes,
                Some(backend_ref),
            )
            .await
            .unwrap();
            assert!(staged.is_empty());
            assert_eq!(
                std::fs::read(archive.join("site/title/挿絵/0002.png")).unwrap(),
                b"image"
            );
            assert!(!archive.join("site/title/挿絵/upstream.png").exists());

            // In-place (`staging = Some`): identical existing target skips staging,
            // while missing target still falls back to `backend`.
            let staging_root = root_path(&dir).join(".narou/.export-staging-test");
            crate::platform::AssetStore::write_stream(
                &backend,
                &crate::platform::illustration_key(&prefix, "0003.png").unwrap(),
                Box::pin(futures::stream::iter(vec![Ok(b"fresh".to_vec())])),
            )
            .await
            .unwrap();
            let staged = write_indexed_illustrations(
                &store,
                &layout,
                &root_path(&dir),
                Some(&staging_root),
                &BTreeSet::new(),
                vec![(prefix, vec!["0002.png".to_string(), "0003.png".to_string()])],
                Some(backend_ref),
            )
            .await
            .unwrap();
            assert_eq!(staged.len(), 1);
            assert_eq!(staged[0].1, archive.join("site/title/挿絵/0003.png"));
            assert_eq!(std::fs::read(&staged[0].0).unwrap(), b"fresh");
        });
    }

    /// Defines the files the marker switch must not lose: an object whose
    /// payload cannot be read fails the export instead of producing a partial
    /// bundle.
    #[test]
    fn unreadable_objects_abort_the_export() {
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let layout = temp_layout(true, dir.path());
            let store = memory_store(&layout);
            let key = ObjectKey::try_new("novels/site/title/toc.yaml").unwrap();
            ObjectStore::write_small(&store, &key, b"toc\n".to_vec())
                .await
                .unwrap();
            // Corrupt the stored CRC-32 so the payload fails verification.
            {
                let conn = store.conn_handle();
                let guard = conn.lock();
                guard
                    .execute(
                        "UPDATE objects SET crc32 = 0 WHERE object_key = ?",
                        rusqlite::params![key.as_ref()],
                    )
                    .unwrap();
            }
            let error = write_stored_objects(&store, &layout, &root_path(&dir), None).await.unwrap_err();
            assert!(error.to_string().contains("toc.yaml"), "{error}");
        });
    }
    /// The in-place cutover publishes staged bodies under the library root.
    #[test]
    fn staged_bodies_wait_for_their_legacy_targets() {
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let layout = temp_layout(true, dir.path());
            let staging = dir.path().join("staging");
            let store = memory_store(&layout);
            let key = ObjectKey::try_new("novels/site/title/toc.yaml").unwrap();
            ObjectStore::write_small(&store, &key, b"toc\n".to_vec())
                .await
                .unwrap();

            let (staged, _, _) =
                write_stored_objects(&store, &layout, &root_path(&dir), Some(&staging))
                    .await
                    .unwrap();
            assert_eq!(staged.len(), 1);
            let (prepared, target) = &staged[0];
            assert_eq!(*target, layout.target_path(&key).unwrap());
            assert_eq!(std::fs::read(prepared).unwrap(), b"toc\n");
            assert!(
                !target.exists(),
                "the body must not land before finalization validates the snapshot"
            );
        });
    }

    /// Transfer/staging namespaces are not library content: they are never
    /// rendered into the export (which would also clobber the live gate
    /// files `resolve_path` maps them to).
    #[test]
    fn reserved_namespace_objects_are_skipped() {
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let layout = temp_layout(true, dir.path());
            let store = memory_store(&layout);
            let canonical = ObjectKey::try_new("novels/site/title/toc.yaml").unwrap();
            ObjectStore::write_small(&store, &canonical, b"toc\n".to_vec())
                .await
                .unwrap();
            for reserved in [".narou-transfer/.data/blob-x", ".narou-staging/y"] {
                ObjectStore::write_small(
                    &store,
                    &ObjectKey::try_new(reserved).unwrap(),
                    b"internal".to_vec(),
                )
                .await
                .unwrap();
            }

            let (staged, _, _) =
                write_stored_objects(&store, &layout, &root_path(&dir), None)
                    .await
                    .unwrap();
            assert!(staged.is_empty());
            let archive = layout.write_root.root();
            assert_eq!(
                std::fs::read(archive.join("site/title/toc.yaml")).unwrap(),
                b"toc\n"
            );
            assert!(
                !archive.join(".objects/.narou-transfer/.data/blob-x").exists(),
                "transfer internals never reach the export"
            );
        });
    }

    /// A canonical reference (`transfer_object_refs`) resolves to its
    /// physical blob, so the exported file carries the imported immutable
    /// bytes; a blob whose generation no longer matches the recorded
    /// reference aborts instead of exporting stale content.
    #[test]
    fn canonical_references_export_blob_bytes_and_tombstones_abort() {
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let layout = temp_layout(true, dir.path());
            let store = memory_store(&layout);
            let blob = ObjectKey::try_new(".narou-transfer/.data/blob-9f").unwrap();
            let canonical = ObjectKey::try_new("novels/site/title/payload.bin").unwrap();
            ObjectStore::write_small(&store, &blob, b"canonical-bytes".to_vec())
                .await
                .unwrap();
            let generation: String = {
                let conn = store.conn_handle();
                conn.lock()
                    .query_row(
                        "SELECT generation FROM objects WHERE object_key=?",
                        [blob.as_ref()],
                        |row| row.get(0),
                    )
                    .unwrap()
            };
            {
                let conn = store.conn_handle();
                conn.lock()
                    .execute(
                        "INSERT INTO transfer_object_refs(object_key,blob_key,size,sha256,backend,updated_at,blob_generation) VALUES(?,?,?,?,'d1',?,?)",
                        rusqlite::params![canonical.as_ref(), blob.as_ref(), 15i64, "00", "t", generation],
                    )
                    .unwrap();
            }

            let staging = dir.path().join("staging");
            let (staged, _, _) = write_stored_objects(
                &store,
                &layout,
                &root_path(&dir),
                Some(&staging),
            )
            .await
            .unwrap();
            assert_eq!(staged.len(), 1);
            assert_eq!(
                std::fs::read(&staged[0].0).unwrap(),
                b"canonical-bytes",
                "the reference resolves to the exact immutable blob"
            );

            // A tombstoned reference (blob generation no longer matches) must
            // fail the export rather than silently emit other bytes.
            {
                let conn = store.conn_handle();
                conn.lock()
                    .execute(
                        "UPDATE objects SET generation='deadbeef' WHERE object_key=?",
                        [blob.as_ref()],
                    )
                    .unwrap();
            }
            let error =
                write_stored_objects(&store, &layout, &root_path(&dir), None).await.unwrap_err();
            assert!(
                error.to_string().contains("generation") || error.to_string().contains("payload.bin"),
                "{error}"
            );
        });
    }

    /// A write committed between snapshot capture and finalization must stop
    /// the marker flip, never be silently lost: the export fails closed and
    /// SQLite stays authoritative with its gates intact.
    #[test]
    fn concurrent_update_between_capture_and_finalize_aborts_cutover() {
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let root = root_path(&dir);
            std::fs::create_dir_all(&root).unwrap();
            let narou_dir = root.join(".narou");
            crate::native::sqlite::state::write_mode(&narou_dir, StorageMode::Sqlite).unwrap();
            let state = super::super::state::configure(&narou_dir).unwrap();
            let layout = ExportLayout::new(&root, narou_dir.clone(), true).unwrap();
            let mirror = NativeObjectStore::from_root(layout.library_archive.clone()).unwrap();
            let store = super::super::object_store::SqliteObjectStore::with_mirror_files(
                state.conn_ref().clone(),
                mirror,
                false,
            );
            let key = ObjectKey::try_new("novels/site/title/toc.yaml").unwrap();
            ObjectStore::write_small(&store, &key, b"before".to_vec())
                .await
                .unwrap();

            let snapshot = state
                .with_compat_lock(|| {
                    CutoverSnapshot::capture(&state, &narou_dir, &layout.library_archive)
                })
                .unwrap();
            let staging = dir.path().join("staging");
            let staged = export_objects(&state, &layout, &root, Some(&staging))
                .await
                .unwrap();
            assert_eq!(staged.len(), 1, "the body waits in private staging");
            assert!(!layout.target_path(&key).unwrap().exists());

            // A writer commits inside the cutover window.
            ObjectStore::write_small(&store, &key, b"after".to_vec())
                .await
                .unwrap();

            let error = finalize_in_place(
                &state,
                &narou_dir,
                &layout,
                &snapshot,
                &staged,
                Some(&state),
            )
            .unwrap_err();
            assert!(error.to_string().contains("切り替えを中止"), "{error}");
            assert_eq!(
                crate::native::sqlite::state::read_mode(&narou_dir),
                StorageMode::Sqlite,
                "a failed cutover keeps the original mode"
            );
            assert!(
                !layout.target_path(&key).unwrap().exists(),
                "the aborted cutover must not publish the stale body"
            );
        });
    }

    /// Successful in-place finalization flips the marker and retires every
    /// open SQL gate into a proven FS gate in one transaction; a closed gate
    /// blocks the whole cutover instead of being partially adopted.
    #[test]
    fn finalization_adopts_open_gates_and_refuses_closed_ones() {
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let root = root_path(&dir);
            std::fs::create_dir_all(&root).unwrap();
            let narou_dir = root.join(".narou");
            crate::native::sqlite::state::write_mode(&narou_dir, StorageMode::Sqlite).unwrap();
            let state = super::super::state::configure(&narou_dir).unwrap();
            let layout = ExportLayout::new(&root, narou_dir.clone(), true).unwrap();
            let mirror = NativeObjectStore::from_root(layout.library_archive.clone()).unwrap();
            let store = super::super::object_store::SqliteObjectStore::with_mirror_files(
                state.conn_ref().clone(),
                mirror.clone(),
                false,
            );
            let key = ObjectKey::try_new("novels/site/title/toc.yaml").unwrap();
            ObjectStore::write_small(&store, &key, b"toc".to_vec())
                .await
                .unwrap();
            {
                let conn = state.conn_ref().lock();
                conn.execute(
                    "INSERT INTO novel_access_gates(prefix,epoch,closed,owner,updated_at,content_revision,origin_url_fold) VALUES('novels/site/title',3,0,NULL,1,7,'')",
                    [],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO novel_reader_leases(prefix,reader_id,epoch,expires_at) VALUES('novels/site/title','reader-1',3,?1)",
                    [crate::platform::transfer_stage::now_secs() + 600],
                )
                .unwrap();
            }

            let snapshot = state
                .with_compat_lock(|| {
                    CutoverSnapshot::capture(&state, &narou_dir, &layout.library_archive)
                })
                .unwrap();
            let staged = export_objects(&state, &layout, &root, Some(&dir.path().join("staging")))
                .await
                .unwrap();
            finalize_in_place(&state, &narou_dir, &layout, &snapshot, &staged, Some(&state))
                .unwrap();

            assert_eq!(
                crate::native::sqlite::state::read_mode(&narou_dir),
                StorageMode::Yaml
            );
            assert_eq!(std::fs::read(layout.target_path(&key).unwrap()).unwrap(), b"toc");
            {
                let conn = state.conn_ref().lock();
                let sql_gate: bool = conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM novel_access_gates WHERE prefix='novels/site/title')",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert!(!sql_gate, "the SQL row is retired");
                let handoff =
                    crate::native::object_store::read_fs_gate_handoff_raw(&conn, "novels/site/title")
                        .unwrap();
                assert_eq!(handoff.map(|(sql, fs, _)| (sql, fs)), Some((3, 4)));
            }
            // The live FS gate carries the reader lease at its original epoch.
            let gate_path = mirror
                .gate_path(&ObjectPrefix::new("novels/site/title".to_string()).unwrap())
                .unwrap();
            let gate: crate::native::object_store::FsGate =
                serde_json::from_slice(&std::fs::read(&gate_path).unwrap()).unwrap();
            assert_eq!(gate.epoch, 4);
            assert_eq!(
                gate.readers.get("reader-1").map(|(epoch, _)| *epoch),
                Some(3)
            );

            // A closed SQL gate blocks adoption: the whole cutover fails and
            // the marker stays on SQLite.
            crate::native::sqlite::state::write_mode(&narou_dir, StorageMode::Sqlite).unwrap();
            {
                let conn = state.conn_ref().lock();
                conn.execute(
                    "INSERT INTO novel_access_gates(prefix,epoch,closed,owner,updated_at,content_revision,origin_url_fold) VALUES('novels/site/closed',2,1,'remove-v1:1:1',1,0,'')",
                    [],
                )
                .unwrap();
            }
            // The earlier object write bumped the captured fingerprint; take
            // a fresh snapshot so only the closed gate fails validation.
            let snapshot = state
                .with_compat_lock(|| {
                    CutoverSnapshot::capture(&state, &narou_dir, &layout.library_archive)
                })
                .unwrap();
            let error = finalize_in_place(
                &state,
                &narou_dir,
                &layout,
                &snapshot,
                &[],
                Some(&state),
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("closed") || error.to_string().contains("FS gate"),
                "{error}"
            );
            assert_eq!(
                crate::native::sqlite::state::read_mode(&narou_dir),
                StorageMode::Sqlite
            );
            {
                let conn = state.conn_ref().lock();
                let rows: i64 = conn
                    .query_row("SELECT COUNT(*) FROM novel_access_gates", [], |row| row.get(0))
                    .unwrap();
                assert_eq!(rows, 1, "the rolled-back cutover leaves SQL authority intact");
            }
        });
    }

    /// Remote-reference markers (`.narou-transfer/.refs/<logical>`) are the
    /// one reserved namespace that must cross the cutover: the FS/YAML
    /// backend reads them verbatim to resolve remote reads. Everything else
    /// under `.narou-transfer/` and `.narou-staging/` stays behind.
    #[test]
    fn remote_reference_markers_cross_the_export_boundary() {
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let layout = temp_layout(true, dir.path());
            let store = memory_store(&layout);
            let canonical = ObjectKey::try_new("novels/site/title/toc.yaml").unwrap();
            ObjectStore::write_small(&store, &canonical, b"toc\n".to_vec())
                .await
                .unwrap();
            // A non-hash S3 alias marker (physical is NOT a content hash —
            // only this row knows where the bytes live) plus a deletion
            // tombstone (physical == "").
            let alias = serde_json::json!({
                "physical": "content/7f9c2ab4-pool",
                "size": 120,
                "sha256": "9f".repeat(32),
                "logical": "novels/site/title/挿絵/ref.png",
                "prefix": "novels/site/title",
                "etag": "etag-abc"
            });
            let alias_key =
                ObjectKey::try_new(".narou-transfer/.refs/novels/site/title/挿絵/ref.png").unwrap();
            ObjectStore::write_small(&store, &alias_key, serde_json::to_vec(&alias).unwrap())
                .await
                .unwrap();
            let tombstone = serde_json::json!({
                "physical": "",
                "size": 0,
                "sha256": "",
                "logical": "novels/site/title/挿絵/gone.png",
                "prefix": "novels/site/title"
            });
            let tomb_key =
                ObjectKey::try_new(".narou-transfer/.refs/novels/site/title/挿絵/gone.png").unwrap();
            ObjectStore::write_small(&store, &tomb_key, serde_json::to_vec(&tombstone).unwrap())
                .await
                .unwrap();
            for reserved in [
                ".narou-transfer/.data/blob-x",
                ".narou-transfer/.remote/session-1",
                ".narou-staging/y",
            ] {
                ObjectStore::write_small(
                    &store,
                    &ObjectKey::try_new(reserved).unwrap(),
                    b"internal".to_vec(),
                )
                .await
                .unwrap();
            }

            let (staged, _, _) =
                write_stored_objects(&store, &layout, &root_path(&dir), None)
                    .await
                    .unwrap();
            assert!(staged.is_empty());
            let objects_root = layout.write_root.root().join(".objects/.narou-transfer/.refs");
            let alias_written: serde_json::Value = serde_json::from_slice(
                &std::fs::read(objects_root.join("novels/site/title/挿絵/ref.png")).unwrap(),
            )
            .unwrap();
            assert_eq!(alias_written["physical"].as_str(), Some("content/7f9c2ab4-pool"));
            assert_eq!(alias_written["etag"].as_str(), Some("etag-abc"));
            let tomb_written: serde_json::Value = serde_json::from_slice(
                &std::fs::read(objects_root.join("novels/site/title/挿絵/gone.png")).unwrap(),
            )
            .unwrap();
            assert_eq!(tomb_written["physical"].as_str(), Some(""));
            assert!(
                !layout.write_root.root().join(".objects/.narou-transfer/.data/blob-x").exists(),
                "sessions/blobs/locks never reach the export"
            );
            assert!(!layout
                .write_root
                .root()
                .join(".objects/.narou-transfer/.remote/session-1")
                .exists());
            assert!(!layout.write_root.root().join(".objects/.narou-staging/y").exists());
        });
    }

    /// Restart recovery for a crashed flip: durable intent file + marker in
    /// each crash position must converge to exactly one authority.
    /// `reconcile_cutover_intent` opens the database itself — the same path a
    /// marker=yaml restart takes via `NativeStore::for_narou_root`.
    #[test]
    fn reconcile_cutover_intent_converges_every_crash_position() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_path(&dir);
        std::fs::create_dir_all(&root).unwrap();
        let narou_dir = root.join(".narou");
        crate::native::sqlite::state::write_mode(&narou_dir, StorageMode::Sqlite).unwrap();
        let state = super::super::state::configure(&narou_dir).unwrap();
        let intent = narou_dir.join(CUTOVER_INTENT_FILE);

        // Case A: marker=yaml written but tx rolled back (no pending row).
        crate::db::inventory::atomic_write(&intent, "in flight\n").unwrap();
        crate::native::sqlite::state::write_mode(&narou_dir, StorageMode::Yaml).unwrap();
        reconcile_cutover_intent(&narou_dir).unwrap();
        assert_eq!(
            crate::native::sqlite::state::read_mode(&narou_dir),
            StorageMode::Sqlite,
            "rolled-back flip restores the SQLite selector"
        );
        assert!(!intent.exists());

        // Case B: marker=sqlite + leftover intent (post-crash debris) —
        // discharges evidence without touching the selector.
        {
            let conn = state.conn_ref().lock();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS cutover_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL) WITHOUT ROWID;\
                 INSERT OR REPLACE INTO cutover_meta(key,value) VALUES('yaml_pending','1');",
            )
            .unwrap();
        }
        crate::db::inventory::atomic_write(&intent, "in flight\n").unwrap();
        reconcile_cutover_intent(&narou_dir).unwrap();
        assert_eq!(
            crate::native::sqlite::state::read_mode(&narou_dir),
            StorageMode::Sqlite
        );
        assert!(!intent.exists());
        {
            let conn = state.conn_ref().lock();
            let pending: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM cutover_meta WHERE key='yaml_pending')",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(!pending, "stale intent row discharged");
        }

        // No intent file: untouched regardless of marker.
        reconcile_cutover_intent(&narou_dir).unwrap();
        assert_eq!(
            crate::native::sqlite::state::read_mode(&narou_dir),
            StorageMode::Sqlite
        );
    }

    /// Committed-flip crash: marker=yaml + committed pending row + a
    /// `.gate.pending` journal -> the reconciler installs the canonical FS
    /// gate at the journaled epoch, retires the journal and discharges
    /// intent. The marker correctly stays yaml (the flip DID commit).
    #[test]
    fn reconcile_cutover_intent_finishes_committed_pending_gates() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_path(&dir);
        std::fs::create_dir_all(&root).unwrap();
        let narou_dir = root.join(".narou");
        crate::native::sqlite::state::write_mode(&narou_dir, StorageMode::Sqlite).unwrap();
        let state = super::super::state::configure(&narou_dir).unwrap();
        let mirror =
            NativeObjectStore::from_root(root.join(crate::downloader::types::ARCHIVE_ROOT_DIR))
                .unwrap();
        let prefix = ObjectPrefix::new("novels/site/title".to_string()).unwrap();
        {
            let conn = state.conn_ref().lock();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS cutover_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL) WITHOUT ROWID;\
                 INSERT OR REPLACE INTO cutover_meta(key,value) VALUES('yaml_pending','1');",
            )
            .unwrap();
            // The handoff tuple the committed adoption recorded (sql 3 -> fs 4).
            crate::native::object_store::record_fs_gate_handoff(&conn, "novels/site/title", 3, 4, "")
                .unwrap();
        }
        let pending_gate = crate::native::object_store::FsGate {
            prefix: Some("novels/site/title".to_string()),
            origin_url_fold: String::new(),
            content_revision: 7,
            epoch: 4,
            owner: None,
            updated_at: 1,
            migrated: false,
            readers: std::collections::BTreeMap::from([(
                "reader-1".to_string(),
                (3u64, crate::platform::transfer_stage::now_secs() + 600),
            )]),
        };
        let pending_path = mirror.gate_pending_path(&prefix).unwrap();
        std::fs::create_dir_all(pending_path.parent().unwrap()).unwrap();
        std::fs::write(&pending_path, serde_json::to_vec(&pending_gate).unwrap()).unwrap();

        let intent = narou_dir.join(CUTOVER_INTENT_FILE);
        crate::db::inventory::atomic_write(&intent, "in flight\n").unwrap();
        crate::native::sqlite::state::write_mode(&narou_dir, StorageMode::Yaml).unwrap();

        reconcile_cutover_intent(&narou_dir).unwrap();

        assert_eq!(
            crate::native::sqlite::state::read_mode(&narou_dir),
            StorageMode::Yaml,
            "committed flip keeps the yaml selector"
        );
        assert!(!intent.exists(), "converged intent discharged");
        assert!(!pending_path.exists(), "journal retired");
        let gate_path = mirror.gate_path(&prefix).unwrap();
        let gate: crate::native::object_store::FsGate =
            serde_json::from_slice(&std::fs::read(&gate_path).unwrap()).unwrap();
        assert_eq!(gate.epoch, 4);
        assert_eq!(gate.content_revision, 7);
        assert_eq!(
            gate.readers.get("reader-1").map(|(epoch, _)| *epoch),
            Some(3),
            "reader lease survives at its original epoch"
        );
    }

    #[test]
    fn export_in_progress_guard_tracks_active_exports() {
        let initial = export_in_progress();
        {
            let _g1 = ExportInProgressGuard::enter();
            assert!(export_in_progress());
            {
                let _g2 = ExportInProgressGuard::enter();
                assert!(export_in_progress());
            }
            assert!(export_in_progress());
        }
        assert_eq!(export_in_progress(), initial);
    }

    #[test]
    fn write_stored_objects_batches_inline_handles_chunked_and_fallback() {
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let layout = temp_layout(true, dir.path());
            let archive = layout.library_archive.clone();
            let store = memory_store(&layout);

            // 1. Inline matching (on disk identically) -> skipped in place
            let k_match = ObjectKey::try_new("novels/site/book/match.txt").unwrap();
            ObjectStore::write_small(&store, &k_match, b"same-bytes".to_vec())
                .await
                .unwrap();
            let match_target = layout.target_path(&k_match).unwrap();
            std::fs::create_dir_all(match_target.parent().unwrap()).unwrap();
            std::fs::write(&match_target, b"same-bytes").unwrap();

            // 2. Inline differing -> staged in place
            let k_diff = ObjectKey::try_new("novels/site/book/diff.txt").unwrap();
            ObjectStore::write_small(&store, &k_diff, b"new-bytes!".to_vec())
                .await
                .unwrap();
            let diff_target = layout.target_path(&k_diff).unwrap();
            std::fs::write(&diff_target, b"old-bytes!").unwrap();

            // 3. Chunked >512 KiB object
            let chunk_bytes = vec![0x5Au8; 600 * 1024];
            let k_chunk = ObjectKey::try_new("novels/site/book/big.bin").unwrap();
            crate::platform::AssetStore::write_stream(
                &store,
                &k_chunk,
                Box::pin(futures::stream::once({
                    let bytes = chunk_bytes.clone();
                    async move { Ok(bytes) }
                })),
            )
            .await
            .unwrap();

            // 4. Filesystem-only fallback under novels/ (skipped in place, copied in bundle)
            let fs_novel_fallback = archive.join("site/book/fallback.txt");
            std::fs::write(&fs_novel_fallback, b"fs-fallback").unwrap();

            // 5. Filesystem-only fallback under .objects/webnovel/ -> webnovel/
            let fs_webnovel_fallback = archive.join(".objects/webnovel/fallback.yaml");
            std::fs::create_dir_all(fs_webnovel_fallback.parent().unwrap()).unwrap();
            std::fs::write(&fs_webnovel_fallback, b"name: fallback\n").unwrap();

            let staging = dir.path().join("staging");
            let (staged, _, _) =
                write_stored_objects(&store, &layout, &root_path(&dir), Some(&staging))
                    .await
                    .unwrap();
            let staged_targets: Vec<PathBuf> = staged.iter().map(|(_, t)| t.clone()).collect();
            assert!(!staged_targets.contains(&match_target), "matching inline skipped");
            assert!(staged_targets.contains(&diff_target), "differing inline staged");
            assert!(
                staged_targets.contains(&layout.target_path(&k_chunk).unwrap()),
                "chunked object staged"
            );
            assert!(
                !staged_targets.contains(&fs_novel_fallback),
                "in-place novel fallback already at target"
            );
            assert!(
                staged_targets.contains(&root_path(&dir).join("webnovel/fallback.yaml")),
                "relocated .objects/webnovel fallback staged into webnovel/"
            );

            // Bundle mode copies everything including fs_novel_fallback
            let bundle_dir = dir.path().join("bundle_out");
            std::fs::create_dir_all(&bundle_dir).unwrap();
            let bundle_layout = ExportLayout::new(&root_path(&dir), bundle_dir.clone(), false).unwrap();
            let (bundle_staged, _, _) =
                write_stored_objects(&store, &bundle_layout, &root_path(&dir), None)
                    .await
                    .unwrap();
            assert!(bundle_staged.is_empty());
            assert_eq!(
                std::fs::read(bundle_dir.join("小説データ/site/book/fallback.txt")).unwrap(),
                b"fs-fallback"
            );
            assert_eq!(
                std::fs::read(bundle_dir.join("小説データ/site/book/big.bin")).unwrap(),
                chunk_bytes
            );
            assert_eq!(
                std::fs::read(bundle_dir.join("webnovel/fallback.yaml")).unwrap(),
                b"name: fallback\n"
            );

            // CRC-32 corruption in an existing inline target propagates as an error.
            {
                let conn = store.conn_handle();
                conn.lock()
                    .execute(
                        "UPDATE objects SET crc32 = 0 WHERE object_key = ?",
                        rusqlite::params![k_match.as_ref()],
                    )
                    .unwrap();
            }
            let err = store
                .check_inline_targets_batch(&[(k_match, 10, match_target)])
                .unwrap_err();
            assert!(err.to_string().contains("match.txt"), "{err}");
        });
    }

    #[test]
    fn fingerprint_tree_prunes_transfer_internals_and_handles_noncanonical_base() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("lib").join("小説データ");
        std::fs::create_dir_all(archive.join("site/book")).unwrap();
        std::fs::create_dir_all(archive.join(".objects/.narou-transfer/.lock")).unwrap();
        std::fs::create_dir_all(archive.join(".objects/.narou-transfer/.refs/site")).unwrap();
        std::fs::create_dir_all(archive.join(".objects/.narou-staging")).unwrap();

        std::fs::write(archive.join("site/book/1.txt"), b"body").unwrap();
        std::fs::write(archive.join(".objects/.narou-transfer/.lock/l1"), b"lock").unwrap();
        std::fs::write(archive.join(".objects/.narou-transfer/.refs/site/r1"), b"ref").unwrap();
        std::fs::write(archive.join(".objects/.narou-staging/tmp"), b"stage").unwrap();

        let noncanonical = dir.path().join("lib").join("小説データ").join("..").join("小説データ");
        let mut stamps = BTreeMap::new();
        fingerprint_tree(&noncanonical, "小説データ/", &mut stamps).unwrap();
        assert!(stamps.contains_key("小説データ/site/book/1.txt"));
        assert!(stamps.contains_key("小説データ/.objects/.narou-transfer/.refs/site/r1"));
        assert!(!stamps.contains_key("小説データ/.objects/.narou-transfer/.lock/l1"));
        assert!(!stamps.contains_key("小説データ/.objects/.narou-staging/tmp"));
    }
}

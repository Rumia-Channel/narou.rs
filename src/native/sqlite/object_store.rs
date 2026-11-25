//! SQLite-backed ObjectStore/AssetStore for `storage-backend = sqlite`.
//!
//! The `objects`/`object_chunks` tables are authoritative for reads and
//! listings; every mutation is mirrored to the native filesystem layout so a
//! rollback to YAML mode (or narou.rb) sees the same bytes without an export
//! step. `sqlite.mirror-files=false` skips the mirror writes and lets the
//! database carry the library alone.
//!
//! Fallback policy — `list`, `stat`, `read_small` and `read_stream` follow one
//! rule in both mirror modes: a stored row always wins, and only a key without
//! a row is served from the filesystem copy. The filesystem side is consulted
//! because legacy files exist before the one-shot import and because some
//! compatibility writers (e.g. the converter's legacy illustration directory
//! handling) still bypass the store.
//!
//! `delete` therefore owns *both* copies and fails instead of reporting
//! success when the file cannot be removed: a deleted object must not stay
//! readable through the fallback (which would also resurrect it in YAML mode
//! after a rollback).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::StreamExt;
use parking_lot::Mutex;
use rusqlite::Connection;

use crate::error::{NarouError, Result};
use crate::platform::{
    AssetStore, AssetStream, ObjectEncoding, ObjectKey, ObjectListPage, ObjectListRequest,
    ObjectMetadata, ObjectPrefix, ObjectStore, PlatformFuture, prefix_upper_bound,
    ObjectUpdateResult, SmallObjectMutator,
};

use crate::native::object_store::{
    FsGate, NativeObjectStore, content_type_for_path, logical_key_for_native_path, run_blocking, run_scoped_blocking,
};

#[path = "asset_stream.rs"]
mod asset_stream;

/// Payloads at or below this size are stored inline in `objects.data`;
/// larger payloads are chunked into `object_chunks` rows of this size.
/// 512 KiB keeps every statement and row comfortably under D1's limits.
pub(crate) const OBJECT_CHUNK_BYTES: usize = 512 * 1024;


/// ObjectStore + AssetStore backed by `.narou/db.sqlite` with a filesystem
/// mirror under the archive root.
#[derive(Debug, Clone)]
pub struct SqliteObjectStore {
    conn: Arc<Mutex<Connection>>,
    mirror: NativeObjectStore,
    /// 実ファイルのミラーを書くか (`sqlite.mirror-files=false` で無効)。
    /// 無効時は読み書きが SQLite だけで完結する。
    mirror_files: bool,
    stage_leases: Arc<crate::platform::staging::StageLeaseCache>,
}

impl SqliteObjectStore {
    pub fn path_for_canonical_key(&self,key:&ObjectKey)->Result<PathBuf>{self.mirror.resolve_path(key)}
    pub(crate) fn with_gate_lock<T>(&self,prefix:&ObjectPrefix,operation:impl FnOnce()->Result<T>)->Result<T>{self.mirror.with_transfer_lock(prefix.as_ref().trim_end_matches('/'),operation)}
    pub(crate) fn namespace_state_locked(&self,prefix:&ObjectPrefix)->Result<(GateInfo,String)>{let connection=self.conn.lock();let info=Self::gate_info_conn(&connection,prefix)?;let origin=connection.query_row("SELECT origin_url_fold FROM novel_access_gates WHERE prefix=?",[prefix.as_ref().trim_end_matches('/')],|row|row.get(0)).map_err(super::sqlite_error)?;Ok((info,origin))}
    pub fn new(conn: Arc<Mutex<Connection>>, mirror: NativeObjectStore) -> Self {
        Self::with_mirror_files(conn, mirror, crate::native::sqlite::state::mirror_files_enabled())
    }

    /// 実ファイルのミラーが置かれるルート (`小説データ/`)。
    pub(crate) fn archive_root(&self) -> &Path {
        self.mirror.root()
    }

    /// 生の接続ハンドル (テスト用)。
    #[cfg(test)]
    pub(crate) fn conn_handle(&self) -> &Arc<Mutex<Connection>> {
        &self.conn
    }

    /// ミラー有無を明示して組み立てる (テスト用)。
    pub fn with_mirror_files(
        conn: Arc<Mutex<Connection>>,
        mirror: NativeObjectStore,
        mirror_files: bool,
    ) -> Self {
        Self {
            conn,
            mirror,
            mirror_files,
            stage_leases: Arc::new(crate::platform::staging::StageLeaseCache::default()),
        }
    }

    fn has_promoted_reference(conn:&Connection,key:&ObjectKey)->Result<bool>{conn.query_row("SELECT EXISTS(SELECT 1 FROM transfer_object_refs WHERE object_key=?)",[key.as_ref()],|row|row.get(0)).map_err(super::sqlite_error)}
    pub fn open_leased_asset<'a>(&'a self,prefix:&'a ObjectPrefix,lease:&'a GateLease,key:&'a ObjectKey,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<(ObjectMetadata,AssetStream)>>>{Box::pin(async move{
        if !prefix.matches(key){return Err(NarouError::Platform("reader key outside novel".into()));}
        run_scoped_blocking(||self.with_reader_transaction(prefix,lease,|conn|self.open_asset_in_connection(conn,key,range)))
    })}

    /// Lease-validated open that never resolves a canonical reference: the ref
    /// check, reader admission and payload pin share one lock boundary.
    pub fn open_local_leased_asset<'a>(&'a self,prefix:&'a ObjectPrefix,lease:&'a GateLease,key:&'a ObjectKey,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<(ObjectMetadata,AssetStream)>>>{Box::pin(async move{
        if !prefix.matches(key){return Err(NarouError::Platform("reader key outside novel".into()));}
        run_scoped_blocking(||self.with_reader_transaction(prefix,lease,|conn|self.open_local_asset_in_connection(conn,key,range)))
    })}
    /// Local-only open under the same OS transfer lock used by reference
    /// publication: `Ok(None)` for missing keys AND canonical references.
    pub fn local_asset_stream<'a>(&'a self,prefix:&'a ObjectPrefix,key:&'a ObjectKey,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<AssetStream>>>{Box::pin(async move{
        let this=self.clone();let key=key.clone();run_scoped_blocking(move||this.mirror.with_reader_gate_lock(prefix.as_ref().trim_end_matches('/'),||{let conn=this.conn.lock();Ok(this.open_local_asset_in_connection(&conn,&key,range)?.map(|(_,stream)|stream))}))
    })}
    /// Whether `key` is backed by a canonical reference. Cheap existence probe;
    /// an existence answer is correct even without the caller's lock.
    pub fn has_remote_reference(&self,key:&ObjectKey)->Result<bool>{Self::has_promoted_reference(&self.conn.lock(),key)}

    fn now_rfc3339() -> String {
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
    }

    /// Rewrite the mirror file for `path` from the stored object.
    ///
    /// The filesystem mirror can disappear outside narou (manual cleanup, a
    /// sync tool). The stored copy stays authoritative, so rebuild the file
    /// instead of failing the caller that needs to read it. Returns `false`
    /// when the storage does not know the key.
    pub fn restore_mirror_file(&self, path: &Path) -> Result<bool> {
        // Callers reach this with either an absolute path or one relative to the
        // library root (the download-triggered conversion passes a relative
        // novel directory), so resolve against the working directory first.
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        };
        let Some(key) = logical_key_for_native_path(self.mirror.root(), &absolute) else {
            return Ok(false);
        };
        let Some(data) = self.read_blocking(&key, None)? else {
            return Ok(false);
        };
        self.mirror.write_bytes_blocking(&key, &data)?;
        Ok(path.is_file())
    }

    fn stat_blocking(&self, key: &ObjectKey) -> Result<Option<ObjectMetadata>> {
        let conn = self.conn.lock();
        let physical=resolve_payload_key(&conn,key)?;
        let mut statement = conn
            .prepare("SELECT size, content_type, updated_at FROM objects WHERE object_key = ?")
            .map_err(super::sqlite_error)?;
        let mut rows = statement
            .query(rusqlite::params![physical.as_ref()])
            .map_err(super::sqlite_error)?;
        match rows.next().map_err(super::sqlite_error)? {
            Some(row) => {
                let size: i64 = row.get(0).map_err(super::sqlite_error)?;
                let content_type: Option<String> = row.get(1).map_err(super::sqlite_error)?;
                let updated_at: String = row.get(2).map_err(super::sqlite_error)?;
                Ok(Some(ObjectMetadata {
                    key: key.clone(),
                    size: size as u64,
                    etag: None,
                    content_type:content_type.or_else(||content_type_for_path(Path::new(key.as_ref())).map(str::to_owned)),
                    last_modified: chrono::DateTime::parse_from_rfc3339(&updated_at)
                        .ok()
                        .map(|value| value.with_timezone(&chrono::Utc)),
                }))
            }
            None => Ok(None),
        }
    }

    /// Read the full payload from SQLite. Returns `Ok(None)` when the key is
    /// absent; errors only on real storage failures. The stored CRC-32 is
    /// verified against the decompressed payload.
    ///
    /// Only the row/chunk reads run under the connection lock: decompression
    /// and CRC verification of (potentially large) payloads happen after it is
    /// released, so a read never blocks writers on CPU work.
    fn read_blocking(&self, key: &ObjectKey, limit: Option<u64>) -> Result<Option<Vec<u8>>> {
        let payload = {
            let conn = self.conn.lock();
            read_stored_payload(&conn, key, limit)?
        };
        payload.map(|payload| decode_stored_payload(payload, key, limit)).transpose()
    }

    fn update_small_blocking<'data>(
        &self,
        key: &ObjectKey,
        mutate: &mut SmallObjectMutator<'data>,
    ) -> Result<ObjectUpdateResult> {
        let path = self.mirror.resolve_path(key)?;
        let mut operation = || {
            let mut conn = self.conn.lock();
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(super::sqlite_error)?;
            let limit = crate::native::object_store::MAX_SMALL_OBJECT_BYTES;
            let current = match read_stored_payload(&tx, key, Some(limit))? {
                Some(payload) => Some(decode_stored_payload(payload, key, Some(limit))?),
                None => self.mirror.read_small_blocking(key)?,
            };
            let Some(data) = mutate(current.as_deref())? else {
                tx.commit().map_err(super::sqlite_error)?;
                return Ok(ObjectUpdateResult::Committed);
            };
            if data.len() as u64 > limit {
                return Err(NarouError::Platform(format!("small object exceeds {limit} bytes: {key}")));
            }
            let (stored, encoding) = crate::platform::encode_object_payload(&data);
            let updated_at = Self::now_rfc3339();
            insert_stored_object(&tx, StoredObject {
                key,
                size: data.len() as i64,
                crc32: crate::platform::object_crc32(&data) as i64,
                stored: &stored,
                encoding,
                content_type: content_type_for_path(Path::new(key.as_ref())),
                updated_at: &updated_at,
            })?;
            tx.commit().map_err(super::sqlite_error)?;
            if self.mirror_files
                && let Err(error) = self.mirror.write_bytes_blocking(key, &data)
            {
                return Ok(ObjectUpdateResult::ProjectionFailed(error));
            }
            Ok(ObjectUpdateResult::Committed)
        };
        if self.mirror_files || path.exists() {
            self.mirror.ensure_parent_dir(&path)?;
            crate::db::inventory::with_exclusive_file_lock(&path, operation)
        } else {
            operation()
        }
    }

    /// Insert or replace one object (inline or chunked) in a single
    /// transaction. Compression runs on the whole payload before chunking so
    /// chunk boundaries never split a brotli stream; `size`/`crc32` describe
    /// the uncompressed payload.
    fn write_blocking(&self, key: &ObjectKey, data: &[u8]) -> Result<()> {
        let (stored, encoding) = crate::platform::compress_object_payload(data);
        let crc32 = crate::platform::object_crc32(data) as i64;
        let mut conn = self.conn.lock();
        let tx = conn.transaction().map_err(super::sqlite_error)?;
        let updated_at = Self::now_rfc3339();
        insert_stored_object(
            &tx,
            StoredObject {
                key,
                size: data.len() as i64,
                crc32,
                stored: &stored,
                encoding,
                content_type: content_type_for_path(Path::new(key.as_ref())),
                updated_at: &updated_at,
            },
        )?;
        tx.commit().map_err(super::sqlite_error)
    }

    /// Bulk prefix delete.
    ///
    /// The filesystem copies go first, without the connection lock: when an
    /// unlink fails the stored rows survive, so the object stays consistent in
    /// both copies (same guarantee as [`Self::delete_both`]). The stored rows
    /// then go in one transaction with a range predicate on the primary key —
    /// one statement instead of a `DELETE` per listed key.
    fn delete_prefix_blocking(&self, prefix: &ObjectKey) -> Result<()> {
        let base = prefix.as_ref().trim_end_matches('/').to_string();

        let prefix_dir = self.mirror.resolve_prefix_dir(&base)?;
        if prefix_dir.is_dir() {
            let mut paths = Vec::new();
            NativeObjectStore::recursive_files(self.mirror.root(), &prefix_dir, &mut paths)?;
            for path in paths {
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        // `ObjectPrefix::matches` also accepts a key *equal* to the prefix; on
        // disk that key is a file at the prefix path, not a child of it.
        // An empty prefix (whole-store delete) is not a valid key, so this
        // only runs for real prefix keys.
        if let Ok(exact_key) = ObjectKey::try_new(base.clone()) {
            let exact = self.mirror.resolve_path(&exact_key)?;
            if exact.is_file() {
                std::fs::remove_file(&exact)?;
            }
        }

        let mut conn = self.conn.lock();
        let tx = conn.transaction().map_err(super::sqlite_error)?;
        if base.is_empty() {
            tx.execute("DELETE FROM transfer_object_refs",[]).map_err(super::sqlite_error)?;
            tx.execute("DELETE FROM object_chunks", [])
                .map_err(super::sqlite_error)?;
            tx.execute("DELETE FROM objects", [])
                .map_err(super::sqlite_error)?;
        } else {
            // `object_key = base` or `<base>/...`: exactly what
            // `ObjectPrefix::matches` accepts, expressed as a primary-key
            // range so the delete never scans the table.
            let lower = format!("{base}/");
            let upper = prefix_upper_bound(&lower);
            for table in ["transfer_object_refs", "object_chunks", "objects"] {
                tx.execute(
                    &format!(
                        "DELETE FROM {table} WHERE object_key = ?1 OR (object_key >= ?2 AND object_key < ?3)"
                    ),
                    rusqlite::params![base, lower, upper],
                )
                .map_err(super::sqlite_error)?;
            }
        }
        tx.commit().map_err(super::sqlite_error)
    }

    fn delete_blocking(&self, key: &ObjectKey) -> Result<()> {
        let mut conn=self.conn.lock();let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(super::sqlite_error)?;
        let pinned:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM transfer_object_refs WHERE blob_key=?)",[key.as_ref()],|row|row.get(0)).map_err(super::sqlite_error)?;
        if pinned{return Err(NarouError::Platform("cannot delete referenced physical object".into()));}
        tx.execute("DELETE FROM transfer_object_refs WHERE object_key=?",[key.as_ref()]).map_err(super::sqlite_error)?;
        tx.execute("DELETE FROM objects WHERE object_key=?",[key.as_ref()]).map_err(super::sqlite_error)?;tx.commit().map_err(super::sqlite_error)
    }

    /// Remove the filesystem copy of `key`; `Ok(())` when it is already gone.
    fn remove_file_copy(&self, key: &ObjectKey) -> Result<()> {
        let path = self.mirror.resolve_path(key)?;
        match std::fs::remove_file(path) {
            Ok(()) => {
                let _ = self.mirror.forget_file_proof(key);
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let _ = self.mirror.forget_file_proof(key);
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Delete a stored object together with its filesystem copy.
    ///
    /// The file is removed first: when that fails (locked file, read-only
    /// medium, a directory occupying the path) the stored row survives, so the
    /// object stays consistent in both copies instead of leaving a
    /// read-visible file that the fallback would resurrect. Without the mirror
    /// a file can still exist — written while the mirror was enabled, or by a
    /// path that bypasses the store — and deleting only the row would keep it
    /// readable through `stat`/`read_small`/`read_stream` and hand it back to
    /// narou.rb after a YAML rollback.
    fn delete_both(&self, key: &ObjectKey) -> Result<()> {
        self.remove_file_copy(key)?;
        self.delete_blocking(key)?;
        self.stage_leases.forget(key);
        Ok(())
    }



    pub(crate) fn list_direct_files(&self, prefix: &ObjectKey) -> Result<Vec<ObjectKey>> {
        let start = format!("{prefix}/");
        let end = prefix_upper_bound(&start);
        let mut keys = Vec::new();
        {
            let conn = self.conn.lock();
            let mut statement = conn.prepare(
                "SELECT object_key FROM native_logical_objects
                 WHERE object_key >= ?1 AND object_key < ?2
                   AND instr(substr(object_key, length(?1) + 1), '/') = 0
                 ORDER BY object_key",
            ).map_err(super::sqlite_error)?;
            let rows = statement.query_map([&start, &end], |row| row.get::<_, String>(0))
                .map_err(super::sqlite_error)?;
            for row in rows {
                keys.push(ObjectKey::try_new(row.map_err(super::sqlite_error)?)?);
            }
        }
        keys.extend(self.mirror.list_direct_files(prefix)?);
        keys.sort_unstable_by(|a, b| a.as_ref().cmp(b.as_ref()));
        keys.dedup();
        Ok(keys)
    }

    fn list_blocking(&self, request: &ObjectListRequest) -> Result<ObjectListPage> {
        let mut merged: BTreeMap<String, ObjectMetadata> = BTreeMap::new();
        // Fetch only this SQL page plus one lookahead row. Merging the first
        // limit + 1 database keys with filesystem fallbacks preserves DB wins.
        let base = request.prefix.as_ref().trim_end_matches('/');
        let lower = if base.is_empty() { String::new() } else { format!("{base}/") };
        let upper=if base.is_empty(){prefix_upper_bound(&lower)}else{format!("{base}0")};
        let limit=request.limit.get().min(100);let cap=(limit+1) as i64;
        {
            let conn = self.conn.lock();
            let mut statement = conn
                .prepare(NATIVE_LIST_PAGE_SQL)
                .map_err(super::sqlite_error)?;
            let mut rows = statement
                .query(rusqlite::params![base, lower, upper, request.cursor.as_deref().unwrap_or(""), cap])
                .map_err(super::sqlite_error)?;
            while let Some(row) = rows.next().map_err(super::sqlite_error)? {
                let key_text: String = row.get(0).map_err(super::sqlite_error)?;
                let Ok(key) = ObjectKey::try_new(key_text.clone()) else {
                    continue;
                };
                if !request.prefix.matches(&key) {
                    continue;
                }
                let size: i64 = row.get(1).map_err(super::sqlite_error)?;
                let content_type: Option<String> = row.get(2).map_err(super::sqlite_error)?;
                let updated_at: String = row.get(3).map_err(super::sqlite_error)?;
                merged.insert(
                    key_text,
                    ObjectMetadata {
                        key,
                        size: size as u64,
                        etag: None,
                        content_type,
                        last_modified: chrono::DateTime::parse_from_rfc3339(&updated_at)
                            .ok()
                            .map(|value| value.with_timezone(&chrono::Utc)),
                    },
                );
            }
        }
        // Filesystem union for keys the database does not know, in *both*
        // mirror modes: that is the same fallback `stat`/`read_small`/
        // `read_stream` use, so a listed key and a readable key always agree.
        // Rows win, so a stale file never overrides stored content.
        let files=self.mirror.list_window_blocking(request,limit+1)?;
        for metadata in files.objects{merged.entry(metadata.key.to_string()).or_insert(metadata);}
        let objects: Vec<ObjectMetadata> = merged.into_values().collect();
        let (page, next_cursor) = crate::platform::paginate_object_listing(
            objects,
            request.cursor.as_deref(),
            limit,
            |item| item.key.as_ref(),
        );
        Ok(ObjectListPage {
            objects: page,
            next_cursor,
        })
    }

    pub(crate) fn list_sqlite_objects_page(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<SqliteObjectPage> {
        let limit = limit.max(1);
        let cap = (limit + 1) as i64;
        let cursor_str = cursor.unwrap_or("");
        let mut raw: Vec<(String, i64)> = {
            let conn = self.conn.lock();
            let mut stmt = conn
                .prepare_cached(
                    "SELECT object_key, size FROM (
                         SELECT o.object_key AS object_key, o.size AS size
                         FROM objects o
                         WHERE o.object_key > ?1
                           AND NOT EXISTS (SELECT 1 FROM transfer_object_refs r WHERE r.object_key = o.object_key)
                         ORDER BY o.object_key LIMIT ?2
                     )
                     UNION ALL
                     SELECT object_key, size FROM (
                         SELECT r.object_key AS object_key, r.size AS size
                         FROM transfer_object_refs r
                         JOIN objects o ON o.object_key = r.blob_key
                         WHERE r.backend = 'd1' AND r.object_key > ?1
                         ORDER BY r.object_key LIMIT ?2
                     )
                     ORDER BY object_key LIMIT ?2",
                )
                .map_err(super::sqlite_error)?;
            let mut rows = stmt
                .query(rusqlite::params![cursor_str, cap])
                .map_err(super::sqlite_error)?;
            let mut out = Vec::with_capacity(limit + 1);
            while let Some(row) = rows.next().map_err(super::sqlite_error)? {
                out.push((
                    row.get::<_, String>(0).map_err(super::sqlite_error)?,
                    row.get::<_, i64>(1).map_err(super::sqlite_error)?,
                ));
            }
            out
        };
        let has_more = raw.len() > limit;
        if has_more {
            raw.truncate(limit);
        }
        let next_cursor = has_more.then(|| raw.last().map(|(key, _)| key.clone())).flatten();
        let mut objects = Vec::with_capacity(raw.len());
        for (key_text, size) in raw {
            let Ok(key) = ObjectKey::try_new(key_text) else {
                continue;
            };
            let Ok(size) = u64::try_from(size) else {
                continue;
            };
            objects.push((key, size));
        }
        Ok(SqliteObjectPage { objects, next_cursor })
    }

    pub(crate) fn check_inline_targets_batch(
        &self,
        items: &[(ObjectKey, u64, PathBuf)],
    ) -> Result<Vec<Option<bool>>> {
        const BATCH_ROWS: usize = 64;
        const BATCH_BYTES: usize = 16 * 1024 * 1024;
        let mut results = vec![None; items.len()];
        let mut candidates = Vec::new();
        for (idx, (key, expected_size, target)) in items.iter().enumerate() {
            let Ok(meta) = std::fs::metadata(target) else {
                results[idx] = Some(false);
                continue;
            };
            if !meta.is_file() || meta.len() != *expected_size {
                results[idx] = Some(false);
                continue;
            }
            if crate::native::object_store::legacy_flat_key(key)?.is_some() {
                continue;
            }
            candidates.push(idx);
        }
        let mut pos = 0usize;
        while pos < candidates.len() {
            let mut fetched = Vec::with_capacity(BATCH_ROWS.min(candidates.len() - pos));
            {
                let conn = self.conn.lock();
                let mut stmt = conn
                    .prepare_cached(
                        "SELECT o.data, o.size, o.encoding, o.crc32
                         FROM objects o
                         WHERE o.object_key = ?1
                           AND NOT EXISTS (SELECT 1 FROM transfer_object_refs r WHERE r.object_key = o.object_key)",
                    )
                    .map_err(super::sqlite_error)?;
                let mut batch_bytes = 0usize;
                while pos < candidates.len() && fetched.len() < BATCH_ROWS {
                    let idx = candidates[pos];
                    pos += 1;
                    let key = &items[idx].0;
                    let mut rows = stmt.query([key.as_ref()]).map_err(super::sqlite_error)?;
                    let Some(row) = rows.next().map_err(super::sqlite_error)? else {
                        continue;
                    };
                    let rusqlite::types::ValueRef::Blob(bytes) =
                        row.get_ref(0).map_err(super::sqlite_error)?
                    else {
                        continue;
                    };
                    let size: i64 = row.get(1).map_err(super::sqlite_error)?;
                    let Ok(size) = u64::try_from(size) else {
                        return Err(NarouError::Database(format!("{key}: invalid object size")));
                    };
                    let encoding: String = row.get(2).map_err(super::sqlite_error)?;
                    let encoding = ObjectEncoding::parse(&encoding)
                        .map_err(|error| NarouError::Database(format!("{key}: {error}")))?;
                    let crc32: i64 = row.get(3).map_err(super::sqlite_error)?;
                    batch_bytes = batch_bytes.saturating_add(bytes.len());
                    fetched.push((
                        idx,
                        StoredPayload {
                            data: bytes.to_vec(),
                            encoding,
                            crc32: crc32 as u32,
                            size,
                        },
                    ));
                    if batch_bytes >= BATCH_BYTES {
                        break;
                    }
                }
            }
            for (idx, payload) in fetched {
                let (key, _, target) = &items[idx];
                let decoded = decode_stored_payload(payload, key, None)
                    .map_err(|error| NarouError::Database(format!("{key}: {error}")))?;
                let matches = match std::fs::read(target) {
                    Ok(disk) => disk == decoded,
                    Err(_) => false,
                };
                results[idx] = Some(matches);
            }
        }
        Ok(results)
    }
}

pub(crate) struct SqliteObjectPage {
    pub objects: Vec<(ObjectKey, u64)>,
    pub next_cursor: Option<String>,
}

struct StoredPayload {
    data: Vec<u8>,
    encoding: ObjectEncoding,
    crc32: u32,
    size: u64,
}

fn resolve_payload_key(conn:&Connection,key:&ObjectKey)->Result<ObjectKey>{
    let alias=crate::native::object_store::legacy_flat_key(key)?;
    let logical=if let Some(alias)=alias{let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM native_logical_objects WHERE object_key=?)",[key.as_ref()],|row|row.get(0)).map_err(super::sqlite_error)?;if exists{key.clone()}else{alias}}else{key.clone()};let key=&logical;
    // Existing narou.rb layouts sometimes changed only the subtitle part of a
    // section filename. Keep that fallback native-only; normal core paths
    // always use the canonical ObjectKey generated from the TOC. Mirrors
    // `NativeObjectStore::resolve_read_path`: `.../本文/<index>.yaml` and
    // `.../本文/<index> <subtitle>.yaml` resolve to the first stored section
    // that carries the same index.
    let logical=resolve_legacy_section_key(conn,key)?;let key=&logical;
    let reference:Option<(String,String)>=conn.query_row("SELECT blob_key,blob_generation FROM transfer_object_refs WHERE object_key=? AND backend='d1'",[key.as_ref()],|row|Ok((row.get(0)?,row.get(1)?))).optional().map_err(super::sqlite_error)?;
    let Some((physical,generation))=reference else{return Ok(key.clone());};
    let current:Option<String>=conn.query_row("SELECT generation FROM objects WHERE object_key=?",[&physical],|row|row.get(0)).optional().map_err(super::sqlite_error)?;
    if current.as_deref()!=Some(generation.as_str()){return Err(NarouError::Platform("canonical transfer object generation changed".into()));}ObjectKey::try_new(physical)
}

/// `resolve_payload_key` の `<index>` フォールバック。`native_logical_objects`
/// に正確なキーが無いときだけ、同じ `本文/` 直下の `<index>.yaml` /
/// `<index> *.yaml` を B-tree 範囲条件で引く。
fn resolve_legacy_section_key(conn:&Connection,key:&ObjectKey)->Result<ObjectKey>{
    let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM native_logical_objects WHERE object_key=?)",[key.as_ref()],|row|row.get(0)).map_err(super::sqlite_error)?;
    if exists{return Ok(key.clone());}
    let Some((dir,file_name))=key.as_ref().rsplit_once('/')else{return Ok(key.clone());};
    if dir.rsplit('/').next()!=Some("本文"){return Ok(key.clone());}
    let Some(stem)=file_name.strip_suffix(".yaml")else{return Ok(key.clone());};
    let index=stem.split_once(' ').map(|(index,_)|index).unwrap_or(stem);
    if index.is_empty(){return Ok(key.clone());}
    let exact=format!("{dir}/{index}.yaml");
    let lower=format!("{dir}/{index} ");
    let upper=format!("{dir}/{index}!");
    let resolved:Option<String>=conn.query_row(
        "SELECT object_key FROM native_logical_objects
         WHERE object_key = ?1
            OR (object_key >= ?2 AND object_key < ?3
                AND instr(substr(object_key, length(?2) + 1), '/') = 0
                AND substr(object_key, -5) = '.yaml')
         ORDER BY object_key LIMIT 1",
        rusqlite::params![exact,lower,upper],
        |row|row.get(0),
    ).optional().map_err(super::sqlite_error)?;
    match resolved{Some(resolved)=>ObjectKey::try_new(resolved),None=>Ok(key.clone())}
}

fn read_stored_payload(
    conn: &Connection,
    key: &ObjectKey,
    limit: Option<u64>,
) -> Result<Option<StoredPayload>> {
    let resolved=resolve_payload_key(conn,key)?;let key=&resolved;
    let mut statement = conn.prepare(
        "SELECT data, size, encoding, crc32 FROM objects WHERE object_key = ?",
    ).map_err(super::sqlite_error)?;
    let mut rows = statement.query([key.as_ref()]).map_err(super::sqlite_error)?;
    let Some(row) = rows.next().map_err(super::sqlite_error)? else { return Ok(None) };
    let size: i64 = row.get(1).map_err(super::sqlite_error)?;
    if size < 0 || limit.is_some_and(|limit| size as u64 > limit) {
        return Err(NarouError::Platform(format!("small object exceeds the read cap: {key}")));
    }
    let encoding: String = row.get(2).map_err(super::sqlite_error)?;
    let encoding = ObjectEncoding::parse(&encoding)?;
    let crc32: i64 = row.get(3).map_err(super::sqlite_error)?;
    let data = if let rusqlite::types::ValueRef::Blob(bytes) = row.get_ref(0).map_err(super::sqlite_error)? {
        if limit.is_some_and(|limit| bytes.len() as u64 > limit) {
            return Err(NarouError::Platform(format!("stored object exceeds the read cap: {key}")));
        }
        bytes.to_vec()
    } else {
        drop(rows);
        drop(statement);
        let mut statement = conn.prepare(
            "SELECT data FROM object_chunks WHERE object_key = ? ORDER BY seq",
        ).map_err(super::sqlite_error)?;
        let mut rows = statement.query([key.as_ref()]).map_err(super::sqlite_error)?;
        let mut data = Vec::new();
        while let Some(row) = rows.next().map_err(super::sqlite_error)? {
            let chunk = row.get_ref(0).map_err(super::sqlite_error)?.as_blob()
                .map_err(|error| NarouError::Platform(error.to_string()))?;
            if limit.is_some_and(|limit| data.len() as u64 + chunk.len() as u64 > limit) {
                return Err(NarouError::Platform(format!("stored object exceeds the read cap: {key}")));
            }
            data.extend_from_slice(chunk);
        }
        data
    };
    Ok(Some(StoredPayload { data, encoding, crc32: crc32 as u32, size: size as u64 }))
}

fn decode_stored_payload(payload: StoredPayload, key: &ObjectKey, limit: Option<u64>) -> Result<Vec<u8>> {
    let StoredPayload { data: stored, encoding, crc32, size } = payload;
    let data = if encoding == ObjectEncoding::None {
        stored
    } else if let Some(limit) = limit {
        crate::platform::decode_object_payload_limited(&stored, encoding, limit as usize)?.into_owned()
    } else {
        crate::platform::decompress_object_payload(&stored, encoding)?
    };
    crate::platform::verify_object_crc32(&data, crc32)?;
    if data.len() as u64 != size {
        return Err(NarouError::Platform(format!("object payload size mismatch: {key}")));
    }
    Ok(data)
}

/// One object ready to be written: metadata plus the stored (compressed)
/// bytes, so the insert path stays under clippy's argument limit.
struct StoredObject<'a> {
    key: &'a ObjectKey,
    /// Uncompressed payload length.
    size: i64,
    crc32: i64,
    stored: &'a [u8],
    encoding: ObjectEncoding,
    content_type: Option<&'a str>,
    updated_at: &'a str,
}

/// Insert or replace one stored object inside the caller's transaction.
///
/// Payloads whose stored size fits in [`OBJECT_CHUNK_BYTES`] live inline in
/// `objects.data`; larger ones move to `object_chunks` (replacing the previous
/// chunks either way). Shared by single writes and the bounded import batches;
/// the statements are cached on the connection, so a batch pays each prepare
/// once.
fn insert_stored_object(tx: &Connection, object: StoredObject<'_>) -> Result<()> {
    let StoredObject {
        key,
        size,
        crc32,
        stored,
        encoding,
        content_type,
        updated_at,
    } = object;
    if stored.len() <= OBJECT_CHUNK_BYTES {
        tx.prepare_cached(
            "INSERT INTO objects (object_key, size, content_type, updated_at, encoding, crc32, data)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(object_key) DO UPDATE SET
                 size = excluded.size,
                 content_type = excluded.content_type,
                 updated_at = excluded.updated_at,
                 encoding = excluded.encoding,
                 crc32 = excluded.crc32,
                 data = excluded.data",
        )
        .map_err(super::sqlite_error)?
        .execute(rusqlite::params![
            key.as_ref(),
            size,
            content_type,
            updated_at,
            encoding.as_str(),
            crc32,
            stored
        ])
        .map_err(super::sqlite_error)?;
        tx.prepare_cached("DELETE FROM object_chunks WHERE object_key = ?")
            .map_err(super::sqlite_error)?
            .execute(rusqlite::params![key.as_ref()])
            .map_err(super::sqlite_error)?;
    } else {
        tx.prepare_cached(
            "INSERT INTO objects (object_key, size, content_type, updated_at, encoding, crc32, data)
             VALUES (?, ?, ?, ?, ?, ?, NULL)
             ON CONFLICT(object_key) DO UPDATE SET
                 size = excluded.size,
                 content_type = excluded.content_type,
                 updated_at = excluded.updated_at,
                 encoding = excluded.encoding,
                 crc32 = excluded.crc32,
                 data = NULL",
        )
        .map_err(super::sqlite_error)?
        .execute(rusqlite::params![
            key.as_ref(),
            size,
            content_type,
            updated_at,
            encoding.as_str(),
            crc32
        ])
        .map_err(super::sqlite_error)?;
        tx.prepare_cached("DELETE FROM object_chunks WHERE object_key = ?")
            .map_err(super::sqlite_error)?
            .execute(rusqlite::params![key.as_ref()])
            .map_err(super::sqlite_error)?;
        let mut insert = tx
            .prepare_cached("INSERT INTO object_chunks (object_key, seq, data) VALUES (?, ?, ?)")
            .map_err(super::sqlite_error)?;
        for (seq, chunk) in stored.chunks(OBJECT_CHUNK_BYTES).enumerate() {
            insert
                .execute(rusqlite::params![key.as_ref(), seq as i64, chunk])
                .map_err(super::sqlite_error)?;
        }
    }
    Ok(())
}

impl ObjectStore for SqliteObjectStore {
    fn delete_closed<'a>(&'a self,prefix:&'a ObjectPrefix,owner:&'a str,epoch:u64,key:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{Box::pin(async move{af::parse_removal_owner(owner)?;if !prefix.matches(key){return Err(NarouError::Platform("closed deletion outside novel".into()));}run_scoped_blocking(||self.with_publication_transaction(prefix,owner,epoch,|conn|{
        self.remove_file_copy(key)?;conn.execute("DELETE FROM transfer_object_refs WHERE object_key=?",[key.as_ref()]).map_err(super::sqlite_error)?;conn.execute("DELETE FROM object_chunks WHERE object_key=?",[key.as_ref()]).map_err(super::sqlite_error)?;conn.execute("DELETE FROM objects WHERE object_key=?",[key.as_ref()]).map_err(super::sqlite_error)?;
        let path=self.mirror.resolve_path(key)?;self.mirror.prune_empty_dirs_in_prefix(prefix,&path)
    }))})}
    fn update_small_guarded<'a,'data:'a>(&'a self,fence:&'a WriteFence,key:&'a ObjectKey,mutate:&'a mut SmallObjectMutator<'data>)->PlatformFuture<'a,Result<ObjectUpdateResult>>{Box::pin(async move{run_scoped_blocking(||self.mirror.with_transfer_lock(fence.prefix.as_ref().trim_end_matches('/'),||{
        if !fence.prefix.matches(key){return Err(NarouError::Platform("atomic update outside fence".into()));}
        let mut conn=self.conn.lock();let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(super::sqlite_error)?;require_transfer_epoch(&tx,&fence.prefix,None,fence.epoch,false)?;let limit=crate::native::object_store::MAX_SMALL_OBJECT_BYTES;
        let current=match read_stored_payload(&tx,key,Some(limit))?{Some(payload)=>Some(decode_stored_payload(payload,key,Some(limit))?),None=>self.mirror.read_small_blocking(key)?};
        let Some(next)=mutate(current.as_deref())?else{tx.commit().map_err(super::sqlite_error)?;return Ok(ObjectUpdateResult::Committed);};if next.len() as u64>limit{return Err(NarouError::Platform("atomic small write exceeds cap".into()));}
        let(stored,encoding)=crate::platform::encode_object_payload(&next);insert_stored_object(&tx,StoredObject{key,size:next.len() as i64,crc32:crate::platform::object_crc32(&next) as i64,stored:&stored,encoding,content_type:crate::platform::content_type_for_key(key),updated_at:&Self::now_rfc3339()})?;
        tx.commit().map_err(super::sqlite_error)?;drop(conn);
        if self.mirror_files&&let Err(error)=self.mirror.write_bytes_blocking(key,&next){return Ok(ObjectUpdateResult::ProjectionFailed(error));}Ok(ObjectUpdateResult::Committed)
    }))})}
    fn access_fence(&self)->Option<&dyn NovelAccessFence>{Some(self)}
    fn read_stream_leased<'a>(&'a self,prefix:&'a ObjectPrefix,lease:&'a GateLease,key:&'a ObjectKey,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<AssetStream>>>{Box::pin(async move{Ok(self.open_leased_asset(prefix,lease,key,range).await?.map(|(_,stream)|stream))})}
    fn write_small_guarded<'a>(&'a self,fence:&'a WriteFence,key:&'a ObjectKey,data:Vec<u8>)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        if !fence.prefix.matches(key)||data.len() as u64>crate::native::object_store::MAX_SMALL_OBJECT_BYTES{return Err(NarouError::Platform("invalid guarded small write".into()));}
        let(stored,encoding)=crate::platform::encode_object_payload(&data);
        run_scoped_blocking(||self.mirror.with_transfer_lock(fence.prefix.as_ref().trim_end_matches('/'),||{
            let mut conn=self.conn.lock();let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(super::sqlite_error)?;require_transfer_epoch(&tx,&fence.prefix,None,fence.epoch,false)?;
            insert_stored_object(&tx,StoredObject{key,size:data.len() as i64,crc32:crate::platform::object_crc32(&data) as i64,stored:&stored,encoding,content_type:crate::platform::content_type_for_key(key),updated_at:&Self::now_rfc3339()})?;
            if self.mirror_files{self.mirror.write_bytes_blocking(key,&data)?;}tx.commit().map_err(super::sqlite_error)
        }))
    })}
    fn delete_guarded<'a>(&'a self,fence:&'a WriteFence,key:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        if !fence.prefix.matches(key){return Err(NarouError::Platform("delete outside fence".into()));}
        run_scoped_blocking(||self.mirror.with_transfer_lock(fence.prefix.as_ref().trim_end_matches('/'),||{
            let mut conn=self.conn.lock();let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(super::sqlite_error)?;require_transfer_epoch(&tx,&fence.prefix,None,fence.epoch,false)?;self.remove_file_copy(key)?;
            tx.execute("DELETE FROM transfer_object_refs WHERE object_key=?",[key.as_ref()]).map_err(super::sqlite_error)?;
            tx.execute("DELETE FROM objects WHERE object_key=?",[key.as_ref()]).map_err(super::sqlite_error)?;tx.commit().map_err(super::sqlite_error)
        }))
    })}

    fn stat<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        let this = self.clone();
        let key = key.clone();
        run_blocking(move || {
            match this.stat_blocking(&key)? {
                Some(metadata) => Ok(Some(metadata)),
                // Pre-import or store-bypassed files still resolve via FS.
                None => {
                    let path = this.mirror.resolve_read_path(&key)?;
                    this.mirror.metadata_for_path(&key, &path)
                }
            }
        })
    }

    fn read_small<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        let this = self.clone();
        let key = key.clone();
        run_blocking(move || {
            match this.read_blocking(&key, Some(crate::native::object_store::MAX_SMALL_OBJECT_BYTES))? {
                Some(data) => Ok(Some(data)),
                None => this.mirror.read_small_blocking(&key),
            }
        })
    }

    fn write_small<'a>(
        &'a self,
        key: &'a ObjectKey,
        data: Vec<u8>,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            if let Some(fence)=run_scoped_blocking(||self.writer_for_key(key))?{return self.write_small_guarded(&fence,key,data).await;}
            let this=self.clone();let key=key.clone();run_blocking(move||{
                if data.len() as u64>crate::native::object_store::MAX_SMALL_OBJECT_BYTES{return Err(NarouError::Platform("small object exceeds write cap".into()));}
                if this.writer_for_key(&key)?.is_some(){return Err(NarouError::Platform("novel_epoch_conflict: admission changed".into()));}
                this.write_blocking(&key,&data)?;if this.mirror_files{this.mirror.write_bytes_blocking(&key,&data)?;}Ok(())
            }).await
        })
    }

    fn update_small<'a, 'data: 'a>(
        &'a self,
        key: &'a ObjectKey,
        mutate: &'a mut SmallObjectMutator<'data>,
    ) -> PlatformFuture<'a, Result<ObjectUpdateResult>> {
        Box::pin(async move {
            if let Some(fence)=run_scoped_blocking(||self.writer_for_key(key))?{return self.update_small_guarded(&fence,key,mutate).await;}
            run_scoped_blocking(|| self.update_small_blocking(key, mutate))
        })
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move{if let Some(fence)=run_scoped_blocking(||self.writer_for_key(key))?{self.delete_guarded(&fence,key).await}else{let this=self.clone();let key=key.clone();run_blocking(move||this.delete_both(&key)).await}})
    }

    fn delete_prefix<'a>(&'a self, prefix: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move{let fence=run_scoped_blocking(||self.writer_for_key(prefix))?;run_scoped_blocking(||match &fence{Some(fence)=>self.with_open_write(fence,||self.delete_prefix_blocking(prefix)),None=>self.delete_prefix_blocking(prefix)})})
    }

    fn list_page<'a>(
        &'a self,
        request: &'a ObjectListRequest,
    ) -> PlatformFuture<'a, Result<ObjectListPage>> {
        let this = self.clone();
        let request = request.clone();
        run_blocking(move || this.list_blocking(&request))
    }
}

impl AssetStore for SqliteObjectStore {
    fn read_stream_range<'a>(&'a self,key:&'a ObjectKey,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<AssetStream>>>{Box::pin(self.read_asset_stream_range(key,range))}
    fn write_stream_guarded<'a>(&'a self,fence:&'a WriteFence,key:&'a ObjectKey,stream:AssetStream)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        if !fence.prefix.matches(key){return Err(NarouError::Platform("write outside fence".into()));}
        let stage=self.stage_stream(stream).await?;
        let result=run_scoped_blocking(||self.with_open_fence_transaction(fence,|conn|self.install_reference(conn,&stage.key,key)));
        if result.is_err(){let _=run_scoped_blocking(||self.delete_both(&stage.key));}
        result
    })}
    fn write_stream_once<'a>(&'a self,key:&'a ObjectKey,stream:AssetStream,size:u64,sha:&'a str)->PlatformFuture<'a,Result<WriteOnce>>{Box::pin(async move{
        ts::check_hash_named_key(key,sha)?;let stage=self.stage_stream(stream).await?;
        let result:Result<WriteOnce> = async {
            let size=if size==u64::MAX{stage.size}else{size};
            if stage.size!=size||!stage.sha256.eq_ignore_ascii_case(sha){return Err(NarouError::Platform("write-once size/SHA mismatch".into()));}
            let before=run_scoped_blocking(||self.source_generation(key))?;
            if before.is_some(){let mut stream=self.read_stream(key).await?.ok_or_else(||NarouError::NotFound(key.to_string()))?;let mut seen=0u64;let mut hash=sha2::Sha256::new();
                while let Some(chunk)=stream.next().await{let bytes=chunk?;seen=seen.checked_add(bytes.len() as u64).filter(|value|*value<=size).ok_or_else(||NarouError::Platform("write-once existing size differs".into()))?;hash.update(&bytes);}
                if seen!=size||!hex::encode(hash.finalize()).eq_ignore_ascii_case(sha){return Err(NarouError::Platform("write-once existing bytes differ".into()));}
            }
            run_scoped_blocking(||self.mirror.with_transfer_lock(&format!("write-once/{key}"),||{
                if self.source_generation(key)?!=before{return Err(NarouError::Platform("write-once existing generation changed".into()));}
                if before.is_some(){return Ok(WriteOnce::Reused);}let mut conn=self.conn.lock();let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(super::sqlite_error)?;self.install_reference(&tx,&stage.key,key)?;tx.commit().map_err(super::sqlite_error)?;Ok(WriteOnce::Written)
            }))
        }.await;
        if !matches!(result,Ok(WriteOnce::Written)){let _=run_scoped_blocking(||self.delete_both(&stage.key));}
        result
    })}

    fn stat<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        ObjectStore::stat(self, key)
    }

    fn read_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        Box::pin(self.read_asset(key))
    }

    fn read_range<'a>(
        &'a self, key: &'a ObjectKey, range: std::ops::Range<u64>,
    ) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(self.read_asset_range(key, range))
    }

    fn stage_stream<'a>(&'a self, stream: AssetStream) -> PlatformFuture<'a, Result<crate::platform::StagedAsset>> {
        Box::pin(async move { self.stage_local_stream(stream).await })
    }

    fn write_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
        stream: AssetStream,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move{if let Some(sha)=ts::hash_named_digest(key){return self.write_stream_once(key,stream,u64::MAX,&sha).await.map(|_|());}
            if let Some(fence)=run_scoped_blocking(||self.writer_for_key(key))?{return self.write_stream_guarded(&fence,key,stream).await;}self.write_asset(key,stream).await})
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        ObjectStore::delete(self, key)
    }

    fn copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            if source==destination{return Ok(());}let fence=run_scoped_blocking(||self.writer_for_key(destination))?;
            let stream=self.read_stream(source).await?.ok_or_else(||NarouError::NotFound(source.to_string()))?;
            match fence{Some(fence)=>self.write_stream_guarded(&fence,destination,stream).await,None=>self.write_stream(destination,stream).await}
        })
    }

    fn move_or_copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move{if source==destination{return Ok(());}let source_fence=run_scoped_blocking(||self.writer_for_key(source))?;
            self.copy(source,destination).await?;match source_fence{Some(fence)=>self.delete_guarded(&fence,source).await,None=>ObjectStore::delete(self,source).await}
        })
    }
}

/// Sanitize only a PRIVATE `VACUUM INTO` image, never the live connection.
/// Canonical refs and every reachable raw blob survive; unpublished transfer
/// state, reader leases and checkpoint payloads do not travel in a backup.
pub fn sanitize_transfer_snapshot(conn:&mut Connection)->Result<()>{
    let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(super::sqlite_error)?;
    tx.execute_batch("CREATE TEMP TABLE backup_transfer_live(key TEXT PRIMARY KEY) WITHOUT ROWID; INSERT INTO backup_transfer_live SELECT blob_key FROM transfer_object_refs WHERE backend='d1' AND blob_key!=''; INSERT OR IGNORE INTO backup_transfer_live SELECT object_key FROM objects WHERE object_key>='.narou-transfer/.refs/' AND object_key<'.narou-transfer/.refs0';
        DELETE FROM objects WHERE (substr(object_key,1,16)='.narou-transfer/' OR substr(object_key,1,15)='.narou-staging/') AND NOT EXISTS(SELECT 1 FROM backup_transfer_live l WHERE l.key=objects.object_key);
        DELETE FROM transfer_verified_objects WHERE NOT EXISTS(SELECT 1 FROM backup_transfer_live l WHERE l.key=transfer_verified_objects.blob_key);
        DELETE FROM novel_reader_leases; DELETE FROM transfer_storage_guards; UPDATE novel_access_gates SET closed=0,owner=NULL; DROP TABLE backup_transfer_live;").map_err(super::sqlite_error)?;
    let tables={let mut query=tx.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name GLOB 'transfer_*' AND name NOT IN ('transfer_object_refs','transfer_verified_objects')").map_err(super::sqlite_error)?;query.query_map([],|row|row.get::<_,String>(0)).map_err(super::sqlite_error)?.collect::<rusqlite::Result<Vec<_>>>().map_err(super::sqlite_error)?};
    for table in tables{tx.execute(&format!("DELETE FROM \"{}\"",table.replace('"',"\"\"")),[]).map_err(super::sqlite_error)?;}
    tx.commit().map_err(super::sqlite_error)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BloatReclaimReport {
    pub stage_ledgers: usize,
    pub stage_checkpoints: usize,
    pub unreferenced_blobs: usize,
    pub shadowed_objects: usize,
    pub internal_debris: usize,
    pub verified_orphans: usize,
    pub section_bodies: usize,
}

impl BloatReclaimReport {
    pub fn total(&self) -> usize {
        self.stage_ledgers
            + self.stage_checkpoints
            + self.unreferenced_blobs
            + self.shadowed_objects
            + self.internal_debris
            + self.verified_orphans
            + self.section_bodies
    }
}

fn sqlite_table_exists(conn: &Connection, name: &str) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |row| row.get(0),
    )
    .map_err(super::sqlite_error)
}

/// Reclaim leaked ordinary transfer stages, checkpoints, shadowed direct rows,
/// internal transfer debris, and orphaned section bodies.
pub fn reclaim_sqlite_bloat(
    conn: &mut Connection,
    limit: Option<usize>,
    min_age_secs: i64,
) -> Result<BloatReclaimReport> {
    let cap = limit.unwrap_or(i64::MAX as usize).min(i64::MAX as usize) as i64;
    let cutoff: Option<String> = (min_age_secs > 0).then(|| {
        (chrono::Utc::now() - chrono::Duration::seconds(min_age_secs))
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
    });
    let mut report = BloatReclaimReport::default();
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(super::sqlite_error)?;

    let has_objects = sqlite_table_exists(&tx, "objects")?;
    let has_ledgers = sqlite_table_exists(&tx, "transfer_stage_ledger")?;
    let has_refs = sqlite_table_exists(&tx, "transfer_object_refs")?;
    let has_verified = sqlite_table_exists(&tx, "transfer_verified_objects")?;
    let has_bodies = sqlite_table_exists(&tx, "section_bodies")?
        && sqlite_table_exists(&tx, "novel_sections")?
        && sqlite_table_exists(&tx, "novel_version_sections")?;

    if has_ledgers {
        let lower = "ordinary-";
        let upper = prefix_upper_bound(lower);
        report.stage_ledgers = if has_objects {
            tx.execute(
                "DELETE FROM transfer_stage_ledger WHERE stage_id IN (
                    SELECT stage_id FROM transfer_stage_ledger
                    WHERE stage_id >= ?1 AND stage_id < ?2
                      AND (?3 IS NULL OR NOT EXISTS (
                          SELECT 1 FROM objects o
                          WHERE o.object_key = '.narou-transfer/data/' || transfer_stage_ledger.stage_id
                            AND o.updated_at >= ?3
                      ))
                    LIMIT ?4
                )",
                rusqlite::params![lower, upper, cutoff.as_deref(), cap],
            )
            .map_err(super::sqlite_error)?
        } else {
            tx.execute(
                "DELETE FROM transfer_stage_ledger WHERE stage_id IN (
                    SELECT stage_id FROM transfer_stage_ledger
                    WHERE stage_id >= ?1 AND stage_id < ?2
                    LIMIT ?3
                )",
                rusqlite::params![lower, upper, cap],
            )
            .map_err(super::sqlite_error)?
        };
    }

    if has_objects {
        let cp_lower = ".narou-transfer/.checkpoints/ordinary-";
        let cp_upper = prefix_upper_bound(cp_lower);
        report.stage_checkpoints = tx
            .execute(
                "DELETE FROM objects WHERE object_key IN (
                    SELECT object_key FROM objects
                    WHERE object_key >= ?1 AND object_key < ?2
                      AND (?3 IS NULL OR updated_at < ?3)
                    LIMIT ?4
                )",
                rusqlite::params![cp_lower, cp_upper, cutoff.as_deref(), cap],
            )
            .map_err(super::sqlite_error)?;

        let blob_lower = ".narou-transfer/data/ordinary-";
        let blob_upper = prefix_upper_bound(blob_lower);
        report.unreferenced_blobs = match (has_refs, has_ledgers) {
            (true, true) => tx.execute(
                "DELETE FROM objects WHERE object_key IN (
                    SELECT object_key FROM objects
                    WHERE object_key >= ?1 AND object_key < ?2
                      AND (?3 IS NULL OR updated_at < ?3)
                      AND NOT EXISTS (SELECT 1 FROM transfer_object_refs WHERE blob_key = objects.object_key)
                      AND NOT EXISTS (SELECT 1 FROM transfer_stage_ledger WHERE stage_id = substr(objects.object_key, length('.narou-transfer/data/') + 1))
                    LIMIT ?4
                )",
                rusqlite::params![blob_lower, blob_upper, cutoff.as_deref(), cap],
            ),
            (true, false) => tx.execute(
                "DELETE FROM objects WHERE object_key IN (
                    SELECT object_key FROM objects
                    WHERE object_key >= ?1 AND object_key < ?2
                      AND (?3 IS NULL OR updated_at < ?3)
                      AND NOT EXISTS (SELECT 1 FROM transfer_object_refs WHERE blob_key = objects.object_key)
                    LIMIT ?4
                )",
                rusqlite::params![blob_lower, blob_upper, cutoff.as_deref(), cap],
            ),
            (false, true) => tx.execute(
                "DELETE FROM objects WHERE object_key IN (
                    SELECT object_key FROM objects
                    WHERE object_key >= ?1 AND object_key < ?2
                      AND (?3 IS NULL OR updated_at < ?3)
                      AND NOT EXISTS (SELECT 1 FROM transfer_stage_ledger WHERE stage_id = substr(objects.object_key, length('.narou-transfer/data/') + 1))
                    LIMIT ?4
                )",
                rusqlite::params![blob_lower, blob_upper, cutoff.as_deref(), cap],
            ),
            (false, false) => tx.execute(
                "DELETE FROM objects WHERE object_key IN (
                    SELECT object_key FROM objects
                    WHERE object_key >= ?1 AND object_key < ?2
                      AND (?3 IS NULL OR updated_at < ?3)
                    LIMIT ?4
                )",
                rusqlite::params![blob_lower, blob_upper, cutoff.as_deref(), cap],
            ),
        }
        .map_err(super::sqlite_error)?;

        if has_refs {
            report.shadowed_objects = tx
                .execute(
                    "DELETE FROM objects WHERE object_key IN (
                        SELECT o.object_key FROM objects o
                        WHERE EXISTS (SELECT 1 FROM transfer_object_refs r WHERE r.object_key = o.object_key)
                          AND NOT EXISTS (SELECT 1 FROM transfer_object_refs rb WHERE rb.blob_key = o.object_key)
                        LIMIT ?1
                    )",
                    rusqlite::params![cap],
                )
                .map_err(super::sqlite_error)?;
        }

        for prefix in [
            ".narou-transfer/.lock/",
            ".narou-transfer/.gate/",
            ".narou-transfer/.gate.pending/",
            ".narou-transfer/.proof/",
        ] {
            let upper = prefix_upper_bound(prefix);
            report.internal_debris += tx
                .execute(
                    "DELETE FROM objects WHERE object_key IN (
                        SELECT object_key FROM objects
                        WHERE object_key >= ?1 AND object_key < ?2
                        LIMIT ?3
                    )",
                    rusqlite::params![prefix, upper, cap],
                )
                .map_err(super::sqlite_error)?;
        }
        report.internal_debris += tx
            .execute(
                "DELETE FROM objects WHERE object_key IN (
                    SELECT object_key FROM objects
                    WHERE object_key >= '.narou-staging/' AND object_key < '.narou-staging0'
                      AND (?1 IS NULL OR updated_at < ?1)
                    LIMIT ?2
                )",
                rusqlite::params![cutoff.as_deref(), cap],
            )
            .map_err(super::sqlite_error)?;
    }

    if has_verified && has_objects {
        report.verified_orphans = tx
            .execute(
                "DELETE FROM transfer_verified_objects WHERE blob_key IN (
                    SELECT blob_key FROM transfer_verified_objects
                    WHERE NOT EXISTS (SELECT 1 FROM objects o WHERE o.object_key = transfer_verified_objects.blob_key)
                    LIMIT ?1
                )",
                rusqlite::params![cap],
            )
            .map_err(super::sqlite_error)?;
    }

    if has_bodies {
        report.section_bodies = tx
            .execute(
                "DELETE FROM section_bodies WHERE body_hash IN (
                    SELECT body_hash FROM section_bodies
                    WHERE NOT EXISTS (SELECT 1 FROM novel_sections s WHERE s.body_hash = section_bodies.body_hash)
                      AND NOT EXISTS (SELECT 1 FROM novel_version_sections v WHERE v.body_hash = section_bodies.body_hash)
                    LIMIT ?1
                )",
                rusqlite::params![cap],
            )
            .map_err(super::sqlite_error)?;
    }

    tx.commit().map_err(super::sqlite_error)?;
    Ok(report)
}

/// Payload bytes (raw + stored) one import transaction may carry. Keeps
/// transient memory bounded on large archives while still amortizing commit
/// and lock costs across a batch.
const IMPORT_BATCH_BYTES: usize = 32 * 1024 * 1024;

/// Row cap for one import transaction (many small files).
const IMPORT_BATCH_ROWS: usize = 256;

/// One object staged for the import batch: payloads and empty-version markers
/// are compressed outside the connection lock and inserted in a bounded transaction.
struct ImportedObject {
    key: ObjectKey,
    size: i64,
    crc32: i64,
    stored: Vec<u8>,
    encoding: ObjectEncoding,
}

enum ArchiveImportEntry {
    File(PathBuf),
    EmptyCacheVersion(ObjectKey),
}

fn is_import_ignored(safe_root: &Path, path: &Path, is_dir: bool) -> bool {
    let Ok(rel) = path.strip_prefix(safe_root) else {
        return false;
    };
    let rel = rel.to_string_lossy().replace('\\', "/");
    if rel == ".objects/.narou-staging" || rel.starts_with(".objects/.narou-staging/") {
        return true;
    }
    if let Some(rest) = rel.strip_prefix(".objects/.narou-transfer/")
        && rest != ".refs"
        && !rest.starts_with(".refs/")
    {
        return true;
    }
    if !is_dir {
        let name = rel.rsplit('/').next().unwrap_or(&rel);
        if name.ends_with(".lock")
            || name.starts_with(".narou-export-")
            || name.starts_with(".narou-asset-")
        {
            return true;
        }
    }
    false
}

/// Preserve only empty version directories in the recognized novel cache
/// layout, not arbitrary empty folders. Existing marker files are ordinary
/// files and retain their payloads through the importer.
fn collect_archive_import_entries(
    safe_root: &Path,
    current: &Path,
    output: &mut Vec<ArchiveImportEntry>,
    ancestors: &mut Vec<PathBuf>,
) -> Result<()> {
    let safe_current = crate::db::paths::ensure_within_archive_root(current, safe_root)?;
    if ancestors.contains(&safe_current) {
        return Err(NarouError::Platform("cyclic native namespace link".into()));
    }
    ancestors.push(safe_current.clone());
    let result = (|| {
        let mut children = Vec::new();
        for entry in std::fs::read_dir(&safe_current)? {
            children.push(entry?.path());
        }
        let mut has_entries = false;
        for raw_path in children {
            let path = crate::db::paths::ensure_within_archive_root(&raw_path, safe_root)?;
            let is_dir = path.is_dir();
            if is_import_ignored(safe_root, &path, is_dir) {
                continue;
            }
            has_entries = true;
            if is_dir {
                collect_archive_import_entries(safe_root, &path, output, ancestors)?;
            } else if path.is_file() {
                output.push(ArchiveImportEntry::File(path));
            }
        }
        if !has_entries {
            let novel_dir = safe_current.parent().and_then(Path::parent).and_then(Path::parent);
            if let Some(novel_dir) = novel_dir
                && let (Some(prefix), Some(marker)) = (
                    logical_key_for_native_path(safe_root, novel_dir),
                    logical_key_for_native_path(safe_root, &safe_current.join(".version")),
                )
            {
                let prefix_depth = prefix.as_ref().split('/').count();
                if matches!(prefix_depth, 3 | 4)
                    && prefix.as_ref().starts_with("novels/")
                    && crate::application::webui::diff::cached_object_parts(
                        prefix.as_ref(),
                        marker.as_ref(),
                    )
                    .is_some()
                {
                    output.push(ArchiveImportEntry::EmptyCacheVersion(marker));
                }
            }
        }
        Ok(())
    })();
    ancestors.pop();
    result
}

/// Import archive files and legitimate empty cache versions into the object tables.
/// Runs once when the table is empty; the upserts keep a partial import
/// resumable on the next start.
///
/// Filesystem reads and compression run one file at a time *outside* the
/// connection lock; the lock is only taken for a bounded transaction that
/// inserts up to [`IMPORT_BATCH_BYTES`]/[`IMPORT_BATCH_ROWS`] rows with
/// (connection-)cached statements — instead of one lock+transaction per file.
pub(crate) fn import_archive_into_objects(
    conn: &Arc<Mutex<Connection>>,
    mirror: &NativeObjectStore,
) -> Result<usize> {
    let root = mirror.root().to_path_buf();
    let mut entries = Vec::new();
    if root.is_dir() {
        let safe_root = crate::db::paths::ensure_within_archive_root(&root, &root)?;
        collect_archive_import_entries(&safe_root, &safe_root, &mut entries, &mut Vec::new())?;
    }
    if entries.is_empty() {
        return Ok(0);
    }
    let mut imported = 0usize;
    let mut batch: Vec<ImportedObject> = Vec::new();
    let mut batch_bytes = 0usize;
    for entry in entries {
        let (key, data) = match entry {
            ArchiveImportEntry::File(path) => {
                let Some(key) = logical_key_for_native_path(&root, &path) else {
                    continue;
                };
                (key, std::fs::read(&path)?)
            }
            ArchiveImportEntry::EmptyCacheVersion(key) => (key, Vec::new()),
        };
        let size = data.len() as i64;
        let crc32 = crate::platform::object_crc32(&data) as i64;
        let (stored, encoding) = crate::platform::compress_object_payload(&data);
        batch_bytes = batch_bytes
            .saturating_add(data.len())
            .saturating_add(stored.len());
        batch.push(ImportedObject {
            key,
            size,
            crc32,
            stored,
            encoding,
        });
        imported += 1;
        if batch_bytes >= IMPORT_BATCH_BYTES || batch.len() >= IMPORT_BATCH_ROWS {
            flush_import_batch(conn, &mut batch)?;
            batch_bytes = 0;
        }
    }
    flush_import_batch(conn, &mut batch)?;
    Ok(imported)
}

/// Insert one staged batch in a single transaction and release the payloads.
fn flush_import_batch(
    conn: &Arc<Mutex<Connection>>,
    batch: &mut Vec<ImportedObject>,
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let updated_at = SqliteObjectStore::now_rfc3339();
    let mut guard = conn.lock();
    let tx = guard.transaction().map_err(super::sqlite_error)?;
    for object in batch.iter() {
        insert_stored_object(
            &tx,
            StoredObject {
                key: &object.key,
                size: object.size,
                crc32: object.crc32,
                stored: &object.stored,
                encoding: object.encoding,
                content_type: content_type_for_path(Path::new(object.key.as_ref())),
                updated_at: &updated_at,
            },
        )?;
    }
    tx.commit().map_err(super::sqlite_error)?;
    batch.clear();
    Ok(())
}

/// Move legacy `body_yaml` payloads into the content-addressed
/// `section_bodies` table and link rows via `body_hash`. Runs inside
/// `migrations::apply` between 0009 (adds the columns) and 0010 (drops
/// `body_yaml`). Idempotent: rows already carrying a hash are skipped, and
/// the insert-if-absent body batch keeps reruns cheap.
pub(crate) fn migrate_section_bodies(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction().map_err(super::sqlite_error)?;
    // Both tables are WITHOUT ROWID: identify rows by their primary key.
    for (table, pk1, pk2) in [
        ("novel_sections", "novel_id", "idx"),
        ("novel_version_sections", "version_id", "idx"),
    ] {
        let select = format!("SELECT {pk1}, {pk2}, body_yaml FROM {table} WHERE body_hash IS NULL");
        let rows: Vec<(i64, String, String)> = {
            let mut statement = tx.prepare(&select).map_err(super::sqlite_error)?;
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .map_err(super::sqlite_error)?
                .collect::<std::result::Result<_, _>>()
                .map_err(super::sqlite_error)?
        };
        // One existence lookup and one prepared UPDATE for the whole table
        // instead of a `SELECT`+`INSERT`+`UPDATE` triple per row.
        let bodies: Vec<&str> = rows.iter().map(|(_, _, body)| body.as_str()).collect();
        let hashes = super::content::store_bodies(&tx, &bodies)?;
        let update = format!("UPDATE {table} SET body_hash = ? WHERE {pk1} = ? AND {pk2} = ?");
        let mut update = tx.prepare_cached(&update).map_err(super::sqlite_error)?;
        for ((pk1, pk2, _), hash) in rows.iter().zip(hashes.iter()) {
            update
                .execute(rusqlite::params![hash, pk1, pk2])
                .map_err(super::sqlite_error)?;
        }
    }
    tx.commit().map_err(super::sqlite_error)
}


/// One stored row that `db verify` could not read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PayloadCorruption {
    /// Object key, or `section_bodies:<hex hash>` for a deduplicated body.
    pub(crate) key: String,
    /// Decode/decompression failure or CRC-32 mismatch detail.
    pub(crate) reason: String,
}

/// Rows read per verify page: bounds transient memory (an inline payload is at
/// most one chunk) while keeping the scan to a few statements per table.
const VERIFY_PAGE_ROWS: i64 = 128;

/// Verify every stored payload against its CRC-32 after decompression.
/// Returns one entry per corrupted row — exact key plus failure reason — for
/// `narou db verify`. Skips when the encoding/crc32 columns predate the schema.
pub(crate) fn verify_payload_crc32(conn: &Connection) -> Result<Vec<PayloadCorruption>> {
    let has_encoding: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('objects') WHERE name = 'encoding'")
        .and_then(|mut s| s.exists([]))
        .unwrap_or(false);
    if !has_encoding {
        return Ok(Vec::new());
    }
    let mut corruptions = Vec::new();

    // Inline objects, paged by primary key so one page's payloads are the only
    // ones resident.
    let mut cursor = String::new();
    loop {
        let page: Vec<(String, Vec<u8>, String, i64)> = {
            let mut statement = conn
                .prepare(
                    "SELECT object_key, data, encoding, crc32 FROM objects
                     WHERE data IS NOT NULL AND object_key > ?1
                     ORDER BY object_key LIMIT ?2",
                )
                .map_err(super::sqlite_error)?;
            statement
                .query_map(rusqlite::params![cursor, VERIFY_PAGE_ROWS], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })
                .map_err(super::sqlite_error)?
                .collect::<std::result::Result<_, _>>()
                .map_err(super::sqlite_error)?
        };
        let Some((last, _, _, _)) = page.last() else {
            break;
        };
        cursor = last.clone();
        for (key, stored, encoding, crc32) in page {
            if let Err(reason) = verify_stored_payload(&stored, &encoding, crc32) {
                corruptions.push(PayloadCorruption { key, reason });
            }
        }
    }

    // Chunked objects: the metadata and chunk streams are each read once, in
    // key order, and merged — no `SELECT` per object, and only one object's
    // stored bytes are held at a time.
    {
        let mut meta_statement = conn
            .prepare(
                "SELECT object_key, encoding, crc32 FROM objects
                 WHERE data IS NULL ORDER BY object_key",
            )
            .map_err(super::sqlite_error)?;
        let mut meta_rows = meta_statement.query([]).map_err(super::sqlite_error)?;
        let mut chunk_statement = conn
            .prepare("SELECT object_key, data FROM object_chunks ORDER BY object_key, seq")
            .map_err(super::sqlite_error)?;
        let mut chunk_rows = chunk_statement.query([]).map_err(super::sqlite_error)?;

        let mut meta = next_verify_meta(&mut meta_rows)?;
        let mut chunk = next_verify_chunk(&mut chunk_rows)?;
        while let Some((meta_key, encoding, crc32)) = meta {
            // Raw (encoding=none) objects — including multi-GiB imported
            // payloads — are checksummed incrementally over each 64KiB
            // chunk row so the object is never assembled in memory.
            // Compressed encodings keep the bounded assemble/decode path.
            let raw = matches!(ObjectEncoding::parse(&encoding), Ok(ObjectEncoding::None));
            let mut stored = Vec::new();
            let mut hasher = crc32fast::Hasher::new();
            while let Some((chunk_key, data)) = chunk.take() {
                if chunk_key.as_str() < meta_key.as_str() {
                    // Leftover chunks without an object row: ignored, as before.
                    chunk = next_verify_chunk(&mut chunk_rows)?;
                    continue;
                }
                if chunk_key == meta_key {
                    if raw { hasher.update(&data); } else { stored.extend_from_slice(&data); }
                    chunk = next_verify_chunk(&mut chunk_rows)?;
                    continue;
                }
                // Belongs to the next object; keep it for the next round.
                chunk = Some((chunk_key, data));
                break;
            }
            if raw {
                let actual = hasher.finalize();
                if actual != crc32 as u32 {
                    corruptions.push(PayloadCorruption {
                        key: meta_key,
                        reason: format!("object payload CRC mismatch: expected {:08x}, got {actual:08x}", crc32 as u32),
                    });
                }
            } else if let Err(reason) = verify_stored_payload(&stored, &encoding, crc32) {
                corruptions.push(PayloadCorruption {
                    key: meta_key,
                    reason,
                });
            }
            meta = next_verify_meta(&mut meta_rows)?;
        }
    }

    // section_bodies: always inline, paged by hash. The cursor is nullable so
    // the first page carries no lower bound (an empty blob hash must not be
    // skipped) while later pages stay a strict keyset walk.
    let mut cursor: Option<Vec<u8>> = None;
    loop {
        let page: Vec<(Vec<u8>, Vec<u8>, String, i64)> = {
            let mut statement = conn
                .prepare(
                    "SELECT body_hash, data, encoding, crc32 FROM section_bodies
                     WHERE ?1 IS NULL OR body_hash > ?1
                     ORDER BY body_hash LIMIT ?2",
                )
                .map_err(super::sqlite_error)?;
            statement
                .query_map(rusqlite::params![cursor, VERIFY_PAGE_ROWS], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })
                .map_err(super::sqlite_error)?
                .collect::<std::result::Result<_, _>>()
                .map_err(super::sqlite_error)?
        };
        let Some((last, _, _, _)) = page.last() else {
            break;
        };
        cursor = Some(last.clone());
        for (hash, stored, encoding, crc32) in page {
            if let Err(reason) = verify_stored_payload(&stored, &encoding, crc32) {
                corruptions.push(PayloadCorruption {
                    key: format!("section_bodies:{}", hex_digest(&hash)),
                    reason,
                });
            }
        }
    }

    Ok(corruptions)
}

fn next_verify_meta(rows: &mut rusqlite::Rows<'_>) -> Result<Option<(String, String, i64)>> {
    match rows.next().map_err(super::sqlite_error)? {
        Some(row) => Ok(Some((
            row.get(0).map_err(super::sqlite_error)?,
            row.get(1).map_err(super::sqlite_error)?,
            row.get(2).map_err(super::sqlite_error)?,
        ))),
        None => Ok(None),
    }
}

fn next_verify_chunk(rows: &mut rusqlite::Rows<'_>) -> Result<Option<(String, Vec<u8>)>> {
    match rows.next().map_err(super::sqlite_error)? {
        Some(row) => Ok(Some((
            row.get(0).map_err(super::sqlite_error)?,
            row.get(1).map_err(super::sqlite_error)?,
        ))),
        None => Ok(None),
    }
}

/// Decompress + CRC-check one stored payload; `Err` carries the reason so the
/// caller can report the exact row instead of silently counting it.
fn verify_stored_payload(
    stored: &[u8],
    encoding: &str,
    crc32: i64,
) -> std::result::Result<(), String> {
    let encoding = ObjectEncoding::parse(encoding).map_err(|error| error.to_string())?;
    let data = crate::platform::decompress_object_payload(stored, encoding)
        .map_err(|error| error.to_string())?;
    crate::platform::verify_object_crc32(&data, crc32 as u32).map_err(|error| error.to_string())
}

/// Lowercase hex of a SHA-256 body hash (the `section_bodies` identifier).
fn hex_digest(hash: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(hash.len() * 2);
    for byte in hash {
        let _ = write!(out, "{byte:02x}");
    }
    out
}
use crate::platform::{transfer_stage as ts, access_fence as af, TransferStageStore, NovelAccessFence,
    StageCursor, SealedAsset, StageLease, GateLease, GateInfo, GateState, WriteFence, WriteOnce};
use rusqlite::OptionalExtension;
use sha2::Digest;

const NATIVE_LIST_PAGE_SQL:&str=r#"
SELECT object_key,size,content_type,updated_at FROM native_logical_objects WHERE object_key=?1 AND object_key>?4
UNION ALL SELECT object_key,size,content_type,updated_at FROM native_logical_objects WHERE ?1!='' AND object_key>=max(?2,?4) AND object_key<?3 AND object_key>?4
UNION ALL SELECT object_key,size,content_type,updated_at FROM native_logical_objects WHERE ?1='' AND object_key>?4
ORDER BY object_key LIMIT ?5
"#;

fn stage_row(conn: &Connection, id: &str) -> Result<Option<ts::StageLedgerRecord>> {
    let value: Option<String> = conn.query_row("SELECT record_json FROM transfer_stage_ledger WHERE stage_id=?",[id],|row|row.get(0)).optional().map_err(super::sqlite_error)?;
    value.map(|value|ts::decode_ledger(value.as_bytes())).transpose()
}
fn put_stage_row(conn: &Connection, record: &ts::StageLedgerRecord) -> Result<()> {
    let json=String::from_utf8(ts::encode_ledger(record)?).map_err(|error|NarouError::Platform(error.to_string()))?;
    conn.execute("INSERT INTO transfer_stage_ledger(stage_id,record_json) VALUES (?,?) ON CONFLICT(stage_id) DO UPDATE SET record_json=excluded.record_json,revision=revision+1",
        rusqlite::params![record.lease.stage_id,json]).map_err(super::sqlite_error)?;
    if record.lease.state==ts::StageLeaseState::Writing{let key=ts::stage_checkpoint_key(&record.lease.stage_id,record.confirmed_bytes)?;let bytes=serde_json::to_vec(&record.cursor()).map_err(|error|NarouError::Platform(error.to_string()))?;
        insert_stored_object(conn,StoredObject{key:&key,size:bytes.len() as i64,crc32:crate::platform::object_crc32(&bytes) as i64,stored:&bytes,encoding:ObjectEncoding::None,content_type:Some("application/json"),updated_at:&SqliteObjectStore::now_rfc3339()})?;}Ok(())
}

/// Must be called inside the SAME transaction as the guarded metadata/content commit.
/// Owner-scoped callers require the CLOSED gate owned by `owner`; owner-less
/// callers require the OPEN gate — both at the exact supplied epoch. Any
/// committed SQL→FS handoff retires every SQL token for the prefix, and a
/// row created afterwards can never revive authority by accident.
pub fn require_transfer_epoch(conn:&Connection,prefix:&ObjectPrefix,owner:Option<&str>,epoch:u64,drained:bool)->Result<()> {
    let row:Option<(u64,i64,Option<String>)>=conn.query_row("SELECT epoch,closed,owner FROM novel_access_gates WHERE prefix=?",
        [prefix.as_ref().trim_end_matches('/')],|row|Ok((super::read_u64(row,0)?,row.get(1)?,row.get(2)?))).optional().map_err(super::sqlite_error)?;
    let Some((current,closed,current_owner))=row else { return Err(af::gate_closed_error(prefix)); };
    if crate::native::object_store::read_fs_gate_handoff_raw(conn,prefix.as_ref().trim_end_matches('/'))?.is_some(){return Err(af::gate_closed_error(prefix));}
    match owner {
        Some(owner)=>{
            if closed==0||current_owner.as_deref()!=Some(owner)||current!=epoch { return Err(af::gate_epoch_error(prefix,epoch,current)); }
        }
        None=>{
            if closed!=0||current!=epoch { return Err(af::gate_epoch_error(prefix,epoch,current)); }
        }
    }
    if drained {
        let count:i64=conn.query_row("SELECT count(*) FROM novel_reader_leases WHERE prefix=? AND expires_at>?",
            rusqlite::params![prefix.as_ref().trim_end_matches('/'),ts::now_secs()],|row|row.get(0)).map_err(super::sqlite_error)?;
        if count!=0 { return Err(af::novel_busy_error(prefix)); }
    } Ok(())
}

impl SqliteObjectStore {
    pub(crate) fn source_generation(&self,key:&ObjectKey)->Result<Option<String>>{
        let conn=self.conn.lock();let physical=resolve_payload_key(&conn,key)?;let generation:Option<String>=conn.query_row("SELECT generation FROM objects WHERE object_key=?",[physical.as_ref()],|row|row.get(0)).optional().map_err(super::sqlite_error)?;
        drop(conn);match generation{Some(generation)=>Ok(Some(format!("db:{physical}:{generation}"))),None=>self.mirror.file_generation(key)}
    }
    fn writer_for_key(&self,key:&ObjectKey)->Result<Option<WriteFence>>{
        if crate::platform::is_reserved_key(key){return Ok(None);}
        let prefix:Option<String>={let conn=self.conn.lock();conn.query_row("SELECT prefix FROM (SELECT prefix FROM novel_access_gates UNION SELECT prefix FROM novel_gate_bindings) WHERE ?1=prefix OR substr(?1,1,length(prefix)+1)=prefix||'/' ORDER BY length(prefix) DESC LIMIT 1",[key.as_ref()],|row|row.get(0)).optional().map_err(super::sqlite_error)?};
        let Some(prefix)=prefix else{return Ok(None);};let prefix=ObjectPrefix::new(prefix)?;
        self.gate_transaction(&prefix,|conn|{let info=Self::gate_info_conn(conn,&prefix)?;if info.state!=GateState::Open{return Err(af::gate_closed_error(&prefix));}Ok(Some(WriteFence::new(prefix.clone(),info.epoch)))})
    }
    pub(crate) fn stored_encoding(&self,key:&ObjectKey)->Result<Option<ObjectEncoding>>{
        let conn=self.conn.lock();let key=resolve_payload_key(&conn,key)?;
        let value:Option<String>=conn.query_row("SELECT encoding FROM objects WHERE object_key=?",[key.as_ref()],|row|row.get(0)).optional().map_err(super::sqlite_error)?;value.map(|value|ObjectEncoding::parse(&value)).transpose()
    }
    pub(crate) fn transfer_fingerprint(&self,key:&ObjectKey)->Result<Option<(u64,String)>>{
        let conn=self.conn.lock();let physical=resolve_payload_key(&conn,key)?;
        let value:Option<(u64,String,String)>=conn.query_row("SELECT v.size,v.sha256,v.generation FROM transfer_verified_objects v JOIN objects o ON o.object_key=v.blob_key AND o.generation=v.generation WHERE v.blob_key=?",[physical.as_ref()],|row|Ok((super::read_u64(row,0)?,row.get(1)?,row.get(2)?))).optional().map_err(super::sqlite_error)?;
        if let Some((size,sha,_))=value{if self.mirror_files&&!crate::platform::is_reserved_key(key){let mirror=self.mirror.trusted_file_fingerprint(key)?.ok_or_else(||NarouError::Platform("canonical mirror proof missing".into()))?;if mirror.0!=size||!mirror.1.eq_ignore_ascii_case(&sha){return Err(NarouError::Platform("canonical mirror fingerprint conflict".into()));}}return Ok(Some((size,sha)));}Ok(None)
    }
    pub(super) fn record_verified_object(conn:&Connection,key:&ObjectKey,size:u64,sha:&str)->Result<()>{
        let (actual,encoding,generation):(u64,String,String)=conn.query_row("SELECT size,encoding,generation FROM objects WHERE object_key=?",[key.as_ref()],|row|Ok((super::read_u64(row,0)?,row.get(1)?,row.get(2)?))).map_err(super::sqlite_error)?;
        if actual!=size||encoding!="none"{return Err(NarouError::Platform("raw stage metadata changed".into()));}
        conn.execute("INSERT INTO transfer_verified_objects(blob_key,size,sha256,generation) VALUES(?,?,?,?) ON CONFLICT(blob_key) DO UPDATE SET size=excluded.size,sha256=excluded.sha256,generation=excluded.generation",rusqlite::params![key.as_ref(),size as i64,sha.to_ascii_lowercase(),generation]).map_err(super::sqlite_error)?;Ok(())
    }
    fn install_reference(&self,conn:&Connection,stage:&ObjectKey,key:&ObjectKey)->Result<()>{
        let (size,sha,generation):(u64,String,String)=conn.query_row("SELECT v.size,v.sha256,v.generation FROM transfer_verified_objects v JOIN objects o ON o.object_key=v.blob_key AND o.generation=v.generation WHERE v.blob_key=?",[stage.as_ref()],|row|Ok((super::read_u64(row,0)?,row.get(1)?,row.get(2)?))).optional().map_err(super::sqlite_error)?.ok_or_else(||NarouError::Platform("sealed stage proof missing or changed".into()))?;
        let same:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM transfer_object_refs WHERE object_key=? AND blob_key=? AND blob_generation=? AND sha256=?)",rusqlite::params![key.as_ref(),stage.as_ref(),generation,sha],|row|row.get(0)).map_err(super::sqlite_error)?;
        if self.mirror_files{let installed=self.mirror.install_verified_file(stage,key)?;if installed.0!=size||!installed.1.eq_ignore_ascii_case(&sha){return Err(NarouError::Platform("mirror stage fingerprint differs".into()));}}
        if stage!=key{conn.execute("DELETE FROM objects WHERE object_key=?",[key.as_ref()]).map_err(super::sqlite_error)?;}
        if same{return Ok(());}
        conn.execute("INSERT INTO transfer_object_refs(object_key,blob_key,size,sha256,backend,updated_at,blob_generation) VALUES(?,?,?,?,'d1',?,?) ON CONFLICT(object_key) DO UPDATE SET blob_key=excluded.blob_key,size=excluded.size,sha256=excluded.sha256,backend=excluded.backend,updated_at=excluded.updated_at,blob_generation=excluded.blob_generation",rusqlite::params![key.as_ref(),stage.as_ref(),size as i64,sha,Self::now_rfc3339(),generation]).map_err(super::sqlite_error)?;Ok(())
    }
    fn append_mirror_stage(&self,key:&ObjectKey,offset:u64,payload:&[u8])->Result<()>{
        if !self.mirror_files{return Ok(());}use std::io::{Seek,SeekFrom,Write};let path=self.mirror.resolve_path(key)?;self.mirror.ensure_parent_dir(&path)?;
        let mut file=std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(path)?;
        if file.metadata()?.len()<offset{return Err(NarouError::Platform("confirmed mirror stage prefix disappeared".into()));}file.set_len(offset)?;file.seek(SeekFrom::Start(offset))?;file.write_all(payload)?;file.sync_all()?;Ok(())
    }
    fn stage_transaction<T>(&self,id:&str,operation:impl FnOnce(&Connection,Option<ts::StageLedgerRecord>)->Result<(T,Option<ts::StageLedgerRecord>)>)->Result<T> {
        ts::stage_data_key(id)?;
        let mut conn=self.conn.lock(); let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(super::sqlite_error)?;
        let current=stage_row(&tx,id)?; let (value,next)=operation(&tx,current)?;
        if let Some(next)=next { put_stage_row(&tx,&next)?; } tx.commit().map_err(super::sqlite_error)?; Ok(value)
    }
    pub fn with_publication_transaction<T>(&self,prefix:&ObjectPrefix,owner:&str,epoch:u64,operation:impl FnOnce(&Connection)->Result<T>)->Result<T> {
        self.mirror.with_transfer_lock(prefix.as_ref().trim_end_matches('/'),|| {
            let mut conn=self.conn.lock(); let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(super::sqlite_error)?;
            require_transfer_epoch(&tx,prefix,Some(owner),epoch,true)?; let value=operation(&tx)?; tx.commit().map_err(super::sqlite_error)?; Ok(value)
        })
    }
    pub fn with_reader_transaction<T>(&self,prefix:&ObjectPrefix,lease:&GateLease,operation:impl FnOnce(&Connection)->Result<T>)->Result<T>{
        let epoch=i64::try_from(lease.epoch).map_err(|_|af::gate_epoch_error(prefix,lease.epoch,u64::MAX))?;
        self.mirror.with_reader_gate_lock(prefix.as_ref().trim_end_matches('/'),||{
            let conn=self.conn.lock();require_transfer_epoch(&conn,prefix,None,lease.epoch,false)?;
            // A token whose row is missing or past `expires_at` is a dead
            // admission, not a closed gate: the gate is still open at the
            // same epoch, so keep the cause distinct for the retry path. A
            // same-reader-id row at a *different* epoch is a stale token from
            // a re-admission, which stays `novel_recovering` (gate boundary).
            let row:Option<(i64,i64)>=conn.query_row("SELECT epoch,expires_at FROM novel_reader_leases WHERE prefix=? AND reader_id=?",rusqlite::params![prefix.as_ref().trim_end_matches('/'),lease.reader_id],|row|Ok((row.get(0)?,row.get(1)?))).optional().map_err(super::sqlite_error)?;
            match row {
                Some((row_epoch,expiry)) if row_epoch==epoch&&expiry>ts::now_secs() => operation(&conn),
                Some((row_epoch,_)) if row_epoch!=epoch => Err(af::gate_closed_error(prefix)),
                _ => Err(af::lease_expired_error(prefix)),
            }
        })
    }
    pub fn with_open_fence_transaction<T>(&self,fence:&WriteFence,operation:impl FnOnce(&Connection)->Result<T>)->Result<T>{
        self.mirror.with_transfer_lock(fence.prefix.as_ref().trim_end_matches('/'),||{
            let mut conn=self.conn.lock();let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(super::sqlite_error)?;
            require_transfer_epoch(&tx,&fence.prefix,None,fence.epoch,false)?;let value=operation(&tx)?;tx.commit().map_err(super::sqlite_error)?;Ok(value)
        })
    }
    pub(crate) fn with_open_write<T>(&self,fence:&WriteFence,operation:impl FnOnce()->Result<T>)->Result<T>{
        self.mirror.with_transfer_lock(fence.prefix.as_ref().trim_end_matches('/'),||{
            {let conn=self.conn.lock();require_transfer_epoch(&conn,&fence.prefix,None,fence.epoch,false)?;}
            // Every native gate transition takes this same OS lock before its
            // SQL transaction, so no close/reopen can race the inner DB commit.
            operation()
        })
    }
    pub(crate) fn with_closed_write<T>(&self,prefix:&ObjectPrefix,owner:&str,epoch:u64,operation:impl FnOnce()->Result<T>)->Result<T>{self.mirror.with_transfer_lock(prefix.as_ref().trim_end_matches('/'),||{{let conn=self.conn.lock();require_transfer_epoch(&conn,prefix,Some(owner),epoch,true)?;}operation()})}
    pub(crate) fn commit_remote_reference(&self,prefix:&ObjectPrefix,owner:Option<&str>,epoch:u64,key:&ObjectKey,data:Option<&[u8]>,expected_sha:Option<&str>)->Result<()> {
        self.mirror.with_transfer_lock(prefix.as_ref().trim_end_matches('/'),||{
            let mut conn=self.conn.lock();let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(super::sqlite_error)?;require_transfer_epoch(&tx,prefix,owner,epoch,owner.is_some())?;
            if let Some(expected)=expected_sha{let Some(payload)=read_stored_payload(&tx,key,Some(64*1024))?else{return Ok(());};let bytes=decode_stored_payload(payload,key,Some(64*1024))?;let row:serde_json::Value=serde_json::from_slice(&bytes).map_err(|error|NarouError::Platform(error.to_string()))?;if !row.get("sha256").and_then(serde_json::Value::as_str).is_some_and(|sha|sha.eq_ignore_ascii_case(expected)){return Err(NarouError::Platform("recovery reference fingerprint conflict".into()));}}
            if let Some(data)=data{insert_stored_object(&tx,StoredObject{key,size:data.len() as i64,crc32:crate::platform::object_crc32(data) as i64,stored:data,encoding:ObjectEncoding::None,content_type:Some("application/json"),updated_at:&Self::now_rfc3339()})?;}else{tx.execute("DELETE FROM objects WHERE object_key=?",[key.as_ref()]).map_err(super::sqlite_error)?;}
            tx.execute("UPDATE novel_access_gates SET content_revision=content_revision+1 WHERE prefix=?",[prefix.as_ref().trim_end_matches('/')]).map_err(super::sqlite_error)?;
            tx.commit().map_err(super::sqlite_error)
        })
    }
    fn gate_transaction<T>(&self,prefix:&ObjectPrefix,operation:impl FnOnce(&Connection)->Result<T>)->Result<T> {
        self.gate_transaction_mode(prefix,false,operation)
    }
    fn reader_gate_transaction<T>(&self,prefix:&ObjectPrefix,operation:impl FnOnce(&Connection)->Result<T>)->Result<T> {
        self.gate_transaction_mode(prefix,true,operation)
    }
    fn gate_transaction_mode<T>(&self,prefix:&ObjectPrefix,reader:bool,operation:impl FnOnce(&Connection)->Result<T>)->Result<T> {
        let run=||{
            let mut conn=self.conn.lock();let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(super::sqlite_error)?;
            // A committed SQL→FS handoff retires every SQL token for this
            // prefix: creating a default epoch-0 row must never revive SQL
            // authority against the live FS gate.
            if crate::native::object_store::read_fs_gate_handoff_raw(&tx,prefix.as_ref().trim_end_matches('/'))?.is_some(){return Err(af::gate_closed_error(prefix));}
            tx.execute("INSERT OR IGNORE INTO novel_access_gates(prefix,updated_at) VALUES (?,?)",rusqlite::params![prefix.as_ref().trim_end_matches('/'),ts::now_secs()]).map_err(super::sqlite_error)?;
            let value=operation(&tx)?;tx.commit().map_err(super::sqlite_error)?;Ok(value)
        };
        if reader { self.mirror.with_reader_gate_lock(prefix.as_ref().trim_end_matches('/'),run) }
        else { self.mirror.with_transfer_lock(prefix.as_ref().trim_end_matches('/'),run) }
    }
    fn gate_info_conn(conn:&Connection,prefix:&ObjectPrefix)->Result<GateInfo> {
        let (epoch,closed,owner,updated_at,content_revision):(u64,i64,Option<String>,i64,u64)=conn.query_row("SELECT epoch,closed,owner,updated_at,content_revision FROM novel_access_gates WHERE prefix=?",[prefix.as_ref().trim_end_matches('/')],|row|Ok((super::read_u64(row,0)?,row.get(1)?,row.get(2)?,row.get(3)?,super::read_u64(row,4)?))).map_err(super::sqlite_error)?;
        let count:u32=conn.query_row("SELECT count(*) FROM novel_reader_leases WHERE prefix=? AND expires_at>?",rusqlite::params![prefix.as_ref().trim_end_matches('/'),ts::now_secs()],|row|row.get(0)).map_err(super::sqlite_error)?;
        Ok(GateInfo{epoch,content_revision,state:if closed==0{GateState::Open}else{GateState::Closed},pending_readers:count,owner,updated_at})
    }

    /// SQL→FS gate authority adoption, in-transaction half. Caller's
    /// Immediate transaction must be the metadata boundary for `prefix`, and
    /// the caller must hold the prefix's OS transfer lock. Reads the gate row
    /// inside `tx`, fails closed on a closed gate, verifies or records the
    /// durable `fs_gate_handoffs` intent (a committed-but-interrupted adoption
    /// resumes on its recorded fs_epoch, never a fresh epoch), imports every
    /// live reader lease with its original token epoch (stale tokens lose on
    /// renew while the drain contract holds), pre-flights the canonical FS
    /// gate conflict rule, writes the pending journal, then deletes the SQL
    /// gate row + leases so no old SQL token can revive.
    /// Returns `Some(gate)` to finalize with
    /// `finalize_sql_gate_adoption_locked` after the caller's COMMIT, or
    /// `None` when the handoff is already proven by the canonical FS gate
    /// (idempotent re-run).
    #[cfg(test)]
    pub(crate) fn adopt_sql_gate_in_tx(&self,tx:&Connection,prefix:&ObjectPrefix)->Result<Option<FsGate>> {
        adopt_sql_gate_tx(&self.mirror,tx,prefix,true)
    }

    pub(crate) fn adopt_sql_gate_in_tx_nosync(&self,tx:&Connection,prefix:&ObjectPrefix)->Result<Option<FsGate>> {
        adopt_sql_gate_tx(&self.mirror,tx,prefix,false)
    }

    /// Post-COMMIT half of SQL→FS adoption: installs the canonical FS gate
    /// (lineage re-validated against the now-committed handoff) and retires
    /// the pending journal. `connection` must be the connection the intent
    /// committed on; the caller still holds the prefix's OS transfer lock.
    #[cfg(test)]
    pub(crate) fn finalize_sql_gate_adoption_locked(&self,prefix:&ObjectPrefix,gate:&FsGate,connection:&Connection)->Result<()> {
        self.mirror.install_adopted_gate_locked(&self.mirror.gate_path(prefix)?,prefix,gate,connection)?;
        self.mirror.remove_pending_gate_locked(prefix)
    }


    /// Batched variant of [`Self::install_adopted_gate_nosync`] for mass
    /// adoption (export cutover). See the mirror's `install_adopted_gate_batch`.
    pub(crate) fn install_adopted_gate_batch(&self,prefix:&ObjectPrefix,gate:&FsGate,connection:&Connection,batch:&crate::native::object_store::GateRevisionBatch)->Result<()> {
        self.mirror.install_adopted_gate_batch(&self.mirror.gate_path(prefix)?,prefix,gate,connection,batch)
    }


    /// SQL→FS gate authority adoption for `prefix`. Under the prefix's OS
    /// transfer lock + one immediate transaction: verify the row, journal the
    /// intent (`fs_gate_handoffs` + `.gate.pending`), delete the SQL row and
    /// leases, commit, then install the canonical FS gate and retire the
    /// journal. A crash anywhere before the journal+intent commit leaves SQL
    /// authoritative and is rolled back by the startup sweep; a crash after
    /// it leaves a committed intent the same sweep completes. Idempotent: a
    /// re-run sees the row deleted and the handoff covering the live FS gate
    /// and passes.
    #[cfg(test)]
    pub(crate) fn adopt_sql_gate_locked(&self,prefix:&ObjectPrefix)->Result<()>{
        self.mirror.with_transfer_lock(prefix.as_ref().trim_end_matches('/'),||{
            let conn=self.conn.lock();
            let tx=rusqlite::Transaction::new_unchecked(&conn,rusqlite::TransactionBehavior::Immediate).map_err(super::sqlite_error)?;
            let gate=self.adopt_sql_gate_in_tx(&tx,prefix)?;
            tx.commit().map_err(super::sqlite_error)?;
            if let Some(gate)=gate{self.finalize_sql_gate_adoption_locked(prefix,&gate,&conn)?;}
            Ok(())
        })
    }
}

/// The in-transaction core shared by `SqliteObjectStore::adopt_sql_gate_in_tx`
/// and `adopt_sql_gate_locked` (the free function used by the startup sweep,
/// which holds the connection guard rather than a store).
fn adopt_sql_gate_tx(mirror:&NativeObjectStore,tx:&Connection,prefix:&ObjectPrefix,sync_parent:bool)->Result<Option<FsGate>> {
    let prefix_key=prefix.as_ref().trim_end_matches('/');
    let row:Option<(u64,bool,String,u64)>=tx.query_row("SELECT epoch,closed,origin_url_fold,content_revision FROM novel_access_gates WHERE prefix=?1",[prefix_key],|row|Ok((super::read_u64(row,0)?,row.get(1)?,row.get(2)?,super::read_u64(row,3)?))).optional().map_err(super::sqlite_error)?;
    let recorded=crate::native::object_store::read_fs_gate_handoff_raw(tx,prefix_key)?;
    let Some((sql_epoch,closed,origin,content_revision))=row else{
        // The SQL row is gone: the handoff is authoritative only when the
        // recorded tuple is covered by the canonical FS gate (idempotent
        // re-run) or still carried by the durable pending journal a previous
        // crash left to finalize.
        let Some((_,fs_epoch,fs_origin))=recorded else{return Err(NarouError::Platform("FS gate adoption lost its durable handoff".into()));};
        match crate::native::object_store::read_durable_gate(&mirror.gate_path(prefix)?)? {
            Some(gate)=>{
                if gate.epoch!=fs_epoch||gate.origin_url_fold!=fs_origin{return Err(NarouError::Platform("FS gate adoption proof mismatch".into()));}
                if gate.migrated{return Err(NarouError::Platform("migrated FS gate lost its SQL handoff".into()));}
                if sync_parent{mirror.remove_pending_gate_locked(prefix)?;}else{mirror.remove_pending_gate_nosync(prefix)?;}
                return Ok(None);
            }
            None=>{
                let Some(gate)=mirror.read_pending_gate_locked(prefix)? else{return Err(NarouError::Platform("FS gate adoption lost its durable journal".into()));};
                if gate.epoch!=fs_epoch||gate.origin_url_fold!=fs_origin{return Err(NarouError::Platform("FS gate journal proof mismatch".into()));}
                return Ok(Some(gate));
            }
        }
    };
    // A committed intent tuple must match the row it was journaled from;
    // anything else is conflicting provenance and fails closed.
    if let Some((recorded_sql,recorded_fs,recorded_origin))=&recorded&&(*recorded_sql!=sql_epoch||*recorded_fs!=sql_epoch.saturating_add(1)||*recorded_origin!=origin){
        return Err(NarouError::Platform("conflicting durable FS gate handoff".into()));
    }
    if closed{return Err(NarouError::Platform("closed SQL gate cannot migrate to filesystem authority".into()));}
    let fs_epoch=recorded.as_ref().map(|(_,fs_epoch,_)|*fs_epoch).map_or_else(||sql_epoch.checked_add(1),Some).ok_or_else(||NarouError::Platform("SQL gate epoch exhausted".into()))?;
    let mut readers=std::collections::BTreeMap::new();
    {let mut statement=tx.prepare("SELECT reader_id,epoch,expires_at FROM novel_reader_leases WHERE prefix=?1 AND expires_at>?2").map_err(super::sqlite_error)?;let mut rows=statement.query(rusqlite::params![prefix_key,ts::now_secs()]).map_err(super::sqlite_error)?;while let Some(row)=rows.next().map_err(super::sqlite_error)?{let id:String=row.get(0).map_err(super::sqlite_error)?;let epoch=super::read_u64(row,1).map_err(super::sqlite_error)?;let expiry:i64=row.get(2).map_err(super::sqlite_error)?;readers.insert(id,(epoch,expiry));}}
    let gate=FsGate{prefix:Some(prefix_key.to_owned()),origin_url_fold:origin.clone(),content_revision,epoch:fs_epoch,owner:None,updated_at:ts::now_secs(),migrated:false,readers};
    mirror.require_adopted_gate_target(&mirror.gate_path(prefix)?,prefix,&gate,tx)?;
    if recorded.is_none(){
        crate::native::object_store::record_fs_gate_handoff(tx,prefix_key,sql_epoch,fs_epoch,&origin)?;
    }
    if sync_parent{mirror.install_pending_gate_locked(prefix,&gate)?;}else{mirror.install_pending_gate_nosync(prefix,&gate)?;}
    tx.execute("DELETE FROM novel_reader_leases WHERE prefix=?1",[prefix_key]).map_err(super::sqlite_error)?;
    tx.execute("DELETE FROM novel_access_gates WHERE prefix=?1",[prefix_key]).map_err(super::sqlite_error)?;
    Ok(Some(gate))
}

/// SQL→FS adoption for callers holding only the connection (startup sweep):
/// identical to `SqliteObjectStore::adopt_sql_gate_locked` — the prefix's OS
/// transfer lock wraps intent commit + canonical finalize.
pub(crate) fn adopt_sql_gate_locked(mirror:&NativeObjectStore,connection:&Connection,prefix:&ObjectPrefix)->Result<()>{
    mirror.with_transfer_lock(prefix.as_ref().trim_end_matches('/'),||{
        let tx=rusqlite::Transaction::new_unchecked(connection,rusqlite::TransactionBehavior::Immediate).map_err(super::sqlite_error)?;
        let gate=adopt_sql_gate_tx(mirror,&tx,prefix,true)?;
        tx.commit().map_err(super::sqlite_error)?;
        if let Some(gate)=gate{
            mirror.install_adopted_gate_locked(&mirror.gate_path(prefix)?,prefix,&gate,connection)?;
            mirror.remove_pending_gate_locked(prefix)?;
        }
        Ok(())
    })
}

/// Resume or roll back every durable SQL→FS adoption intent. Used by both
/// the Sqlite-mode startup sweep and restart recovery after an interrupted
/// management-backend switch: a recorded handoff means the deletion
/// transaction committed, so the adoption is finalized from its journal; an
/// unrecorded journal means the commit never landed and the intent is
/// discarded so its SQL row stays the single authority.
pub(crate) fn reconcile_pending_gate_journals(mirror:&NativeObjectStore,connection:&Arc<Mutex<Connection>>)->Result<()> {
    // Phase 1: pending journals. A recorded handoff resumes the committed
    // adoption (the journal is finalized or the row deleted); an unproven
    // journal is rolled back so its SQL row stays the single authority.
    for (prefix_key,_gate) in crate::native::object_store::durable_fs_gates(&mirror.root().join(".objects/.narou-transfer/.gate.pending"))? {
        let prefix=ObjectPrefix::new(prefix_key.clone())?;
        let recorded={let conn=connection.lock();crate::native::object_store::read_fs_gate_handoff_raw(&conn,&prefix_key)?};
        if recorded.is_none(){
            mirror.with_transfer_lock(&prefix_key,||mirror.remove_pending_gate_locked(&prefix))?;
            continue;
        }
        adopt_sql_gate_locked(mirror,&connection.lock(),&prefix)
            .map_err(|error|NarouError::Platform(format!("pending FS gate adoption failed for {prefix_key}: {error}")))?;
    }
    // Phase 2: recorded intents the journal scan cannot see (crash before
    // the journal write). A live SQL row resumes the adoption; a deleted row
    // finalizes whatever the journal carried.
    {let conn=connection.lock();
        let table:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='fs_gate_handoffs')",[],|row|row.get(0)).map_err(super::sqlite_error)?;
        if table{let mut statement=conn.prepare("SELECT prefix FROM fs_gate_handoffs").map_err(super::sqlite_error)?;let mut rows=statement.query([]).map_err(super::sqlite_error)?;let mut prefixes=Vec::new();while let Some(row)=rows.next().map_err(super::sqlite_error)?{prefixes.push(row.get::<_,String>(0).map_err(super::sqlite_error)?);}drop(rows);drop(statement);drop(conn);
            for prefix_key in prefixes{let prefix=ObjectPrefix::new(prefix_key.clone())?;
                adopt_sql_gate_locked(mirror,&connection.lock(),&prefix)
                    .map_err(|error|NarouError::Platform(format!("FS gate handoff resume failed for {prefix_key}: {error}")))?;}}
    }
    Ok(())
}

/// Delete `.gate.pending` journals that have no committed handoff behind
/// them. Used by restart recovery when the switch transaction provably
/// never committed (or the database itself is gone); a journal WITH a
/// recorded handoff is a conflict and fails closed instead.
pub(crate) fn rollback_unproven_pending_journals(mirror:&NativeObjectStore,connection:Option<&Connection>)->Result<()> {
    for (prefix_key,_gate) in crate::native::object_store::durable_fs_gates(&mirror.root().join(".objects/.narou-transfer/.gate.pending"))? {
        let recorded=match connection{
            Some(conn)=>crate::native::object_store::read_fs_gate_handoff_raw(conn,&prefix_key)?,
            None=>None,
        };
        if recorded.is_some(){return Err(NarouError::Platform(format!("pending FS gate journal has committed provenance for {prefix_key}")));}
        let prefix=ObjectPrefix::new(prefix_key.clone())?;
        mirror.with_transfer_lock(&prefix_key,||mirror.remove_pending_gate_locked(&prefix))?;
    }
    Ok(())
}

/// Startup authority recovery: resolve every committed-but-interrupted
/// SQL→FS handoff, roll back orphaned journals, reconcile each durable FS
/// gate file, then adopt live FS authority into SQL. Every classification
/// uses exact epoch+origin tuples; conflicting provenance fails closed.
pub(crate) fn reconcile_namespace_gate_authority(mirror:&NativeObjectStore,connection:&Arc<Mutex<Connection>>)->Result<()> {
    reconcile_pending_gate_journals(mirror,connection)?;
    // Phase 3: classify every remaining durable FS gate file. Exact
    // epoch+origin tuples decide; every ambiguous combination fails closed.
    for (prefix_key,mut gate) in crate::native::object_store::durable_fs_gates(&mirror.root().join(".objects/.narou-transfer/.gate"))? {
        let prefix=ObjectPrefix::new(prefix_key.clone())?;
        // Read-only classification under the conn guard; actions below take
        // their own guards so the OS transfer lock is never sought while a
        // long-lived connection borrow is held.
        let(proven_sql,fs_handoff,row)={
            let conn=connection.lock();
            (crate::native::object_store::read_sql_gate_handoff(&conn,&prefix_key,gate.epoch,&gate.origin_url_fold)?,
             crate::native::object_store::read_fs_gate_handoff_raw(&conn,&prefix_key)?,
             conn.query_row("SELECT epoch,origin_url_fold FROM novel_access_gates WHERE prefix=?1",[&prefix_key],|row|Ok((super::read_u64(row,0)?,row.get::<_,String>(1)?))).optional().map_err(super::sqlite_error)?)
        };
        let fs_owned=fs_handoff.as_ref().is_some_and(|(_,fs_epoch,fs_origin)|*fs_epoch==gate.epoch&&fs_origin==&gate.origin_url_fold);
        if fs_handoff.is_some()&&!fs_owned{return Err(NarouError::Platform(format!("conflicting durable FS gate handoff for {prefix_key}")));}
        if gate.migrated{
            if fs_owned{return Err(NarouError::Platform(format!("SQL and filesystem handoffs conflict for {prefix_key}")));}
            // A proven FS→SQL handoff retires this file: SQL row is canonical.
            if proven_sql{continue;}
            // The FS→SQL commit never landed: the file is the only authority
            // again — clear the marker and adopt afresh below.
            mirror.clear_unproven_migrated_gate_locked(&prefix,&connection.lock())?;
            gate.migrated=false;
        }
        if let Some((sql_epoch,origin))=row {
            if gate.migrated{return Err(NarouError::Platform("migrated FS gate lost its SQL handoff".into()));}
            if proven_sql{
                // SQL→FS commit landed but crashed before the migrated
                // marker: the file was already superseded — record it so
                // FS-side checks fail closed from now on.
                mirror.mark_proven_migrated_gate_locked(&prefix)?;
                continue;
            }
            if fs_owned{return Err(NarouError::Platform(format!("FS gate adoption resume missing for {prefix_key}")));}
            if sql_epoch.checked_add(1)==Some(gate.epoch)&&origin==gate.origin_url_fold{
                return Err(NarouError::Platform(format!("SQL and filesystem gates conflict for {prefix_key}")));
            }
            return Err(NarouError::Platform(format!("SQL gate conflicts with filesystem authority for {prefix_key}")));
        }
        if fs_owned{
            // An FS gate a proven SQL→FS handoff produced stays FS-side; it
            // already carried this exact epoch+origin back from SQL. A stale
            // migrated marker was cleared above if needed.
            continue;
        }
        if gate.migrated{return Err(NarouError::Platform("migrated FS gate lost its SQL handoff".into()));}
        // Pristine defaults (stray releases created the file without ever
        // holding authority) carry nothing worth adopting.
        if gate.epoch == 0 && gate.owner.is_none() && gate.origin_url_fold.is_empty() && gate.readers.is_empty() && gate.content_revision == 0 { continue; }
        if gate.owner.is_some() { continue; }
        // Fail closed: a skipped adoption would leave live dual authority
        // (FS gate + this handle) for the same prefix.
        mirror.adopt_fs_gate_locked(&prefix, &connection.lock())
            .map_err(|error| NarouError::Platform(format!("FS gate adoption failed for {prefix_key}: {error}")))?;
    }
    Ok(())
}

impl TransferStageStore for SqliteObjectStore {
    fn stage_open<'a>(&'a self,id:&'a str,owner:&'a str,epoch:u64,ttl:i64)->PlatformFuture<'a,Result<StageCursor>> {
        Box::pin(async move { run_scoped_blocking(||self.stage_transaction(id,|conn,current| {
            let record=match current {Some(record)=>{ts::require_owner(&record,owner,epoch)?;if record.lease.state==ts::StageLeaseState::Aborting{return Err(NarouError::Platform("stage_aborted".into()));}record},None=>{
                let record=ts::StageLedgerRecord::new(id,owner,epoch,ttl)?;
                conn.execute("INSERT INTO objects(object_key,size,updated_at,encoding,crc32,data) VALUES (?,0,?,'none',0,NULL)",rusqlite::params![ts::stage_data_key(id)?.as_ref(),Self::now_rfc3339()]).map_err(super::sqlite_error)?;record}};
            Ok((record.cursor(),Some(record)))
        })) })
    }
    fn stage_cursor<'a>(&'a self,id:&'a str)->PlatformFuture<'a,Result<Option<StageCursor>>> {
        Box::pin(async move{run_scoped_blocking(||self.stage_transaction(id,|conn,record|{if record.as_ref().is_some_and(|record|record.lease.state==ts::StageLeaseState::Aborting){let pinned:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM transfer_object_refs WHERE blob_key=?)",[ts::stage_data_key(id)?.as_ref()],|row|row.get(0)).map_err(super::sqlite_error)?;if pinned{return Ok((None,None));}}Ok((record.as_ref().map(|record|record.cursor()),None))}))})
    }
    fn stage_append<'a>(&'a self,id:&'a str,owner:&'a str,epoch:u64,offset:u64,payload:&'a [u8],last:bool)->PlatformFuture<'a,Result<StageCursor>> {
        Box::pin(async move{run_scoped_blocking(||self.stage_transaction(id,|conn,current|{
            let mut record=current.ok_or_else(||NarouError::Platform("stage not open".into()))?;record.append(owner,epoch,offset,payload,last)?;let key=ts::stage_data_key(id)?;
            let mut position=offset; let mut input=payload;
            while !input.is_empty(){
                let seq=position/OBJECT_CHUNK_BYTES as u64;let inside=(position%OBJECT_CHUNK_BYTES as u64) as usize;let length=input.len().min(OBJECT_CHUNK_BYTES-inside);
                if inside==0 {conn.execute("INSERT INTO object_chunks(object_key,seq,data) VALUES (?,?,?) ON CONFLICT(object_key,seq) DO UPDATE SET data=excluded.data",rusqlite::params![key.as_ref(),seq as i64,&input[..length]]).map_err(super::sqlite_error)?;}
                else {let changed=conn.execute("UPDATE object_chunks SET data=CAST(substr(data,1,?) || ? AS BLOB) WHERE object_key=? AND seq=? AND length(data)>=?",rusqlite::params![inside as i64,&input[..length],key.as_ref(),seq as i64,inside as i64]).map_err(super::sqlite_error)?;if changed!=1{return Err(NarouError::Platform("stage prefix missing".into()));}}
                position+=length as u64;input=&input[length..];
            }
            self.append_mirror_stage(&key,offset,payload)?;
            conn.execute("UPDATE objects SET size=?,crc32=?,updated_at=? WHERE object_key=?",rusqlite::params![record.confirmed_bytes as i64,record.crc32,Self::now_rfc3339(),key.as_ref()]).map_err(super::sqlite_error)?;
            Ok((record.cursor(),Some(record)))
        }))})
    }
    fn stage_rewind<'a>(&'a self,id:&'a str,owner:&'a str,epoch:u64,checkpoint:&'a StageCursor)->PlatformFuture<'a,Result<StageCursor>>{
        Box::pin(async move{run_scoped_blocking(||self.stage_transaction(id,|conn,current|{
            let mut record=current.ok_or_else(||NarouError::Platform("stage not open".into()))?;
            let trusted=if checkpoint.confirmed_bytes==0{ts::StageLedgerRecord::new(id,owner,epoch,1)?.cursor()}else{
                let key=ts::stage_checkpoint_key(id,checkpoint.confirmed_bytes)?;let payload=read_stored_payload(conn,&key,Some(64*1024))?.ok_or_else(||NarouError::Platform("rewind checkpoint missing".into()))?;let bytes=decode_stored_payload(payload,&key,Some(64*1024))?;let stored:StageCursor=serde_json::from_slice(&bytes).map_err(|error|NarouError::Platform(error.to_string()))?;if stored.prefix_sha256()!=checkpoint.prefix_sha256(){return Err(NarouError::Platform("rewind SHA conflict".into()));}stored};
            record.rewind(owner,epoch,&trusted)?;let key=ts::stage_data_key(id)?;let size=checkpoint.confirmed_bytes;
            conn.execute("DELETE FROM object_chunks WHERE object_key=? AND seq>=?",rusqlite::params![key.as_ref(),size.div_ceil(OBJECT_CHUNK_BYTES as u64) as i64]).map_err(super::sqlite_error)?;
            if !size.is_multiple_of(OBJECT_CHUNK_BYTES as u64) {conn.execute("UPDATE object_chunks SET data=substr(data,1,?) WHERE object_key=? AND seq=?",rusqlite::params![(size%OBJECT_CHUNK_BYTES as u64) as i64,key.as_ref(),(size/OBJECT_CHUNK_BYTES as u64) as i64]).map_err(super::sqlite_error)?;}
            if self.mirror_files{let path=self.mirror.resolve_path(&key)?;match std::fs::OpenOptions::new().write(true).open(path){Ok(file)=>{file.set_len(size)?;file.sync_all()?;},Err(error)if error.kind()==std::io::ErrorKind::NotFound&&size==0=>{},Err(error)=>return Err(error.into())}}
            conn.execute("UPDATE objects SET size=?,crc32=? WHERE object_key=?",rusqlite::params![size as i64,record.crc32,key.as_ref()]).map_err(super::sqlite_error)?;Ok((record.cursor(),Some(record)))
        }))})
    }
    fn stage_read<'a>(&'a self,id:&'a str,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<AssetStream>>>{Box::pin(async move{
        run_scoped_blocking(||{let conn=self.conn.lock();let Some(record)=stage_row(&conn,id)?.filter(|record|record.lease.state==ts::StageLeaseState::Sealed)else{return Ok(None);};let key=ts::stage_data_key(id)?;
            let proof:Option<(u64,String)>=conn.query_row("SELECT v.size,v.sha256 FROM transfer_verified_objects v JOIN objects o ON o.object_key=v.blob_key AND o.generation=v.generation WHERE v.blob_key=?",[key.as_ref()],|row|Ok((super::read_u64(row,0)?,row.get(1)?))).optional().map_err(super::sqlite_error)?;
            if !proof.is_some_and(|(size,sha)|size==record.confirmed_bytes&&record.cursor().prefix_sha256().is_some_and(|expected|expected.eq_ignore_ascii_case(&sha))){return Err(NarouError::Platform("sealed stage generation/SHA proof missing or changed".into()));}Ok(self.open_asset_in_connection(&conn,&key,range)?.map(|(_,stream)|stream))
        })
    })}
    fn stage_touch<'a>(&'a self,id:&'a str,owner:&'a str,epoch:u64,ttl:i64)->PlatformFuture<'a,Result<()>>{Box::pin(async move{run_scoped_blocking(||self.stage_transaction(id,|_,current|{
        let mut record=current.ok_or_else(||NarouError::Platform("stage not open".into()))?;ts::require_owner(&record,owner,epoch)?;if record.lease.state==ts::StageLeaseState::Aborting{return Err(NarouError::Platform("stage_aborted".into()));}
        record.lease.expires_at=ts::now_secs().saturating_add(ttl.max(1));Ok(((),Some(record)))
    }))})}
    fn stage_seal<'a>(&'a self,id:&'a str,owner:&'a str,epoch:u64,size:u64,sha:&'a str)->PlatformFuture<'a,Result<SealedAsset>>{Box::pin(async move{run_scoped_blocking(||self.stage_transaction(id,|conn,current|{
        let mut record=current.ok_or_else(||NarouError::Platform("stage not open".into()))?;let sealed=record.verify_seal(owner,epoch,size,sha)?;
        Self::record_verified_object(conn,&sealed.key,size,sha)?;
        if self.mirror_files{self.mirror.save_verified_file(&sealed.key,size,sha)?;}record.lease.state=ts::StageLeaseState::Sealed;Ok((sealed,Some(record)))
    }))})}
    fn stage_abort<'a>(&'a self,id:&'a str,owner:&'a str,epoch:u64)->PlatformFuture<'a,Result<()>>{Box::pin(async move{run_scoped_blocking(||self.stage_transaction(id,|conn,current|{
        let Some(mut record)=current else{return Ok(((),None));};ts::require_owner(&record,owner,epoch)?;
        let key=ts::stage_data_key(id)?;let referenced:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM transfer_object_refs WHERE blob_key=?)",[key.as_ref()],|row|row.get(0)).map_err(super::sqlite_error)?;
        if !referenced{self.remove_file_copy(&key)?;conn.execute("DELETE FROM object_chunks WHERE object_key=?1 AND seq IN (SELECT seq FROM object_chunks WHERE object_key=?1 ORDER BY seq LIMIT 16)",[key.as_ref()]).map_err(super::sqlite_error)?;conn.execute("DELETE FROM objects WHERE object_key=?1 AND NOT EXISTS(SELECT 1 FROM object_chunks WHERE object_key=?1)",[key.as_ref()]).map_err(super::sqlite_error)?;}record.lease.state=ts::StageLeaseState::Aborting;
        let lower=format!(".narou-transfer/.checkpoints/{id}/");let upper=prefix_upper_bound(&lower);
        conn.execute("DELETE FROM objects WHERE object_key IN (SELECT object_key FROM objects WHERE object_key>=? AND object_key<? ORDER BY object_key LIMIT 32)",rusqlite::params![lower,upper]).map_err(super::sqlite_error)?;
        // Canonical references own a promoted payload; only its stage control
        // and checkpoints belong to the terminal transfer.
        let more:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM objects WHERE object_key>=?1 AND object_key<?2) OR (?4=0 AND EXISTS(SELECT 1 FROM objects WHERE object_key=?3))",rusqlite::params![lower,upper,key.as_ref(),referenced],|row|row.get(0)).map_err(super::sqlite_error)?;
        if more{Ok(((),Some(record)))}else{conn.execute("DELETE FROM transfer_stage_ledger WHERE stage_id=?",[id]).map_err(super::sqlite_error)?;Ok(((),None))}
    }))})}
    fn stage_lease_page<'a>(&'a self,cursor:Option<&'a str>,limit:usize)->PlatformFuture<'a,Result<(Vec<StageLease>,Option<String>)>>{Box::pin(async move{run_scoped_blocking(||{
        let conn=self.conn.lock();let mut statement=conn.prepare("SELECT stage_id,record_json FROM transfer_stage_ledger WHERE stage_id>? ORDER BY stage_id LIMIT ?").map_err(super::sqlite_error)?;let cap=limit.clamp(1,100);
        let mut rows=statement.query(rusqlite::params![cursor.unwrap_or(""),(cap+1) as i64]).map_err(super::sqlite_error)?;let mut out=Vec::new();let mut last=None;
        while let Some(row)=rows.next().map_err(super::sqlite_error)?{if out.len()==cap{return Ok((out,last));}let id:String=row.get(0).map_err(super::sqlite_error)?;let json:String=row.get(1).map_err(super::sqlite_error)?;out.push(ts::decode_ledger(json.as_bytes())?.lease);last=Some(id);}Ok((out,None))
    })})}
    fn stage_reclaim<'a>(&'a self,id:&'a str,owner:&'a str,epoch:u64)->PlatformFuture<'a,Result<bool>>{Box::pin(async move{
        let claimed=run_scoped_blocking(||self.stage_transaction(id,|_,current|{
            let Some(mut record)=current else{return Ok((true,None));};if record.lease.owner!=owner||record.lease.epoch!=epoch||(record.lease.state!=ts::StageLeaseState::Aborting&&record.lease.expires_at>ts::now_secs()){return Ok((false,None));}
            record.lease.state=ts::StageLeaseState::Aborting;Ok((true,Some(record)))
        }))?;if !claimed{return Ok(false);}self.stage_abort(id,owner,epoch).await?;Ok(self.stage_cursor(id).await?.is_none())
    })}
}

impl NovelAccessFence for SqliteObjectStore {
    fn retire_writer<'a>(&'a self,fence:&'a WriteFence,owner:&'a str)->PlatformFuture<'a,Result<u64>>{Box::pin(async move{af::parse_removal_owner(owner)?;let next=fence.epoch.checked_add(1).filter(|value|*value<=i64::MAX as u64).ok_or_else(||NarouError::Platform("gate epoch overflow".into()))?;run_scoped_blocking(||self.gate_transaction(&fence.prefix,|conn|{
        let info=Self::gate_info_conn(conn,&fence.prefix)?;if info.state==GateState::Closed&&info.owner.as_deref()==Some(owner)&&info.epoch==next{return Ok(next);}require_transfer_epoch(conn,&fence.prefix,None,fence.epoch,false)?;
        let changed=conn.execute("UPDATE novel_access_gates SET closed=1,owner=?,epoch=?,updated_at=? WHERE prefix=? AND closed=0 AND epoch=?",rusqlite::params![owner,next as i64,ts::now_secs(),fence.prefix.as_ref().trim_end_matches('/'),i64::try_from(fence.epoch).map_err(|_|af::gate_epoch_error(&fence.prefix,fence.epoch,u64::MAX))?]).map_err(super::sqlite_error)?;if changed!=1{return Err(af::gate_closed_error(&fence.prefix));}Ok(next)
    }))})}
    fn pending_removals<'a>(&'a self,cursor:Option<&'a str>,limit:usize)->PlatformFuture<'a,Result<(Vec<(ObjectPrefix,String,u64)>,Option<String>)>>{Box::pin(async move{run_scoped_blocking(||{
        let cap=limit.clamp(1,100);let conn=self.conn.lock();let mut statement=conn.prepare("SELECT prefix,owner,epoch,closed FROM novel_access_gates WHERE prefix>? ORDER BY prefix LIMIT ?").map_err(super::sqlite_error)?;let mut rows=statement.query(rusqlite::params![cursor.unwrap_or(""),(cap+1) as i64]).map_err(super::sqlite_error)?;let mut out=Vec::new();let mut last=None;let mut scanned=0;
        while let Some(row)=rows.next().map_err(super::sqlite_error)?{if scanned==cap{return Ok((out,last));}scanned+=1;let prefix:String=row.get(0).map_err(super::sqlite_error)?;let owner:Option<String>=row.get(1).map_err(super::sqlite_error)?;let epoch=super::read_u64(row,2).map_err(super::sqlite_error)?;let closed:bool=row.get(3).map_err(super::sqlite_error)?;last=Some(prefix.clone());if closed&&let Some(owner)=owner.filter(|owner|owner.starts_with("remove-v1:")){af::parse_removal_owner(&owner)?;out.push((ObjectPrefix::new(prefix)?,owner,epoch));}}
        Ok((out,None))
    })})}
    fn rebind_writer<'a>(&'a self,prefix:&'a ObjectPrefix,expected_epoch:u64)->PlatformFuture<'a,Result<WriteFence>>{Box::pin(async move{run_scoped_blocking(||self.gate_transaction(prefix,|conn|{require_transfer_epoch(conn,prefix,None,expected_epoch,false)?;Ok(WriteFence::new(prefix.clone(),expected_epoch))}))})}
    fn gate_info<'a>(&'a self,prefix:&'a ObjectPrefix)->PlatformFuture<'a,Result<GateInfo>>{Box::pin(async move{run_scoped_blocking(||self.reader_gate_transaction(prefix,|conn|Self::gate_info_conn(conn,prefix)))})}
    fn admit_reader<'a>(&'a self,prefix:&'a ObjectPrefix,id:&'a str,ttl:i64)->PlatformFuture<'a,Result<GateLease>>{Box::pin(async move{run_scoped_blocking(||self.reader_gate_transaction(prefix,|conn|{
        // Sweep this prefix's expired rows in the same admission transaction
        // so a busy prefix cannot accumulate dead leases (matches the FS
        // gate's `prune` on every gate operation).
        let info=Self::gate_info_conn(conn,prefix)?;if info.state!=GateState::Open{return Err(af::gate_closed_error(prefix));}let expires=ts::now_secs().saturating_add(ttl.clamp(1,af::READER_LEASE_SECS));
        conn.execute("DELETE FROM novel_reader_leases WHERE prefix=? AND expires_at<=?",rusqlite::params![prefix.as_ref().trim_end_matches('/'),ts::now_secs()]).map_err(super::sqlite_error)?;
        conn.execute("INSERT INTO novel_reader_leases(prefix,reader_id,epoch,expires_at) VALUES (?,?,?,?) ON CONFLICT(prefix,reader_id) DO UPDATE SET epoch=excluded.epoch,expires_at=excluded.expires_at",rusqlite::params![prefix.as_ref().trim_end_matches('/'),id,i64::try_from(info.epoch).map_err(|_|NarouError::Platform("gate epoch exceeds SQLite integer range".into()))?,expires]).map_err(super::sqlite_error)?;
        Ok(GateLease{reader_id:id.into(),epoch:info.epoch,expires_at:expires})
    }))})}
    fn renew_reader<'a>(&'a self,prefix:&'a ObjectPrefix,lease:&'a GateLease,ttl:i64)->PlatformFuture<'a,Result<GateLease>>{Box::pin(async move{run_scoped_blocking(||self.reader_gate_transaction(prefix,|conn|{
        let info=Self::gate_info_conn(conn,prefix)?;if info.state!=GateState::Open||info.epoch!=lease.epoch{return Err(af::gate_closed_error(prefix));}let now=ts::now_secs();let expires=now.saturating_add(ttl.clamp(1,af::READER_LEASE_SECS));
        let epoch_i64=i64::try_from(lease.epoch).map_err(|_|af::gate_epoch_error(prefix,lease.epoch,u64::MAX))?;
        let changed=conn.execute("UPDATE novel_reader_leases SET expires_at=? WHERE prefix=? AND reader_id=? AND epoch=? AND expires_at>?",rusqlite::params![expires,prefix.as_ref().trim_end_matches('/'),lease.reader_id,epoch_i64,now]).map_err(super::sqlite_error)?;
        if changed!=1{
            // No live row was updated. Classify: a same-id row at a different
            // epoch is a stale token (gate boundary), while a missing or
            // expired row at this epoch is a dead admission.
            let row:Option<(i64,i64)>=conn.query_row("SELECT epoch,expires_at FROM novel_reader_leases WHERE prefix=? AND reader_id=?",rusqlite::params![prefix.as_ref().trim_end_matches('/'),lease.reader_id],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(super::sqlite_error)?;
            match row {
                Some((row_epoch,_)) if row_epoch!=epoch_i64 => return Err(af::gate_closed_error(prefix)),
                _ => return Err(af::lease_expired_error(prefix)),
            }
        }
        Ok(GateLease{reader_id:lease.reader_id.clone(),epoch:lease.epoch,expires_at:expires})
    }))})}
    fn release_reader<'a>(&'a self,prefix:&'a ObjectPrefix,lease:&'a GateLease)->PlatformFuture<'a,Result<()>>{Box::pin(async move{run_scoped_blocking(||{
        let epoch=i64::try_from(lease.epoch).map_err(|_|af::gate_epoch_error(prefix,lease.epoch,u64::MAX))?;
        // Original-lease release drains the authority that still knows the
        // token: the SQL row AND a durable FS gate entry an SQL→FS adoption
        // imported — both under the prefix's OS transfer lock, so a release
        // racing an adoption can never leave a live lease on the other side.
        // The delete qualifies the exact (reader_id, epoch) the token was
        // admitted at: drain stays legal after close advanced the gate
        // epoch, a stale token cannot touch a same-reader-id lease
        // re-admitted at a newer epoch, and a missing/already-released row
        // is idempotent. The second clause sweeps this prefix's expired rows
        // so a release also reclaims dead admissions for free.
        self.mirror.with_reader_gate_lock(prefix.as_ref().trim_end_matches('/'),||{
            self.conn.lock().execute("DELETE FROM novel_reader_leases WHERE prefix=? AND ((reader_id=? AND epoch=?) OR expires_at<=?)",rusqlite::params![prefix.as_ref().trim_end_matches('/'),lease.reader_id,epoch,ts::now_secs()]).map_err(super::sqlite_error)?;
            self.mirror.release_reader_locked(prefix,lease)
        })
    })})}
    fn close_gate<'a>(&'a self,prefix:&'a ObjectPrefix,owner:&'a str)->PlatformFuture<'a,Result<u64>>{Box::pin(async move{run_scoped_blocking(||self.gate_transaction(prefix,|conn|{
        let info=Self::gate_info_conn(conn,prefix)?;if info.state==GateState::Closed{if info.owner.as_deref()==Some(owner){return Ok(info.epoch);}return Err(af::gate_owner_error(prefix,info.owner.as_deref().unwrap_or("")));}
        let next=info.epoch.checked_add(1).filter(|value|*value<=i64::MAX as u64).ok_or_else(||NarouError::Platform("gate epoch overflow".into()))?;
        conn.execute("UPDATE novel_access_gates SET closed=1,owner=?,epoch=?,updated_at=? WHERE prefix=?",rusqlite::params![owner,next as i64,ts::now_secs(),prefix.as_ref().trim_end_matches('/')]).map_err(super::sqlite_error)?;Ok(next)
    }))})}
    fn renew_gate<'a>(&'a self,prefix:&'a ObjectPrefix,owner:&'a str,epoch:u64)->PlatformFuture<'a,Result<()>>{Box::pin(async move{run_scoped_blocking(||self.gate_transaction(prefix,|conn|{
        require_transfer_epoch(conn,prefix,Some(owner),epoch,false)?;conn.execute("UPDATE novel_access_gates SET updated_at=? WHERE prefix=?",rusqlite::params![ts::now_secs(),prefix.as_ref().trim_end_matches('/')]).map_err(super::sqlite_error)?;Ok(())
    }))})}
    fn open_gate<'a>(&'a self,prefix:&'a ObjectPrefix,owner:&'a str,epoch:u64)->PlatformFuture<'a,Result<()>>{Box::pin(async move{run_scoped_blocking(||self.gate_transaction(prefix,|conn|{
        require_transfer_epoch(conn,prefix,Some(owner),epoch,false)?;let clear_origin=owner.starts_with("remove-v1:")&&af::parse_removal_owner(owner)?.1;conn.execute("UPDATE novel_access_gates SET closed=0,owner=NULL,updated_at=?1,origin_url_fold=CASE WHEN ?3 THEN '' ELSE origin_url_fold END WHERE prefix=?2",rusqlite::params![ts::now_secs(),prefix.as_ref().trim_end_matches('/'),clear_origin]).map_err(super::sqlite_error)?;Ok(())
    }))})}
    fn commit_staged<'a>(&'a self,prefix:&'a ObjectPrefix,owner:&'a str,epoch:u64,stage:&'a ObjectKey,key:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        if !prefix.matches(key)||!crate::platform::is_reserved_key(stage){return Err(NarouError::Platform("invalid fenced install namespace".into()));}
        run_scoped_blocking(||self.with_publication_transaction(prefix,owner,epoch,|conn|self.install_reference(conn,stage,key)))
    })}
    fn remove_committed<'a>(&'a self,prefix:&'a ObjectPrefix,owner:&'a str,epoch:u64,key:&'a ObjectKey,sha:&'a str)->PlatformFuture<'a,Result<()>>{Box::pin(async move{run_scoped_blocking(||self.with_publication_transaction(prefix,owner,epoch,|conn|{
        if !prefix.matches(key){return Err(NarouError::Platform("rollback key outside novel".into()));}
        let proof:Option<(String,String,String)>=conn.query_row("SELECT blob_key,blob_generation,sha256 FROM transfer_object_refs WHERE object_key=? AND backend='d1'",[key.as_ref()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional().map_err(super::sqlite_error)?;
        let Some((physical,generation,current))=proof else{let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM objects WHERE object_key=?)",[key.as_ref()],|row|row.get(0)).map_err(super::sqlite_error)?;if exists{return Err(NarouError::Platform("rollback canonical reference proof missing".into()));}return Ok(());};
        if !current.eq_ignore_ascii_case(sha){return Err(NarouError::Platform("recovery fingerprint conflict".into()));}
        let actual:Option<String>=conn.query_row("SELECT generation FROM objects WHERE object_key=?",[&physical],|row|row.get(0)).optional().map_err(super::sqlite_error)?;
        if actual.as_deref()!=Some(generation.as_str()){return Err(NarouError::Platform("recovery generation conflict".into()));}
        if self.mirror_files{self.mirror.remove_verified_file(key,sha)?;}
        conn.execute("DELETE FROM transfer_object_refs WHERE object_key=? AND blob_generation=?",rusqlite::params![key.as_ref(),generation]).map_err(super::sqlite_error)?;Ok(())
    }))})}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::object_store::NativeStore;

    #[test]
    fn transfer_db_append_rewind_seal_is_durable_and_crc_correct(){
        let root=tempfile::tempdir().unwrap();let conn=crate::native::sqlite::open_in_memory().unwrap();
        let store=SqliteObjectStore::with_mirror_files(conn.clone(),NativeObjectStore::from_root(root.path().to_owned()).unwrap(),false);
        futures::executor::block_on(async{
            store.stage_open("durable","owner",9,30).await.unwrap();let first=store.stage_append("durable","owner",9,0,b"abc",false).await.unwrap();
            store.stage_append("durable","owner",9,3,b"tail",true).await.unwrap();
            let reopened=SqliteObjectStore::with_mirror_files(conn,NativeObjectStore::from_root(root.path().to_owned()).unwrap(),false);
            reopened.stage_rewind("durable","owner",9,&StageCursor::from_sha_checkpoint(3,first.hash_state)).await.unwrap();
            reopened.stage_append("durable","owner",9,3,b"def",true).await.unwrap();let sha=hex::encode(sha2::Sha256::digest(b"abcdef"));let sealed=reopened.stage_seal("durable","owner",9,6,&sha).await.unwrap();
            let mut stream=reopened.read_stream(&sealed.key).await.unwrap().unwrap();let mut out=Vec::new();while let Some(chunk)=stream.next().await{out.extend_from_slice(&chunk.unwrap());}assert_eq!(out,b"abcdef");
            assert!(reopened.stage_abort("durable","wrong",9).await.is_err());
        });
    }
    #[test]
    fn promoted_stage_cleanup_finishes_without_losing_shared_canonical_bytes(){
        let base=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-libraries");
        std::fs::create_dir_all(&base).unwrap();
        let root=tempfile::Builder::new().prefix("promoted-stage-gc-").tempdir_in(base).unwrap();
        let conn=crate::native::sqlite::open_in_memory().unwrap();
        let store=SqliteObjectStore::with_mirror_files(conn,NativeObjectStore::from_root(root.path().to_owned()).unwrap(),false);
        futures::executor::block_on(async{
            let bytes=vec![b'z';OBJECT_CHUNK_BYTES+17];
            let sha=hex::encode(sha2::Sha256::digest(&bytes));
            store.stage_open("promoted_gc","session",1,3600).await.unwrap();
            store.stage_append("promoted_gc","session",1,0,&bytes,true).await.unwrap();
            let sealed=store.stage_seal("promoted_gc","session",1,bytes.len() as u64,&sha).await.unwrap();
            let prefix=ObjectPrefix::new("novels/site/book").unwrap();
            let first=ObjectKey::try_new("novels/site/book/first.bin").unwrap();
            let second=ObjectKey::try_new("novels/site/book/second.bin").unwrap();
            let epoch=store.close_gate(&prefix,"publication").await.unwrap();
            store.commit_staged(&prefix,"publication",epoch,&sealed.key,&first).await.unwrap();
            store.commit_staged(&prefix,"publication",epoch,&sealed.key,&second).await.unwrap();
            store.open_gate(&prefix,"publication",epoch).await.unwrap();
            store.stage_abort("promoted_gc","session",1).await.unwrap();
            assert!(store.stage_reclaim("promoted_gc","session",1).await.unwrap(),"published bytes must not keep a terminal transfer advancing forever");
            assert!(store.stage_cursor("promoted_gc").await.unwrap().is_none());
            assert!(store.stage_open("promoted_gc","intruder",2,3600).await.is_err());
            assert_eq!(ObjectStore::read_small(&store,&first).await.unwrap().unwrap(),bytes);
            ObjectStore::delete(&store,&first).await.unwrap();
            assert_eq!(ObjectStore::read_small(&store,&second).await.unwrap().unwrap(),bytes,"another canonical reference still owns the shared payload");
            ObjectStore::write_small(&store,&second,b"replacement".to_vec()).await.unwrap();
            assert_eq!(ObjectStore::read_small(&store,&second).await.unwrap().unwrap(),b"replacement");
            assert!(ObjectStore::read_small(&store,&sealed.key).await.unwrap().is_none(),"the last canonical replacement releases the retired physical payload");
        });
    }
    #[test]
    fn transfer_db_stream_aborts_when_generation_changes(){
        let root=tempfile::tempdir().unwrap();let conn=crate::native::sqlite::open_in_memory().unwrap();let store=SqliteObjectStore::with_mirror_files(conn,NativeObjectStore::from_root(root.path().to_owned()).unwrap(),false);
        futures::executor::block_on(async{
            let key=ObjectKey::try_new("novels/site/book/large.bin").unwrap();let bytes=vec![1;192*1024];store.write_stream(&key,Box::pin(futures::stream::once(async move{Ok(bytes)}))).await.unwrap();
            let mut opened=store.read_stream_range(&key,0..192*1024).await.unwrap().unwrap();assert_eq!(opened.next().await.unwrap().unwrap().len(),64*1024);
            let replacement=vec![2;192*1024];store.write_stream(&key,Box::pin(futures::stream::once(async move{Ok(replacement)}))).await.unwrap();assert!(opened.next().await.unwrap().is_err());
        });
    }
    use crate::platform::{NovelObjectKeys, ObjectPrefix, illustration_key};
    use std::num::NonZeroUsize;

    #[test]
    fn streamed_sqlite_assets_roundtrip_without_a_tokio_runtime() {
        use futures::StreamExt;
        assert!(tokio::runtime::Handle::try_current().is_err());
        for mirror_files in [false, true] {
            let (_dir, mut store) = temp_store();
            store.mirror_files = mirror_files;
            futures::executor::block_on(async {
                let size = crate::platform::staging::ASSET_CHUNK_BYTES;
                let stage = store.stage_stream(Box::pin(futures::stream::iter([
                    Ok(vec![3; size]), Ok(vec![5; 19]),
                ]))).await.unwrap();
                assert_eq!(stage.size, size as u64 + 19);
                let key = ObjectKey::try_new("sync/copied.bin").unwrap();
                AssetStore::copy(&store, &stage.key, &key).await.unwrap();
                let bytes = store.read_range(&key, size as u64 - 3..size as u64 + 8).await.unwrap().unwrap();
                assert_eq!(bytes, [vec![3; 3], vec![5; 8]].concat());
                let mut stream = store.read_stream(&key).await.unwrap().unwrap();
                let mut bytes = Vec::new();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.unwrap();
                    assert!(chunk.len() <= size);
                    bytes.extend_from_slice(&chunk);
                }
                assert_eq!(&bytes[..size], vec![3; size]);
                assert_eq!(&bytes[size..], &[5; 19]);
                AssetStore::delete(&store, &stage.key).await.unwrap();
            });
        }
    }

    #[test]
    fn stage_progress_renews_expiry_without_invalidating_the_read_snapshot() {
        use futures::StreamExt;
        let (_dir, mut store) = temp_store();
        store.mirror_files = false;
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let length = crate::platform::staging::ASSET_CHUNK_BYTES * 2;
            let stage = store.stage_stream(Box::pin(futures::stream::once(async move { Ok(vec![7; length]) }))).await.unwrap();
            let expired = (chrono::Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
            store.conn.lock().execute("UPDATE objects SET updated_at=? WHERE object_key=?",
                rusqlite::params![expired, stage.key.as_ref()]).unwrap();
            store.stage_leases.forget(&stage.key);
            let mut stream = store.read_stream(&stage.key).await.unwrap().unwrap();
            let mut read = stream.next().await.unwrap().unwrap().len();
            store.cleanup_asset_stages().await.unwrap();
            assert!(AssetStore::stat(&store, &stage.key).await.unwrap().is_some());
            while let Some(chunk) = stream.next().await { read += chunk.unwrap().len(); }
            assert_eq!(read, length);
            AssetStore::delete(&store, &stage.key).await.unwrap();
            assert!(store.renew_stage_progress(&stage.key).is_err());
            assert!(AssetStore::stat(&store, &stage.key).await.unwrap().is_none());
        });
    }

    #[test]
    fn streamed_assets_are_chunked_seekable_and_atomic_on_error() {
        let (_dir, mut store) = temp_store();
        store.mirror_files = false;
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let key = ObjectKey::try_new("generated/large.bin").unwrap();
            let input: AssetStream = Box::pin(futures::stream::iter(
                (0..40).map(|index| Ok(vec![index as u8; 64 * 1024]))));
            store.write_stream(&key, input).await.unwrap();
            let range = store.read_range(&key, (512 * 1024 - 8)..(512 * 1024 + 8)).await.unwrap().unwrap();
            assert_eq!(range, [vec![7; 8], vec![8; 8]].concat());
            let failure: AssetStream = Box::pin(futures::stream::iter(vec![
                Ok(vec![99; 64 * 1024]), Err(NarouError::Platform("source interrupted".into())),
            ]));
            assert!(store.write_stream(&key, failure).await.is_err());
            assert_eq!(store.read_range(&key, 0..4).await.unwrap().unwrap(), vec![0; 4]);
            let mut stream = store.read_stream(&key).await.unwrap().unwrap();
            let mut size = 0;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.unwrap();
                assert!(chunk.len() <= 64 * 1024);
                size += chunk.len();
            }
            assert_eq!(size, 40 * 64 * 1024);
        });
    }

    #[test]
    fn streamed_legacy_compression_detects_corruption_at_eof() {
        let (_dir, mut store) = temp_store();
        store.mirror_files = false;
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let key = ObjectKey::try_new("novels/site/title/本文/1 test.yaml").unwrap();
            store.write_small(&key, vec![b'x'; 200_000]).await.unwrap();
            let mut stream = store.read_stream(&key).await.unwrap().unwrap();
            let mut count = 0;
            while let Some(chunk) = stream.next().await { count += chunk.unwrap().len(); }
            assert_eq!(count, 200_000);
            store.conn.lock().execute("UPDATE objects SET crc32=0 WHERE object_key=?", [key.as_ref()]).unwrap();
            let mut stream = store.read_stream(&key).await.unwrap().unwrap();
            let mut failed = false;
            while let Some(chunk) = stream.next().await { if chunk.is_err() { failed = true; break; } }
            assert!(failed, "corruption must not become successful EOF");
        });
    }

    #[test]
    fn db_only_cache_catalog_survives_missing_files_and_corrupt_sections() {
        use crate::application::webui::diff::{plan_diff_cache_clean, read_diff_cache_catalog};
        use crate::platform::NovelObjectKeys;

        let dir = tempfile::tempdir().unwrap();
        let mut conn = Connection::open_in_memory().unwrap();
        crate::native::sqlite::migrations::apply(&mut conn).unwrap();
        let mirror = NativeObjectStore::from_root(dir.path().to_path_buf()).unwrap();
        let store = SqliteObjectStore::with_mirror_files(Arc::new(Mutex::new(conn)), mirror, false);
        let keys = NovelObjectKeys::new("site", "title", false).unwrap();
        let cached = keys.cached_section("v2", "2", "保存済み");
        let section = "index: '2'\nhref: 'https://example.test/2'\nsubtitle: \"保存済み  \\n\"\nfile_subtitle: 保存済み\nsubdate: ''\nelement:\n  body: 旧本文\n";
        futures::executor::block_on(async {
            store.write_small(&cached, section.as_bytes().to_vec()).await.unwrap();
            store.write_small(&keys.cached_version_marker("v2"), Vec::new()).await.unwrap();
            store.write_small(&keys.cached_version_marker("v1"), Vec::new()).await.unwrap();
            store.write_small(&keys.section("2", "現在"), b"current".to_vec()).await.unwrap();
            assert!(!dir.path().join("site/title").exists(), "all cache objects are DB-only");
            let objects:Arc<dyn ObjectStore>=Arc::new(store.clone());
            let mut scope=crate::application::novel_access::NovelReadScope::new(&objects,"cache-catalog-test");
            let versions = read_diff_cache_catalog(&mut scope,&store, keys.prefix(), &[]).await.unwrap();
            assert_eq!(versions.len(), 2);
            assert_eq!(versions[0].entries, vec![("2".to_string(), "保存済み".to_string())]);
            assert!(!versions[0].only_latest_update);
            assert!(versions[1].only_latest_update);

            store.conn_handle().lock().execute(
                "UPDATE objects SET crc32 = crc32 + 1 WHERE object_key = ?",
                [cached.as_ref()],
            ).unwrap();
            assert!(store.read_small(&cached).await.is_err(), "exercise a real stored-payload read failure");
            assert!(read_diff_cache_catalog(&mut scope,&store,keys.prefix(),&[]).await.is_err(),"stored CRC errors must not be mistaken for an empty cache version");
            for key in plan_diff_cache_clean(&store, keys.prefix()).await.unwrap() {
                ObjectStore::delete(&store, &key).await.unwrap();
            }
            assert!(read_diff_cache_catalog(&mut scope,&store, keys.prefix(), &[]).await.unwrap().is_empty());
            scope.release_all().await.unwrap();
            assert_eq!(store.read_small(&keys.section("2", "現在")).await.unwrap().unwrap(), b"current");
            assert!(!dir.path().join("site/title").exists(), "catalog/clean must not materialize DB-only files");
        });
    }

    #[test]
    fn gate_authority_roundtrips_between_filesystem_and_sql(){
        let root=tempfile::tempdir().unwrap();let conn=crate::native::sqlite::open_in_memory().unwrap();
        let mirror=NativeObjectStore::from_root(root.path().to_owned()).unwrap();
        let store=SqliteObjectStore::with_mirror_files(conn.clone(),mirror.clone(),false);
        let prefix=ObjectPrefix::new("novels/site/book/").unwrap();
        futures::executor::block_on(async{
            // FS gate opens at epoch 0; a live reader lease exists pre-cutover.
            mirror.rebind_writer(&prefix,0).await.unwrap();
            let lease=mirror.admit_reader(&prefix,"rd",60).await.unwrap();
            // FS→SQL adoption: SQL row at epoch+1, lease imported, gate migrated.
            mirror.adopt_fs_gate_locked(&prefix,&conn.lock()).unwrap();
            let (closed,epoch,origin)=crate::native::object_store::read_namespace_gate_locked(root.path(),&conn.lock(),prefix.as_ref().trim_end_matches('/')).unwrap().unwrap();
            assert_eq!((closed,epoch,origin.as_str()),(false,1,""));
            // Old FS token dead; SQL rebind works at the advanced epoch.
            assert!(mirror.rebind_writer(&prefix,0).await.is_err());
            store.rebind_writer(&prefix,1).await.unwrap();
            // SQL→FS adoption: FS gate at sql_epoch+1, row+leases deleted.
            store.adopt_sql_gate_locked(&prefix).unwrap();
            assert!(store.rebind_writer(&prefix,1).await.is_err());
            let (closed,epoch,_)=crate::native::object_store::read_namespace_gate_locked(root.path(),&conn.lock(),prefix.as_ref().trim_end_matches('/')).unwrap().unwrap();
            assert_eq!((closed,epoch),(false,2));
            // The imported lease counts as a live reader; stale-token release
            // stays idempotent and clears the drain.
            assert_eq!(mirror.gate_info(&prefix).await.unwrap().pending_readers,1);
            mirror.release_reader(&prefix,&lease).await.unwrap();
            assert_eq!(mirror.gate_info(&prefix).await.unwrap().pending_readers,0);
            mirror.release_reader(&prefix,&lease).await.unwrap();
            mirror.rebind_writer(&prefix,2).await.unwrap();
            // Re-adoption FS→SQL: epoch advances again, reverse handoff cleared.
            mirror.adopt_fs_gate_locked(&prefix,&conn.lock()).unwrap();
            assert!(crate::native::object_store::read_fs_gate_handoff_raw(&conn.lock(),prefix.as_ref().trim_end_matches('/')).unwrap().is_none());
            let (_,epoch,_)=crate::native::object_store::read_namespace_gate_locked(root.path(),&conn.lock(),prefix.as_ref().trim_end_matches('/')).unwrap().unwrap();
            assert_eq!(epoch,3);
            // Idempotent re-runs: proven re-adoption + already-adopted reverse.
            mirror.adopt_fs_gate_locked(&prefix,&conn.lock()).unwrap();
            store.adopt_sql_gate_locked(&prefix).unwrap();
            store.adopt_sql_gate_locked(&prefix).unwrap();
            let (_,epoch,_)=crate::native::object_store::read_namespace_gate_locked(root.path(),&conn.lock(),prefix.as_ref().trim_end_matches('/')).unwrap().unwrap();
            assert_eq!(epoch,4);
        });
    }

    #[test]
    fn gate_adoption_fails_closed_on_conflict_and_closed_gates(){
        let root=tempfile::tempdir().unwrap();let conn=crate::native::sqlite::open_in_memory().unwrap();
        let mirror=NativeObjectStore::from_root(root.path().to_owned()).unwrap();
        let store=SqliteObjectStore::with_mirror_files(conn.clone(),mirror.clone(),false);
        let prefix=ObjectPrefix::new("novels/site/conflict/").unwrap();
        futures::executor::block_on(async{
            // Closed FS gate cannot migrate.
            mirror.close_gate(&prefix,"writer").await.unwrap();
            assert!(mirror.adopt_fs_gate_locked(&prefix,&conn.lock()).is_err());
            mirror.open_gate(&prefix,"writer",1).await.unwrap();
            // A competing un-proven SQL row blocks adoption.
            conn.lock().execute("INSERT INTO novel_access_gates(prefix,epoch,closed,updated_at) VALUES(?1,99,0,0)",[prefix.as_ref().trim_end_matches('/')]).unwrap();
            assert!(mirror.adopt_fs_gate_locked(&prefix,&conn.lock()).is_err());
            conn.lock().execute("DELETE FROM novel_access_gates WHERE prefix=?1",[prefix.as_ref().trim_end_matches('/')]).unwrap();
            // Closed SQL gate cannot migrate either.
            mirror.adopt_fs_gate_locked(&prefix,&conn.lock()).unwrap();
            store.close_gate(&prefix,"writer").await.unwrap();
            assert!(store.adopt_sql_gate_locked(&prefix).is_err());
        });
    }

    fn temp_store() -> (tempfile::TempDir, SqliteObjectStore) {
        let dir = tempfile::tempdir().unwrap();
        let conn = {
            let mut conn = Connection::open_in_memory().unwrap();
            crate::native::sqlite::migrations::apply(&mut conn).unwrap();
            conn
        };
        let mirror = NativeObjectStore::from_root(dir.path().to_path_buf()).unwrap();
        (
            dir,
            SqliteObjectStore::new(Arc::new(Mutex::new(conn)), mirror),
        )
    }

    /// ミラー無効なら実ファイルを書かず、要求されたときだけ取り出すこと。
    #[test]
    fn mirror_files_can_be_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let conn = {
            let mut conn = Connection::open_in_memory().unwrap();
            crate::native::sqlite::migrations::apply(&mut conn).unwrap();
            conn
        };
        let mirror = NativeObjectStore::from_root(dir.path().to_path_buf()).unwrap();
        let store = SqliteObjectStore::with_mirror_files(Arc::new(Mutex::new(conn)), mirror, false);

        let key = ObjectKey::try_new("novels/site/title/本文/1 x.yaml").unwrap();
        futures::executor::block_on(ObjectStore::write_small(&store, &key, b"body".to_vec()))
            .unwrap();
        let mirrored = dir.path().join("site/title/本文/1 x.yaml");
        assert!(
            !mirrored.exists(),
            "no mirror file may be written when the mirror is off"
        );

        // 一覧は DB 行を返す (この時点で実ファイルは 1 つも無い)。
        let request = ObjectListRequest::new(
            ObjectPrefix::new("novels/site").unwrap(),
            NonZeroUsize::new(10).unwrap(),
        );
        let page = futures::executor::block_on(ObjectStore::list_page(&store, &request)).unwrap();
        assert_eq!(page.objects.len(), 1);

        let prefix=ObjectPrefix::new("novels/site/title").unwrap();
        futures::executor::block_on(async{let lease=store.admit_reader(&prefix,"db-only",30).await.unwrap();let mut stream=store.read_stream_leased(&prefix,&lease,&key,0..u64::MAX).await.unwrap().unwrap();assert_eq!(stream.next().await.unwrap().unwrap(),b"body");assert!(stream.next().await.is_none());store.release_reader(&prefix,&lease).await.unwrap();});
        assert!(!mirrored.exists(),"leased reads must not materialize canonical paths");
    }

    #[test]
    fn leased_reads_page_past_illustrations_without_materializing(){
        let(dir,mut store)=temp_store();store.mirror_files=false;let prefix=ObjectKey::try_new("novels/site/title").unwrap();
        let keys=NovelObjectKeys::new("site","title",false).unwrap();
        for index in 0..4096{store.write_blocking(&illustration_key(&prefix,&format!("{index:04}.png")).unwrap(),b"image").unwrap();}
        let body=keys.section("1","body");store.write_blocking(&body,b"body").unwrap();
        let existing=dir.path().join("site/title/本文/2 existing.yaml");std::fs::create_dir_all(existing.parent().unwrap()).unwrap();std::fs::write(&existing,b"keep local").unwrap();
        futures::executor::block_on(async{
            let prefix=ObjectPrefix::from(prefix);let lease=store.admit_reader(&prefix,"paged",30).await.unwrap();let mut request=ObjectListRequest::new(prefix.clone(),NonZeroUsize::new(100).unwrap());let mut found=false;
            loop{let page=store.list_page(&request).await.unwrap();for metadata in page.objects{if metadata.key==body{let mut stream=store.read_stream_leased(&prefix,&lease,&body,0..u64::MAX).await.unwrap().unwrap();assert_eq!(stream.next().await.unwrap().unwrap(),b"body");assert!(stream.next().await.is_none());found=true;}}
                match page.next_cursor{Some(cursor)=>request=request.after(cursor),None=>break}
            }assert!(found);store.release_reader(&prefix,&lease).await.unwrap();
        });
        assert!(!dir.path().join("site/title/本文/1 body.yaml").exists());assert!(!dir.path().join("site/title/挿絵").exists());assert_eq!(std::fs::read(existing).unwrap(),b"keep local");
    }

    #[test]
    fn leased_inputs_leave_existing_conversion_outputs_untouched(){
        let(dir,mut store)=temp_store();store.mirror_files=false;let prefix=ObjectKey::try_new("novels/site/title").unwrap();
        let keys=NovelObjectKeys::new("site","title",false).unwrap();
        let inputs=[keys.toc(),keys.section("1","body"),keys.setting(),keys.replace(),prefix.join("converter.yaml").unwrap(),keys.illustration_cache(),prefix.join("cover.png").unwrap()];
        for key in&inputs{store.write_blocking(key,b"stored").unwrap();}
        let novel_dir=dir.path().join("site/title");std::fs::create_dir_all(&novel_dir).unwrap();for output in ["novel.txt","Book.epub"]{std::fs::write(novel_dir.join(output),b"new conversion output").unwrap();}
        futures::executor::block_on(async{let scope=ObjectPrefix::from(prefix.clone());let lease=store.admit_reader(&scope,"inputs",30).await.unwrap();for key in&inputs{let mut stream=store.read_stream_leased(&scope,&lease,key,0..u64::MAX).await.unwrap().unwrap();assert_eq!(stream.next().await.unwrap().unwrap(),b"stored");assert!(stream.next().await.is_none());}store.release_reader(&scope,&lease).await.unwrap();});
        for output in ["novel.txt","Book.epub"]{assert_eq!(std::fs::read(novel_dir.join(output)).unwrap(),b"new conversion output");}for key in&inputs{let logical=key.as_ref().strip_prefix("novels/").unwrap_or(key.as_ref());assert!(!dir.path().join(logical).exists());}
    }

    #[test]
    fn small_write_read_stat_delete_roundtrip() {
        let (_dir, store) = temp_store();
        let key = ObjectKey::try_new("novels/site/title/toc.yaml").unwrap();
        futures::executor::block_on(store.write_small(&key, b"hello".to_vec())).unwrap();

        let meta = futures::executor::block_on(ObjectStore::stat(&store, &key))
            .unwrap()
            .unwrap();
        assert_eq!(meta.size, 5);

        let data = futures::executor::block_on(store.read_small(&key)).unwrap().unwrap();
        assert_eq!(data, b"hello");

        futures::executor::block_on(ObjectStore::delete(&store, &key)).unwrap();
        assert!(
            futures::executor::block_on(store.read_small(&key))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn chunked_payload_roundtrips() {
        let (_dir, store) = temp_store();
        let key = ObjectKey::try_new("generated/epub/book.epub").unwrap();
        // Incompressible bytes (xorshift PRNG) so the payload stays chunked
        // even with deflate enabled.
        let mut state = 0x9E3779B97F4A7C15u64;
        let payload: Vec<u8> = (0..(OBJECT_CHUNK_BYTES * 2 + 1234))
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 32) as u8
            })
            .collect();
        futures::executor::block_on(store.write_small(&key, payload.clone())).unwrap();
        let data = futures::executor::block_on(store.read_small(&key)).unwrap().unwrap();
        assert_eq!(data, payload);
    }

    #[test]
    fn compressible_payload_is_stored_compressed() {
        let (_dir, store) = temp_store();
        let key = ObjectKey::try_new("novels/site/title/本文/0001 本文.yaml").unwrap();
        let payload = "同じ文章の繰り返し。".repeat(10_000).into_bytes();
        futures::executor::block_on(store.write_small(&key, payload.clone())).unwrap();

        let conn = store.conn.lock();
        let (encoding, stored_len): (String, i64) = conn
            .query_row(
                "SELECT encoding, LENGTH(data) FROM objects WHERE object_key = ?",
                rusqlite::params![key.as_ref()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(encoding, "brotli");
        assert!((stored_len as usize) < payload.len());
        drop(conn);

        let data = futures::executor::block_on(store.read_small(&key)).unwrap().unwrap();
        assert_eq!(data, payload);
    }

    #[test]
    fn filesystem_fallback_reads_unimported_file() {
        let (dir, store) = temp_store();
        let key = ObjectKey::try_new("novels/site/title/setting.ini").unwrap();
        let path = dir.path().join("site/title/setting.ini");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"legacy").unwrap();

        let data = futures::executor::block_on(store.read_small(&key)).unwrap().unwrap();
        assert_eq!(data, b"legacy");
    }

    #[test]
    fn list_merges_db_and_filesystem() {
        let (dir, store) = temp_store();
        let db_key = ObjectKey::try_new("novels/site/a/toc.yaml").unwrap();
        futures::executor::block_on(store.write_small(&db_key, b"db".to_vec())).unwrap();
        let fs_path = dir.path().join("site/b/toc.yaml");
        std::fs::create_dir_all(fs_path.parent().unwrap()).unwrap();
        std::fs::write(&fs_path, b"fs").unwrap();

        let request = ObjectListRequest::new(
            ObjectPrefix::new("novels/site").unwrap(),
            NonZeroUsize::new(50).unwrap(),
        );
        let page = futures::executor::block_on(store.list_page(&request)).unwrap();
        let keys: Vec<String> = page
            .objects
            .iter()
            .map(|meta| meta.key.as_ref().to_string())
            .collect();
        assert!(keys.contains(&"novels/site/a/toc.yaml".to_string()));
        assert!(keys.contains(&"novels/site/b/toc.yaml".to_string()));
    }

    #[test]
    fn import_populates_objects_from_archive() {
        let dir = tempfile::tempdir().unwrap();
        let conn = {
            let mut conn = Connection::open_in_memory().unwrap();
            crate::native::sqlite::migrations::apply(&mut conn).unwrap();
            conn
        };
        let mirror = NativeObjectStore::from_root(dir.path().to_path_buf()).unwrap();
        let path = dir.path().join("site/title/toc.yaml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"toc-data").unwrap();

        let conn = Arc::new(Mutex::new(conn));
        let imported = import_archive_into_objects(&conn, &mirror).unwrap();
        assert_eq!(imported, 1);

        let store = SqliteObjectStore::new(conn, mirror);
        let key = ObjectKey::try_new("novels/site/title/toc.yaml").unwrap();
        let data = futures::executor::block_on(store.read_small(&key)).unwrap().unwrap();
        assert_eq!(data, b"toc-data");
    }

    /// A mirror file deleted outside narou is rebuilt from the stored object.
    #[test]
    fn restore_mirror_file_rebuilds_deleted_file() {
        let (dir, store) = temp_store();
        let keys = crate::platform::NovelObjectKeys::new("site", "n1234ab", false).unwrap();
        let section = keys.section("1", "第一話");
        futures::executor::block_on(store.write_small(&section, b"section".to_vec())).unwrap();
        let path = dir.path().join("site/n1234ab/本文/1 第一話.yaml");
        assert!(path.is_file());

        std::fs::remove_file(&path).unwrap();
        assert!(store.restore_mirror_file(&path).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"section");

        let unknown = dir.path().join("site/n1234ab/本文/9 存在しない.yaml");
        assert!(!store.restore_mirror_file(&unknown).unwrap());
    }

    /// `remove --with-file` relies on this: a prefix delete must drop the
    /// stored objects *and* their mirror files, including nested section keys.
    #[test]
    fn delete_prefix_removes_stored_objects_and_mirror_files() {
        let (dir, store) = temp_store();
        let keys = crate::platform::NovelObjectKeys::new("site", "n1234ab", false).unwrap();
        let section = keys.section("1", "第一話");
        futures::executor::block_on(async {
            store.write_small(&keys.toc(), b"toc".to_vec()).await.unwrap();
            store
                .write_small(&section, b"section".to_vec())
                .await
                .unwrap();
        });
        let section_path = dir.path().join("site/n1234ab/本文/1 第一話.yaml");
        assert!(section_path.is_file());

        crate::native::object_store::delete_prefix_sync(&store, keys.prefix()).unwrap();

        assert!(
            futures::executor::block_on(store.read_small(&keys.toc()))
                .unwrap()
                .is_none()
        );
        assert!(
            futures::executor::block_on(store.read_small(&section))
                .unwrap()
                .is_none()
        );
        assert!(!section_path.exists());
        assert!(!dir.path().join("site/n1234ab/toc.yaml").exists());
    }

    /// Turning the mirror off after an object was written must not leave a
    /// readable file behind: the delete covers both copies, so nothing
    /// resurrects through the filesystem fallback (or through narou.rb after a
    /// YAML rollback). `list`/`stat`/`read_small`/`read_stream` all agree.
    #[test]
    fn delete_removes_the_filesystem_copy_when_the_mirror_is_off() {
        let (dir, mut store) = temp_store();
        let key = ObjectKey::try_new("novels/site/title/本文/1 第一話.yaml").unwrap();
        futures::executor::block_on(store.write_small(&key, b"body".to_vec())).unwrap();
        let path = dir.path().join("site/title/本文/1 第一話.yaml");
        assert!(path.is_file());

        store.mirror_files = false;
        futures::executor::block_on(ObjectStore::delete(&store, &key)).unwrap();

        assert!(
            !path.exists(),
            "the stale mirror copy must go with the stored row"
        );
        assert!(
            futures::executor::block_on(ObjectStore::stat(&store, &key))
                .unwrap()
                .is_none()
        );
        assert!(
            futures::executor::block_on(store.read_small(&key))
                .unwrap()
                .is_none()
        );
        assert!(
            futures::executor::block_on(AssetStore::read_stream(&store, &key))
                .unwrap()
                .is_none()
        );
        let request = ObjectListRequest::new(
            ObjectPrefix::new("novels/site").unwrap(),
            NonZeroUsize::new(10).unwrap(),
        );
        let page = futures::executor::block_on(store.list_page(&request)).unwrap();
        assert!(page.objects.is_empty(), "a deleted object must not be listed");
    }

    /// The fallback boundary: only keys the database does not know come from
    /// the filesystem, and a stored row always wins over a differing file.
    #[test]
    fn filesystem_fallback_serves_only_keys_the_database_lacks() {
        let (dir, mut store) = temp_store();
        store.mirror_files = false;

        // A file the store never saw (pre-import, or a writer that bypasses
        // the store) stays readable and listed.
        let legacy = ObjectKey::try_new("novels/site/title/本文/2 第二話.yaml").unwrap();
        let legacy_path = dir.path().join("site/title/本文/2 第二話.yaml");
        std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        std::fs::write(&legacy_path, b"legacy").unwrap();

        // The database wins over a stale file with the same key.
        let managed = ObjectKey::try_new("novels/site/title/本文/1 第一話.yaml").unwrap();
        let managed_path = dir.path().join("site/title/本文/1 第一話.yaml");
        std::fs::write(&managed_path, b"stale").unwrap();
        futures::executor::block_on(store.write_small(&managed, b"stored".to_vec())).unwrap();

        assert_eq!(
            futures::executor::block_on(store.read_small(&managed))
                .unwrap()
                .unwrap(),
            b"stored"
        );
        assert_eq!(
            futures::executor::block_on(store.read_small(&legacy))
                .unwrap()
                .unwrap(),
            b"legacy"
        );
        assert!(
            futures::executor::block_on(ObjectStore::stat(&store, &legacy))
                .unwrap()
                .is_some()
        );
        let request = ObjectListRequest::new(
            ObjectPrefix::new("novels/site").unwrap(),
            NonZeroUsize::new(10).unwrap(),
        );
        let page = futures::executor::block_on(store.list_page(&request)).unwrap();
        let keys: Vec<String> = page
            .objects
            .iter()
            .map(|meta| meta.key.as_ref().to_string())
            .collect();
        assert_eq!(
            keys,
            vec![
                "novels/site/title/本文/1 第一話.yaml".to_string(),
                "novels/site/title/本文/2 第二話.yaml".to_string(),
            ]
        );
        let input = store.clone();
        assert_eq!(futures::executor::block_on(input.read_small(&managed)).unwrap().unwrap(), b"stored");
        assert_eq!(futures::executor::block_on(ObjectStore::stat(&input, &managed)).unwrap().unwrap().size, 6);
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let bytes = runtime.block_on(async {
            let mut stream = input.read_stream(&managed).await.unwrap().unwrap();
            let mut bytes = Vec::new();
            while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
                bytes.extend(chunk.unwrap());
            }
            bytes
        });
        assert_eq!(bytes, b"stored");
        assert_eq!(std::fs::read(&managed_path).unwrap(), b"stale",
            "canonical reads must leave the stale filesystem copy untouched");
        std::fs::remove_file(managed_path).unwrap();
        assert_eq!(futures::executor::block_on(input.read_small(&managed)).unwrap().unwrap(), b"stored");
    }

    /// When the filesystem copy cannot be removed, the delete fails and the
    /// object stays intact in both copies instead of half-deleting (a
    /// read-visible file with no stored row is the failure mode to avoid).
    #[test]
    fn delete_fails_when_the_filesystem_copy_cannot_be_removed() {
        let (dir, store) = temp_store();
        let key = ObjectKey::try_new("novels/site/title/toc.yaml").unwrap();
        futures::executor::block_on(store.write_small(&key, b"toc".to_vec())).unwrap();

        // A directory in the file's place makes `remove_file` fail on every
        // platform (EISDIR on Unix, access denied on Windows).
        let path = dir.path().join("site/title/toc.yaml");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();

        let error = futures::executor::block_on(ObjectStore::delete(&store, &key)).unwrap_err();
        assert!(!error.to_string().is_empty());
        assert_eq!(
            futures::executor::block_on(store.read_small(&key))
                .unwrap()
                .unwrap(),
            b"toc",
            "the stored row survives a failed delete"
        );
    }

    /// `move_or_copy` deletes the source row, so without the mirror the stale
    /// source file has to go as well or the object stays readable.
    #[test]
    fn move_or_copy_removes_the_stale_source_copy() {
        let (dir, mut store) = temp_store();
        let source = ObjectKey::try_new("novels/site/title/本文/1 第一話.yaml").unwrap();
        let destination =
            ObjectKey::try_new("novels/site/title/本文/cache/20240101/1 第一話.yaml").unwrap();
        futures::executor::block_on(store.write_small(&source, b"body".to_vec())).unwrap();
        let source_path = dir.path().join("site/title/本文/1 第一話.yaml");
        assert!(source_path.is_file());

        store.mirror_files = false;
        futures::executor::block_on(AssetStore::move_or_copy(&store, &source, &destination))
            .unwrap();

        assert!(!source_path.exists());
        assert!(
            futures::executor::block_on(store.read_small(&source))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            futures::executor::block_on(store.read_small(&destination))
                .unwrap()
                .unwrap(),
            b"body"
        );
    }

    /// Deterministic incompressible bytes so payloads stay chunked.
    fn incompressible(len: usize) -> Vec<u8> {
        let mut state = 0x9e37_79b9u32;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect()
    }

    /// `db verify` reports the *exact* corrupted key and reason for inline,
    /// chunked and section-body payloads, including decode failures; healthy
    /// payloads are not reported.
    #[test]
    fn verify_reports_exact_corruption_keys() {
        let (_dir, store) = temp_store();
        let inline_key = ObjectKey::try_new("novels/site/title/toc.yaml").unwrap();
        let decode_key = ObjectKey::try_new("novels/site/title/setting.ini").unwrap();
        let chunk_key = ObjectKey::try_new("novels/site/title/raw/1.html").unwrap();
        let big = incompressible(OBJECT_CHUNK_BYTES + 1024);
        let body_hash = {
            let guard = store.conn_handle().lock();
            crate::native::sqlite::content::store_body(&guard, "body: verify\n").unwrap()
        };
        futures::executor::block_on(async {
            store
                .write_small(&inline_key, b"toc".to_vec())
                .await
                .unwrap();
            store
                .write_small(&decode_key, b"setting".to_vec())
                .await
                .unwrap();
            store.write_small(&chunk_key, big).await.unwrap();
        });

        {
            let guard = store.conn_handle().lock();
            assert!(verify_payload_crc32(&guard).unwrap().is_empty());
        }

        {
            let guard = store.conn_handle().lock();
            // CRC mismatch on an inline payload.
            guard
                .execute(
                    "UPDATE objects SET crc32 = crc32 + 1 WHERE object_key = ?",
                    rusqlite::params![inline_key.as_ref()],
                )
                .unwrap();
            // Malformed codec marker: a decode failure, not a CRC one.
            guard
                .execute(
                    "UPDATE objects SET encoding = 'bogus' WHERE object_key = ?",
                    rusqlite::params![decode_key.as_ref()],
                )
                .unwrap();
            // A chunk of the chunked payload is altered.
            guard
                .execute(
                    "UPDATE object_chunks SET data = X'FF' WHERE object_key = ? AND seq = 0",
                    rusqlite::params![chunk_key.as_ref()],
                )
                .unwrap();
            guard
                .execute(
                    "UPDATE section_bodies SET crc32 = crc32 + 7 WHERE body_hash = ?",
                    rusqlite::params![body_hash],
                )
                .unwrap();
        }

        let corruptions = {
            let guard = store.conn_handle().lock();
            verify_payload_crc32(&guard).unwrap()
        };
        let keys: Vec<&str> = corruptions.iter().map(|issue| issue.key.as_str()).collect();
        assert!(keys.contains(&inline_key.as_ref()), "{keys:?}");
        assert!(keys.contains(&decode_key.as_ref()), "{keys:?}");
        assert!(keys.contains(&chunk_key.as_ref()), "{keys:?}");
        let body_key = format!("section_bodies:{}", hex_digest(&body_hash));
        assert!(keys.contains(&body_key.as_str()), "{keys:?}");
        assert_eq!(corruptions.len(), 4, "{corruptions:?}");
        for issue in &corruptions {
            assert!(!issue.reason.is_empty(), "{issue:?}");
        }
        let decode_reason = corruptions
            .iter()
            .find(|issue| issue.key == decode_key.as_ref())
            .map(|issue| issue.reason.as_str())
            .unwrap();
        assert!(decode_reason.contains("bogus"), "{decode_reason}");
    }

    /// Import stages files outside the connection lock and commits bounded
    /// batches: a multi-file archive (including a chunked payload) lands
    /// completely and reads back byte-for-byte.
    #[test]
    fn import_batches_multiple_files_including_chunked_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let conn = {
            let mut conn = Connection::open_in_memory().unwrap();
            crate::native::sqlite::migrations::apply(&mut conn).unwrap();
            conn
        };
        let mirror = NativeObjectStore::from_root(dir.path().to_path_buf()).unwrap();
        let big = incompressible(OBJECT_CHUNK_BYTES + 4096);
        let files: [(&str, &[u8]); 3] = [
            ("site/title/toc.yaml", b"toc-data"),
            ("site/title/本文/1 第一話.yaml", b"body-data"),
            ("site/title/raw/1.html", big.as_slice()),
        ];
        for (relative, bytes) in files {
            let path = dir.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }

        let conn = Arc::new(Mutex::new(conn));
        let imported = import_archive_into_objects(&conn, &mirror).unwrap();
        assert_eq!(imported, 3);

        let store = SqliteObjectStore::new(conn, mirror);
        for (key, expected) in [
            ("novels/site/title/toc.yaml", b"toc-data".to_vec()),
            (
                "novels/site/title/本文/1 第一話.yaml",
                b"body-data".to_vec(),
            ),
            ("novels/site/title/raw/1.html", big),
        ] {
            let key = ObjectKey::try_new(key).unwrap();
            let data = futures::executor::block_on(store.read_small(&key)).unwrap().unwrap();
            assert_eq!(data, expected, "{key}");
        }
        let guard = store.conn_handle().lock();
        let inline: Option<Vec<u8>> = guard
            .query_row(
                "SELECT data FROM objects WHERE object_key = 'novels/site/title/raw/1.html'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(inline.is_none(), "large payload stays chunked");
        let chunks: i64 = guard
            .query_row(
                "SELECT COUNT(*) FROM object_chunks WHERE object_key = 'novels/site/title/raw/1.html'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(chunks >= 2, "{chunks}");
    }

    /// Prefix delete is a bulk operation: it removes every stored row under
    /// the prefix (inline and chunked, more than one old listing page) plus the
    /// mirror files, and leaves keys that only share a string prefix.
    #[test]
    fn delete_prefix_bulk_covers_chunked_rows_and_spares_siblings() {
        let (dir, store) = temp_store();
        let keys = crate::platform::NovelObjectKeys::new("site", "n1234ab", false).unwrap();
        let sibling = ObjectKey::try_new("novels/site/n1234ab2/toc.yaml").unwrap();
        let big_key = ObjectKey::try_new("novels/site/n1234ab/raw/1.html").unwrap();
        let big = incompressible(OBJECT_CHUNK_BYTES + 2048);
        futures::executor::block_on(async {
            for index in 1..=120 {
                let section = keys.section(&index.to_string(), &format!("第{index}話"));
                store
                    .write_small(&section, format!("body {index}").into_bytes())
                    .await
                    .unwrap();
            }
            store.write_small(&keys.toc(), b"toc".to_vec()).await.unwrap();
            store.write_small(&big_key, big).await.unwrap();
            store
                .write_small(&sibling, b"keep".to_vec())
                .await
                .unwrap();
        });
        let big_path = dir.path().join("site/n1234ab/raw/1.html");
        assert!(big_path.is_file());

        futures::executor::block_on(ObjectStore::delete_prefix(&store, keys.prefix())).unwrap();

        let (base, lower, upper) = {
            let base = keys.prefix().as_ref().to_string();
            let lower = format!("{base}/");
            let upper = prefix_upper_bound(&lower);
            (base, lower, upper)
        };
        let guard = store.conn_handle().lock();
        let remaining: i64 = guard
            .query_row(
                "SELECT COUNT(*) FROM objects
                 WHERE object_key = ?1 OR (object_key >= ?2 AND object_key < ?3)",
                rusqlite::params![base, lower, upper],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0);
        let chunks: i64 = guard
            .query_row("SELECT COUNT(*) FROM object_chunks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(chunks, 0, "chunk rows go with their objects");
        drop(guard);

        assert_eq!(
            futures::executor::block_on(store.read_small(&keys.toc())).unwrap(),
            None
        );
        assert_eq!(
            futures::executor::block_on(store.read_small(&big_key)).unwrap(),
            None
        );
        assert!(!big_path.exists(), "the mirror file is swept too");
        assert!(!dir.path().join("site/n1234ab/toc.yaml").exists());
        assert_eq!(
            futures::executor::block_on(store.read_small(&sibling))
                .unwrap()
                .unwrap(),
            b"keep",
            "a sibling whose name only shares a string prefix is untouched"
        );
        assert!(dir.path().join("site/n1234ab2/toc.yaml").is_file());
    }

    /// Prefix listings stay component-scoped (not string-scoped), keep the
    /// exact key match, and still merge bare files the database does not know.
    #[test]
    fn list_prefix_matches_components_and_merges_filesystem() {
        let (dir, mut store) = temp_store();
        store.mirror_files = false;
        futures::executor::block_on(async {
            for key in [
                "novels/site",
                "novels/site/小説/toc.yaml",
                "novels/site-x/toc.yaml",
                "novels/site0/toc.yaml",
            ] {
                store
                    .write_small(&ObjectKey::try_new(key).unwrap(), b"x".to_vec())
                    .await
                    .unwrap();
            }
        });
        let fs_only = dir.path().join("site/小説2/fs-only.yaml");
        std::fs::create_dir_all(fs_only.parent().unwrap()).unwrap();
        std::fs::write(&fs_only, b"fs").unwrap();

        let request = ObjectListRequest::new(
            ObjectPrefix::new("novels/site").unwrap(),
            NonZeroUsize::new(10).unwrap(),
        );
        let page = futures::executor::block_on(store.list_page(&request)).unwrap();
        let keys: Vec<String> = page
            .objects
            .iter()
            .map(|meta| meta.key.as_ref().to_string())
            .collect();
        assert_eq!(
            keys,
            vec![
                "novels/site".to_string(),
                "novels/site/小説/toc.yaml".to_string(),
                "novels/site/小説2/fs-only.yaml".to_string(),
            ]
        );
    }

    /// require_transfer_epoch must reject every stale epoch/wrong owner/wrong
    /// gate state through the actual mutation entry points, not just reads.
    #[test]
    fn transfer_epoch_validation_rejects_stale_owner_epoch_and_wrong_state(){
        let root=tempfile::tempdir().unwrap();let conn=crate::native::sqlite::open_in_memory().unwrap();
        let store=SqliteObjectStore::with_mirror_files(conn,NativeObjectStore::from_root(root.path().to_owned()).unwrap(),false);
        let prefix=ObjectPrefix::new("novels/site/epoch/").unwrap();
        let key=ObjectKey::try_new("novels/site/epoch/toc.yaml").unwrap();
        futures::executor::block_on(async{
            store.rebind_writer(&prefix,0).await.unwrap();
            // Stale epoch on an OPEN gate must fail before any row is touched.
            assert!(store.rebind_writer(&prefix,9).await.is_err());
            let lease=store.admit_reader(&prefix,"rd",60).await.unwrap();
            let epoch=store.close_gate(&prefix,"publication").await.unwrap();
            // A wrong owner cannot renew, reopen, mutate or release the gate.
            assert!(store.renew_gate(&prefix,"intruder",epoch).await.is_err());
            assert!(store.open_gate(&prefix,"intruder",epoch).await.is_err());
            let stage_id="epoch-install";store.stage_open(stage_id,"publication",epoch,30).await.unwrap();store.stage_append(stage_id,"publication",epoch,0,b"x",true).await.unwrap();
            let sha=hex::encode(sha2::Sha256::digest(b"x"));let stage=store.stage_seal(stage_id,"publication",epoch,1,&sha).await.unwrap();
            assert!(store.commit_staged(&prefix,"intruder",epoch,&stage.key,&key).await.is_err());
            // A stale epoch fails even for the true owner.
            assert!(store.commit_staged(&prefix,"publication",epoch+1,&stage.key,&key).await.is_err());
            assert!(store.open_gate(&prefix,"publication",epoch+1).await.is_err());
            // The gate stays closed on owner+epoch until its reader drains.
            assert!(store.commit_staged(&prefix,"publication",epoch,&stage.key,&key).await.is_err());
            store.release_reader(&prefix,&lease).await.unwrap();
            store.commit_staged(&prefix,"publication",epoch,&stage.key,&key).await.unwrap();
            store.open_gate(&prefix,"publication",epoch).await.unwrap();
            assert_eq!(store.read_small(&key).await.unwrap().unwrap(),b"x");
        });
    }

    /// After a committed SQL→FS handoff, no SQL-side path may revive the
    /// namespace: default-row creation, reader admission, rebind and writer
    /// close all fail closed while the FS gate serves the original tokens.
    #[test]
    fn revived_default_row_cannot_reopen_after_reverse_handoff(){
        let root=tempfile::tempdir().unwrap();let conn=crate::native::sqlite::open_in_memory().unwrap();
        let mirror=NativeObjectStore::from_root(root.path().to_owned()).unwrap();
        let store=SqliteObjectStore::with_mirror_files(conn.clone(),mirror.clone(),false);
        let prefix=ObjectPrefix::new("novels/site/revive/").unwrap();
        futures::executor::block_on(async{
            store.rebind_writer(&prefix,0).await.unwrap();
            store.adopt_sql_gate_locked(&prefix).unwrap();
            // The durable handoff retires every SQL token: no SQL path may
            // create a fresh epoch-0 row or serve the deleted one.
            assert!(store.gate_info(&prefix).await.is_err());
            assert!(store.admit_reader(&prefix,"late",60).await.is_err());
            assert!(store.rebind_writer(&prefix,0).await.is_err());
            assert!(store.rebind_writer(&prefix,1).await.is_err());
            assert!(store.close_gate(&prefix,"ghost").await.is_err());
            // The FS gate is the single live authority for this prefix.
            mirror.rebind_writer(&prefix,1).await.unwrap();
            mirror.admit_reader(&prefix,"rd",60).await.unwrap();
            assert_eq!(mirror.gate_info(&prefix).await.unwrap().epoch,1);
        });
    }

    /// Releasing a reader through the ORIGINAL backend drains the lease from
    /// the authority that actually owns it: FS tokens clear their imported
    /// SQL row, SQL tokens clear their imported FS gate entry.
    #[test]
    fn original_backend_release_drains_imported_leases_both_ways(){
        let _guard=crate::test_support::global_state_guard();
        let root=tempfile::tempdir().unwrap();
        let narou_dir=root.path().join(".narou");std::fs::create_dir_all(&narou_dir).unwrap();
        crate::native::sqlite::state::write_mode(&narou_dir,crate::native::sqlite::state::StorageMode::Sqlite).unwrap();
        let handle=crate::native::sqlite::state::active_for(&narou_dir).expect("sqlite state");
        // The mirror must live under the library's archive root so the FS
        // release path resolves this same `.narou` handle.
        let mirror=NativeObjectStore::from_root(root.path().join(crate::downloader::types::ARCHIVE_ROOT_DIR)).unwrap();
        let store=SqliteObjectStore::with_mirror_files(handle.conn_ref().clone(),mirror.clone(),false);
        let prefix=ObjectPrefix::new("novels/site/drain/").unwrap();
        futures::executor::block_on(async{
            // FS→SQL: the original FS release drains the imported SQL row.
            let fs_lease=mirror.admit_reader(&prefix,"fs-rd",300).await.unwrap();
            mirror.adopt_fs_gate_locked(&prefix,&handle.conn_ref().lock()).unwrap();
            {let count:i64=handle.conn_ref().lock().query_row("SELECT count(*) FROM novel_reader_leases WHERE prefix=?1",[prefix.as_ref().trim_end_matches('/')],|row|row.get(0)).unwrap();assert_eq!(count,1);}
            mirror.release_reader(&prefix,&fs_lease).await.unwrap();
            {let count:i64=handle.conn_ref().lock().query_row("SELECT count(*) FROM novel_reader_leases WHERE prefix=?1",[prefix.as_ref().trim_end_matches('/')],|row|row.get(0)).unwrap();assert_eq!(count,0,"original FS release must drain the imported SQL lease");}
            // SQL→FS: the original SQL release drains the imported FS entry.
            let sql_lease=store.admit_reader(&prefix,"sql-rd",300).await.unwrap();
            store.adopt_sql_gate_locked(&prefix).unwrap();
            assert_eq!(mirror.gate_info(&prefix).await.unwrap().pending_readers,1);
            store.release_reader(&prefix,&sql_lease).await.unwrap();
            assert_eq!(mirror.gate_info(&prefix).await.unwrap().pending_readers,0,"original SQL release must drain the imported FS lease");
            // The stale SQL token can never renew against the live FS gate.
            assert!(store.renew_reader(&prefix,&sql_lease,60).await.is_err());
        });
    }

    /// `admit_reader` / `release_reader` sweep expired rows in the same
    /// transaction so a busy prefix cannot accumulate dead leases
    /// (`novel_reader_leases_expiry` index), while live rows — even on the
    /// same prefix — are never collected.
    #[test]
    fn reader_admission_and_release_reclaim_only_expired_lease_rows(){
        let root=tempfile::tempdir().unwrap();let conn=crate::native::sqlite::open_in_memory().unwrap();
        let mirror=NativeObjectStore::from_root(root.path().to_owned()).unwrap();
        let store=SqliteObjectStore::with_mirror_files(conn.clone(),mirror,false);
        let prefix=ObjectPrefix::new("novels/site/gc/").unwrap();
        let other=ObjectPrefix::new("novels/site/other/").unwrap();
        futures::executor::block_on(async{
            store.rebind_writer(&prefix,0).await.unwrap();
            store.rebind_writer(&other,0).await.unwrap();
            let key=prefix.as_ref().trim_end_matches('/');
            let other_key=other.as_ref().trim_end_matches('/');
            let past=ts::now_secs()-60;
            let far_future=ts::now_secs()+3600;
            {
                let conn=conn.lock();
                // Expired rows on this prefix (the production accumulation),
                // plus a live row and an expired row on another prefix that
                // must not be touched by this prefix's sweep.
                conn.execute("INSERT INTO novel_reader_leases(prefix,reader_id,epoch,expires_at) VALUES(?,'dead-a',0,?)",rusqlite::params![key,past]).unwrap();
                conn.execute("INSERT INTO novel_reader_leases(prefix,reader_id,epoch,expires_at) VALUES(?,'dead-b',0,?)",rusqlite::params![key,past]).unwrap();
                conn.execute("INSERT INTO novel_reader_leases(prefix,reader_id,epoch,expires_at) VALUES(?,'alive',0,?)",rusqlite::params![key,far_future]).unwrap();
                conn.execute("INSERT INTO novel_reader_leases(prefix,reader_id,epoch,expires_at) VALUES(?,'dead-other',0,?)",rusqlite::params![other_key,past]).unwrap();
            }
            let count=|conn:&std::sync::Arc<parking_lot::Mutex<rusqlite::Connection>>,prefix:&str|->i64{
                conn.lock().query_row("SELECT count(*) FROM novel_reader_leases WHERE prefix=?1",[prefix],|row|row.get(0)).unwrap()
            };
            assert_eq!(count(&conn,key),3);
            store.admit_reader(&prefix,"new-reader",300).await.unwrap();
            assert_eq!(count(&conn,key),2,"expired rows of this prefix are swept, the live row stays");
            assert_eq!(count(&conn,other_key),1,"other prefixes are untouched");

            // release_reader also sweeps this prefix's expired leftovers.
            conn.lock().execute("INSERT INTO novel_reader_leases(prefix,reader_id,epoch,expires_at) VALUES(?,'dead-c',0,?)",rusqlite::params![key,past]).unwrap();
            let lease=GateLease{reader_id:"new-reader".into(),epoch:0,expires_at:far_future};
            store.release_reader(&prefix,&lease).await.unwrap();
            assert_eq!(count(&conn,key),1,"release drains the token AND sweeps expired rows; 'alive' survives");
        });
    }

    /// Crash boundaries of SQL→FS adoption: an uncommitted intent is rolled
    /// back by the startup sweep (journal orphan removed, SQL row stays the
    /// single authority); a committed intent is finalized from its durable
    /// journal exactly once.
    #[test]
    fn startup_sweep_rolls_back_orphans_and_finalizes_committed_handoffs(){
        let root=tempfile::tempdir().unwrap();let conn=crate::native::sqlite::open_in_memory().unwrap();
        let mirror=NativeObjectStore::from_root(root.path().to_owned()).unwrap();
        let store=SqliteObjectStore::with_mirror_files(conn.clone(),mirror.clone(),false);
        let rollback=ObjectPrefix::new("novels/site/rollback/").unwrap();
        let resume=ObjectPrefix::new("novels/site/resume/").unwrap();
        futures::executor::block_on(async{
            store.rebind_writer(&rollback,0).await.unwrap();
            store.rebind_writer(&resume,0).await.unwrap();
        });
        // Orphan journal: fabricated pending gate with NO recorded intent —
        // the sweep must delete it and keep the SQL row authoritative.
        let orphan=FsGate{prefix:Some(rollback.as_ref().trim_end_matches('/').to_owned()),origin_url_fold:String::new(),content_revision:0,epoch:1,owner:None,updated_at:0,migrated:false,readers:std::collections::BTreeMap::new()};
        mirror.install_pending_gate_locked(&rollback,&orphan).unwrap();
        // Abandoned intent: the in-tx half ran but the transaction rolled
        // back — SQL stays authoritative and the journal orphan is retired.
        {let conn_lock=conn.lock();let tx=rusqlite::Transaction::new_unchecked(&conn_lock,rusqlite::TransactionBehavior::Immediate).unwrap();
            let gate=adopt_sql_gate_tx(&mirror,&tx,&resume,true).unwrap();
            assert!(gate.is_some());tx.rollback().unwrap();}
        assert!(mirror.read_pending_gate_locked(&resume).unwrap().is_some(),"rolled-back intent leaves its durable journal for the sweep");
        {let row:bool=conn.lock().query_row("SELECT EXISTS(SELECT 1 FROM novel_access_gates WHERE prefix=?1)",[resume.as_ref().trim_end_matches('/')],|row|row.get(0)).unwrap();assert!(row,"rollback restores the SQL row");}
        // Startup recovery.
        reconcile_namespace_gate_authority(&mirror,&conn).unwrap();
        // Orphan removed, SQL row still the only authority.
        assert!(mirror.read_pending_gate_locked(&rollback).unwrap().is_none());
        {let row:bool=conn.lock().query_row("SELECT EXISTS(SELECT 1 FROM novel_access_gates WHERE prefix=?1)",[rollback.as_ref().trim_end_matches('/')],|row|row.get(0)).unwrap();assert!(row);}
        futures::executor::block_on(async{store.rebind_writer(&rollback,0).await.unwrap();});
        // The rolled-back intent's journal was swept too: the SQL row stays
        // the single authority.
        assert!(mirror.read_pending_gate_locked(&resume).unwrap().is_none());
        {let row:bool=conn.lock().query_row("SELECT EXISTS(SELECT 1 FROM novel_access_gates WHERE prefix=?1)",[resume.as_ref().trim_end_matches('/')],|row|row.get(0)).unwrap();assert!(row,"the rolled-back intent leaves the SQL row the single authority");}
        futures::executor::block_on(async{store.rebind_writer(&resume,0).await.unwrap();});
    }

    /// A committed SQL→FS intent whose transaction deleted the row (crash
    /// before finalize) completes from its durable journal — the FS gate is
    /// installed at the recorded epoch and the journal retired.
    #[test]
    fn startup_sweep_completes_committed_handoff_from_journal(){
        let root=tempfile::tempdir().unwrap();let conn=crate::native::sqlite::open_in_memory().unwrap();
        let mirror=NativeObjectStore::from_root(root.path().to_owned()).unwrap();
        let store=SqliteObjectStore::with_mirror_files(conn.clone(),mirror.clone(),false);
        let prefix=ObjectPrefix::new("novels/site/journal/").unwrap();
        futures::executor::block_on(async{
            store.rebind_writer(&prefix,0).await.unwrap();
            let lease=store.admit_reader(&prefix,"rd",300).await.unwrap();
            // Commit the intent + journal + row/lease deletes as one unit —
            // the durable crash state between COMMIT and canonical install.
            store.adopt_sql_gate_locked(&prefix).unwrap();
            // Fabricate the crash: intent committed, canonical missing,
            // journal still present is impossible after a clean adopt —
            // rebuild it from the recorded tuple to test finalize itself.
            let Some((sql_epoch,fs_epoch,origin))=crate::native::object_store::read_fs_gate_handoff_raw(&conn.lock(),prefix.as_ref().trim_end_matches('/')).unwrap()else{panic!("handoff must be recorded")};
            assert_eq!((sql_epoch,fs_epoch,origin.as_str()),(0,1,""));
            assert!(store.release_reader(&prefix,&lease).await.is_ok());
        });
        // Simulate the pre-finalize crash: pending journal exists and no
        // canonical file — resolve must finalize it.
        std::fs::remove_file(mirror.gate_path(&prefix).unwrap()).unwrap();
        let journal=FsGate{prefix:Some(prefix.as_ref().trim_end_matches('/').to_owned()),origin_url_fold:String::new(),content_revision:0,epoch:1,owner:None,updated_at:0,migrated:false,readers:std::collections::BTreeMap::new()};
        mirror.install_pending_gate_locked(&prefix,&journal).unwrap();
        reconcile_namespace_gate_authority(&mirror,&conn).unwrap();
        assert!(mirror.read_pending_gate_locked(&prefix).unwrap().is_none());
        {let row:bool=conn.lock().query_row("SELECT EXISTS(SELECT 1 FROM novel_access_gates WHERE prefix=?1)",[prefix.as_ref().trim_end_matches('/')],|row|row.get(0)).unwrap();assert!(!row,"the retired SQL row must stay deleted");}
        futures::executor::block_on(async{
            assert_eq!(mirror.gate_info(&prefix).await.unwrap().epoch,1);
            mirror.rebind_writer(&prefix,1).await.unwrap();
            assert!(store.rebind_writer(&prefix,1).await.is_err());
        });
        // Second sweep is idempotent.
        reconcile_namespace_gate_authority(&mirror,&conn).unwrap();
    }

    /// Crash window between the FS→SQL commit and the migrated-marker write:
    /// every FS-side admission path must honor the committed handoff via the
    /// live SQL authority, heal the marker, and reject stale FS tokens — no
    /// startup sweep required.
    #[test]
    fn committed_handoff_without_marker_rejects_all_fs_admission(){
        let root=tempfile::tempdir().unwrap();
        let narou_dir=root.path().join(".narou");
        let state=crate::native::sqlite::state::configure(&narou_dir).unwrap();
        crate::native::sqlite::state::write_mode(&narou_dir,crate::native::sqlite::state::StorageMode::Sqlite).unwrap();
        let mirror=NativeObjectStore::from_root(root.path().join(crate::downloader::types::ARCHIVE_ROOT_DIR)).unwrap();
        let store=SqliteObjectStore::with_mirror_files(state.conn_ref().clone(),mirror.clone(),false);
        let prefix=ObjectPrefix::new("novels/site/lost-marker/").unwrap();
        let key=ObjectKey::try_new("novels/site/lost-marker/toc.yaml").unwrap();
        futures::executor::block_on(async{
            mirror.rebind_writer(&prefix,0).await.unwrap();
            let lease=mirror.admit_reader(&prefix,"rd",300).await.unwrap();
            mirror.write_small(&key,b"title: t\n".to_vec()).await.unwrap();
            // COMMIT the adoption but suppress the migrated marker: the
            // durable post-commit/pre-marker crash state.
            {let conn=state.conn_ref().lock();let tx=rusqlite::Transaction::new_unchecked(&conn,rusqlite::TransactionBehavior::Immediate).unwrap();
                tx.execute("INSERT INTO novel_access_gates(prefix,epoch,closed,owner,updated_at,content_revision,origin_url_fold) VALUES('novels/site/lost-marker',1,0,NULL,unixepoch(),0,'')",[]).unwrap();
                tx.execute("INSERT INTO novel_reader_leases(prefix,reader_id,epoch,expires_at) VALUES('novels/site/lost-marker','rd',0,99999999)",[]).unwrap();
                crate::native::object_store::record_sql_gate_handoff(&tx,"novels/site/lost-marker",0,1,"").unwrap();
                tx.commit().unwrap();}
            // The file still claims live authority — every mutation lane
            // must refuse it through the live SQL consult.
            assert!(mirror.rebind_writer(&prefix,0).await.is_err());
            assert!(mirror.write_small(&key,b"title: x\n".to_vec()).await.is_err());
            // Drain stays legal but heals the marker.
            mirror.release_reader(&prefix,&lease).await.unwrap();
            let gate:FsGate=serde_json::from_slice(&std::fs::read(mirror.gate_path(&prefix).unwrap()).unwrap()).unwrap();
            assert!(gate.migrated,"the release consult must heal the lost marker");
            // The stale imported epoch-0 token cannot revoke the lease a
            // reader re-admitted under the live SQL gate: release only ever
            // drops the exact (reader_id, epoch) row it was admitted at.
            let fresh=store.admit_reader(&prefix,"stale-proof",60).await.unwrap();
            store.release_reader(&prefix,&lease).await.unwrap();
            let alive:bool=state.conn_ref().lock().query_row("SELECT EXISTS(SELECT 1 FROM novel_reader_leases WHERE prefix=?1 AND reader_id='stale-proof' AND epoch=1)",[prefix.as_ref().trim_end_matches('/')],|row|row.get(0)).unwrap();
            assert!(alive,"stale epoch-0 release must not delete the epoch-1 row");
            store.renew_reader(&prefix,&fresh,60).await.unwrap();
            // SQL authority is whole and owns the namespace now.
            store.rebind_writer(&prefix,1).await.unwrap();
        });
    }

    /// `for_narou_root` must converge a crashed SQLite->Yaml flip before the
    /// store variant is chosen. Uncommitted barrier tx (yaml marker + intent
    /// file, no `yaml_pending` row, orphan `.gate.pending` journal): restore
    /// the sqlite marker, keep the live SQL row, drain the orphan journal —
    /// the FS side never gains authority.
    #[test]
    fn for_narou_root_reverts_uncommitted_cutover_flip(){
        let root=tempfile::tempdir().unwrap();
        let narou_dir=root.path().join(".narou");
        let state=crate::native::sqlite::state::configure(&narou_dir).unwrap();
        crate::native::sqlite::state::write_mode(&narou_dir,crate::native::sqlite::state::StorageMode::Sqlite).unwrap();
        let mirror=NativeObjectStore::from_root(root.path().join(crate::downloader::types::ARCHIVE_ROOT_DIR)).unwrap();
        let store=SqliteObjectStore::with_mirror_files(state.conn_ref().clone(),mirror.clone(),false);
        let prefix=ObjectPrefix::new("novels/site/flip/").unwrap();
        futures::executor::block_on(async{store.rebind_writer(&prefix,0).await.unwrap();});
        // Fabricate the crashed flip: intent file + Yaml marker + orphan
        // journal, but no committed yaml_pending and no fs_gate_handoffs.
        let journal=FsGate{prefix:Some(prefix.as_ref().trim_end_matches('/').to_owned()),origin_url_fold:String::new(),content_revision:0,epoch:1,owner:None,updated_at:0,migrated:false,readers:std::collections::BTreeMap::new()};
        mirror.install_pending_gate_locked(&prefix,&journal).unwrap();
        std::fs::write(narou_dir.join(".cutover-yaml-intent"),"sqlite-to-yaml").unwrap();
        crate::native::sqlite::state::write_mode(&narou_dir,crate::native::sqlite::state::StorageMode::Yaml).unwrap();
        let resolved=NativeStore::for_narou_root(root.path()).unwrap();
        assert!(matches!(resolved,NativeStore::Sqlite(_)),"uncommitted flip must restore SQLite authority");
        assert_eq!(crate::native::sqlite::state::read_mode(&narou_dir),crate::native::sqlite::state::StorageMode::Sqlite);
        assert!(!narou_dir.join(".cutover-yaml-intent").exists());
        assert!(mirror.read_pending_gate_locked(&prefix).unwrap().is_none(),"orphan journal must be rolled back");
        {let row:bool=state.conn_ref().lock().query_row("SELECT EXISTS(SELECT 1 FROM novel_access_gates WHERE prefix='novels/site/flip' AND closed=0)",[],|row|row.get(0)).unwrap();assert!(row,"the live SQL row stays the single authority");}
        futures::executor::block_on(async{
            // The FS gate never existed; stale FS admission stays absent and
            // SQL authority is whole.
            store.rebind_writer(&prefix,0).await.unwrap();
        });
    }

    /// Committed barrier tx (yaml marker + intent + `yaml_pending` row +
    /// recorded fs_gate_handoffs + pending journal, canonical FS gate never
    /// installed): `for_narou_root` keeps Yaml and finalizes the canonical
    /// gate from the journal — FS authority resumes exactly once.
    #[test]
    fn for_narou_root_finalizes_committed_cutover_flip(){
        let root=tempfile::tempdir().unwrap();
        let narou_dir=root.path().join(".narou");
        let state=crate::native::sqlite::state::configure(&narou_dir).unwrap();
        crate::native::sqlite::state::write_mode(&narou_dir,crate::native::sqlite::state::StorageMode::Sqlite).unwrap();
        let mirror=NativeObjectStore::from_root(root.path().join(crate::downloader::types::ARCHIVE_ROOT_DIR)).unwrap();
        let prefix=ObjectPrefix::new("novels/site/flip/").unwrap();
        futures::executor::block_on(async{mirror.rebind_writer(&prefix,0).await.unwrap();});
        // Fabricate the committed flip: handoff recorded, row deleted,
        // yaml_pending committed, canonical FS gate still missing.
        {let conn=state.conn_ref().lock();let tx=rusqlite::Transaction::new_unchecked(&conn,rusqlite::TransactionBehavior::Immediate).unwrap();
            tx.execute("CREATE TABLE IF NOT EXISTS cutover_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL) WITHOUT ROWID",[]).unwrap();
            crate::native::object_store::record_fs_gate_handoff(&tx,"novels/site/flip",0,1,"").unwrap();
            tx.execute("DELETE FROM novel_access_gates WHERE prefix='novels/site/flip'",[]).unwrap();
            tx.execute("INSERT OR REPLACE INTO cutover_meta(key,value) VALUES('yaml_pending','1')",[]).unwrap();
            tx.commit().unwrap();}
        let journal=FsGate{prefix:Some(prefix.as_ref().trim_end_matches('/').to_owned()),origin_url_fold:String::new(),content_revision:0,epoch:1,owner:None,updated_at:0,migrated:false,readers:std::collections::BTreeMap::new()};
        mirror.install_pending_gate_locked(&prefix,&journal).unwrap();
        std::fs::remove_file(mirror.gate_path(&prefix).unwrap()).unwrap();
        std::fs::write(narou_dir.join(".cutover-yaml-intent"),"sqlite-to-yaml").unwrap();
        crate::native::sqlite::state::write_mode(&narou_dir,crate::native::sqlite::state::StorageMode::Yaml).unwrap();
        let resolved=NativeStore::for_narou_root(root.path()).unwrap();
        assert!(matches!(resolved,NativeStore::Fs(_)),"committed flip keeps filesystem authority");
        assert_eq!(crate::native::sqlite::state::read_mode(&narou_dir),crate::native::sqlite::state::StorageMode::Yaml);
        assert!(!narou_dir.join(".cutover-yaml-intent").exists());
        assert!(mirror.read_pending_gate_locked(&prefix).unwrap().is_none(),"journal finalized into the canonical gate");
        futures::executor::block_on(async{
            // The finalized FS gate serves the namespace at fs_epoch 1.
            mirror.rebind_writer(&prefix,1).await.unwrap();
        });
    }

    /// Zero-gate library variant of the interrupted flip: marker=yaml +
    /// intent, no committed evidence and no gate files at all — the marker
    /// restores to sqlite and nothing else is touched.
    #[test]
    fn for_narou_root_reverts_flip_with_no_gates(){
        let root=tempfile::tempdir().unwrap();
        let narou_dir=root.path().join(".narou");
        let _state=crate::native::sqlite::state::configure(&narou_dir).unwrap();
        std::fs::write(narou_dir.join(".cutover-yaml-intent"),"sqlite-to-yaml").unwrap();
        crate::native::sqlite::state::write_mode(&narou_dir,crate::native::sqlite::state::StorageMode::Yaml).unwrap();
        let resolved=NativeStore::for_narou_root(root.path()).unwrap();
        assert!(matches!(resolved,NativeStore::Sqlite(_)),"uncommitted flip must restore SQLite authority");
        assert_eq!(crate::native::sqlite::state::read_mode(&narou_dir),crate::native::sqlite::state::StorageMode::Sqlite);
        assert!(!narou_dir.join(".cutover-yaml-intent").exists());
    }

    #[test]
    fn stage_stream_and_guarded_overwrite_do_not_leak_ledgers_checkpoints_or_old_blobs() {
        let (_dir, mut store) = temp_store();
        store.mirror_files = false;
        let prefix = ObjectPrefix::new("novels/site/book").unwrap();
        let epub = ObjectKey::try_new("novels/site/book/book.epub").unwrap();
        futures::executor::block_on(async {
            // Simulate a pre-existing imported direct row shadowed later by transfer_object_refs.
            ObjectStore::write_small(&store, &epub, b"legacy imported epub".to_vec()).await.unwrap();
            let fence = store.rebind_writer(&prefix, 0).await.unwrap();

            for version in [b"v1-epub-payload".repeat(40_000), b"v2-epub-payload".repeat(40_000)] {
                let stream: AssetStream = Box::pin(futures::stream::iter([
                    Ok(version[..64 * 1024].to_vec()),
                    Ok(version[64 * 1024..].to_vec()),
                ]));
                store.write_stream_guarded(&fence, &epub, stream).await.unwrap();
            }

            // A stream failing before installation must not leak stages either.
            let failing: AssetStream = Box::pin(futures::stream::iter([
                Ok(vec![1u8; 64 * 1024]),
                Err(NarouError::Platform("interrupted".into())),
            ]));
            assert!(store.write_stream_guarded(&fence, &epub, failing).await.is_err());

            let outside = ObjectKey::try_new("novels/site/other/book.epub").unwrap();
            let ok_stream: AssetStream = Box::pin(futures::stream::once(async { Ok(b"outside".to_vec()) }));
            assert!(store.write_stream_guarded(&fence, &outside, ok_stream).await.is_err());
        });

        let conn = store.conn.lock();
        let ledgers: i64 = conn.query_row("SELECT COUNT(*) FROM transfer_stage_ledger", [], |r| r.get(0)).unwrap();
        let refs: i64 = conn.query_row("SELECT COUNT(*) FROM transfer_object_refs", [], |r| r.get(0)).unwrap();
        let objects: i64 = conn.query_row("SELECT COUNT(*) FROM objects", [], |r| r.get(0)).unwrap();
        let checkpoints: i64 = conn
            .query_row("SELECT COUNT(*) FROM objects WHERE object_key >= '.narou-transfer/.checkpoints/' AND object_key < '.narou-transfer/.checkpoints0'", [], |r| r.get(0))
            .unwrap();
        let verified: i64 = conn.query_row("SELECT COUNT(*) FROM transfer_verified_objects", [], |r| r.get(0)).unwrap();
        assert_eq!(ledgers, 0);
        assert_eq!(checkpoints, 0);
        assert_eq!(refs, 1);
        assert_eq!(objects, 1, "shadowed direct row and superseded blobs must all be released");
        assert_eq!(verified, 1);
    }

    #[test]
    fn write_stream_once_cleans_stage_on_reuse_and_mismatch() {
        let (_dir, mut store) = temp_store();
        store.mirror_files = false;
        let bytes = b"content-addressed-payload".to_vec();
        let sha = hex::encode(sha2::Sha256::digest(&bytes));
        let key = ObjectKey::try_new(format!("illustrations/{sha}")).unwrap();
        futures::executor::block_on(async {
            let s1 = Box::pin(futures::stream::once({
                let bytes = bytes.clone();
                async move { Ok(bytes) }
            }));
            assert!(matches!(store.write_stream_once(&key, s1, bytes.len() as u64, &sha).await.unwrap(), WriteOnce::Written));

            let s2 = Box::pin(futures::stream::once({
                let bytes = bytes.clone();
                async move { Ok(bytes) }
            }));
            assert!(matches!(store.write_stream_once(&key, s2, bytes.len() as u64, &sha).await.unwrap(), WriteOnce::Reused));

            let wrong = Box::pin(futures::stream::once(async { Ok(b"wrong".to_vec()) }));
            assert!(store.write_stream_once(&key, wrong, 5, &sha).await.is_err());
        });

        let conn = store.conn.lock();
        let objects: i64 = conn.query_row("SELECT COUNT(*) FROM objects", [], |r| r.get(0)).unwrap();
        let ledgers: i64 = conn.query_row("SELECT COUNT(*) FROM transfer_stage_ledger", [], |r| r.get(0)).unwrap();
        assert_eq!(objects, 1);
        assert_eq!(ledgers, 0);
    }

    #[test]
    fn reclaim_sqlite_bloat_cleans_v068_leaked_stages_and_debris() {
        let (_dir, store) = temp_store();
        let now = SqliteObjectStore::now_rfc3339();
        let body_hash_live = b"live-hash".to_vec();
        let body_hash_orphan = b"orphan-hash".to_vec();
        let live_blob = ".narou-transfer/data/ordinary-live";
        let dead_blob = ".narou-transfer/data/ordinary-dead";
        let canonical = "novels/site/book/book.epub";

        {
            let mut conn = store.conn.lock();
            conn.execute(
                "INSERT INTO objects(object_key,size,updated_at,encoding,crc32,data) VALUES (?1,4,?2,'none',0,X'01020304')",
                rusqlite::params![live_blob, &now],
            ).unwrap();
            conn.execute(
                "INSERT INTO objects(object_key,size,updated_at,encoding,crc32,data) VALUES (?1,4,?2,'none',0,X'05060708')",
                rusqlite::params![dead_blob, &now],
            ).unwrap();
            conn.execute(
                "INSERT INTO objects(object_key,size,updated_at,encoding,crc32,data) VALUES (?1,3,?2,'none',0,X'AABBCC')",
                rusqlite::params![canonical, &now],
            ).unwrap();
            conn.execute(
                "INSERT INTO objects(object_key,size,updated_at,encoding,crc32,data) VALUES (?1,2,?2,'none',0,X'1122')",
                rusqlite::params![".narou-transfer/.checkpoints/ordinary-dead/65536", &now],
            ).unwrap();
            conn.execute(
                "INSERT INTO objects(object_key,size,updated_at,encoding,crc32,data) VALUES (?1,1,?2,'none',0,X'00')",
                rusqlite::params![".narou-transfer/.lock/abc", &now],
            ).unwrap();
            conn.execute(
                "INSERT INTO objects(object_key,size,updated_at,encoding,crc32,data) VALUES (?1,1,?2,'none',0,X'00')",
                rusqlite::params![".narou-transfer/.refs/novels/site/book/keep.ref", &now],
            ).unwrap();
            conn.execute(
                "INSERT INTO transfer_stage_ledger(stage_id,record_json) VALUES ('ordinary-dead','{}')",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO transfer_verified_objects(blob_key,size,sha256,generation) VALUES (?1,4,'00','g1'),('.narou-transfer/data/ordinary-missing',4,'00','g2')",
                rusqlite::params![live_blob],
            ).unwrap();
            conn.execute(
                "INSERT INTO transfer_object_refs(object_key,blob_key,size,sha256,backend,updated_at,blob_generation) VALUES (?1,?2,4,'00','d1',?3,'g1')",
                rusqlite::params![canonical, live_blob, &now],
            ).unwrap();
            conn.execute(
                "INSERT INTO novels(id,author,author_fold,title,title_fold,file_title,toc_url,toc_url_fold,sitename,sitename_fold,last_update) VALUES (1,'a','a','t','t','ft','https://x/1','https://x/1','s','s',?1)",
                [&now],
            ).unwrap();
            conn.execute(
                "INSERT INTO section_bodies(body_hash,size,encoding,crc32,data) VALUES (?1,1,'none',0,X'01'),(?2,1,'none',0,X'02')",
                rusqlite::params![&body_hash_live, &body_hash_orphan],
            ).unwrap();
            conn.execute(
                "INSERT INTO novel_sections(novel_id,idx,subtitle,body_hash) VALUES (1,'1','sub',?1)",
                [&body_hash_live],
            ).unwrap();

            let report = reclaim_sqlite_bloat(&mut conn, None, 0).unwrap();
            assert_eq!(report.stage_ledgers, 1);
            assert_eq!(report.stage_checkpoints, 1);
            assert_eq!(report.unreferenced_blobs, 1);
            assert_eq!(report.shadowed_objects, 1);
            assert_eq!(report.internal_debris, 1);
            assert_eq!(report.verified_orphans, 1);
            assert_eq!(report.section_bodies, 1);

            let remaining: Vec<String> = conn
                .prepare("SELECT object_key FROM objects ORDER BY object_key")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(
                remaining,
                vec![
                    ".narou-transfer/.refs/novels/site/book/keep.ref".to_string(),
                    live_blob.to_string(),
                ]
            );
            let live_bodies: i64 = conn.query_row("SELECT COUNT(*) FROM section_bodies", [], |r| r.get(0)).unwrap();
            assert_eq!(live_bodies, 1);
        }
    }

    #[test]
    fn import_archive_skips_internal_transfer_debris_and_guards_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("site/title")).unwrap();
        std::fs::write(root.join("site/title/toc.yaml"), b"title: ok\n").unwrap();
        std::fs::write(root.join("site/title/toc.yaml.lock"), b"lock").unwrap();
        std::fs::create_dir_all(root.join(".objects/.narou-transfer/.lock")).unwrap();
        std::fs::write(root.join(".objects/.narou-transfer/.lock/l1"), b"lock").unwrap();
        std::fs::create_dir_all(root.join(".objects/.narou-transfer/.refs/site")).unwrap();
        std::fs::write(root.join(".objects/.narou-transfer/.refs/site/k.ref"), b"ref").unwrap();
        std::fs::create_dir_all(root.join(".objects/.narou-staging")).unwrap();
        std::fs::write(root.join(".objects/.narou-staging/tmp"), b"stage").unwrap();

        let mirror = NativeObjectStore::from_root(root.to_path_buf()).unwrap();
        let conn = crate::native::sqlite::open_in_memory().unwrap();
        let imported = import_archive_into_objects(&conn, &mirror).unwrap();
        assert_eq!(imported, 2);

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root, root.join("site/loop")).unwrap();
            let err = import_archive_into_objects(&conn, &mirror).unwrap_err();
            assert!(err.to_string().contains("cyclic native namespace link"), "{err}");
        }
    }

    #[test]
    fn list_sqlite_objects_page_merges_direct_and_ref_objects_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let mirror = NativeObjectStore::from_root(dir.path().to_path_buf()).unwrap();
        let conn = crate::native::sqlite::open_in_memory().unwrap();
        let store = SqliteObjectStore::with_mirror_files(conn.clone(), mirror, false);

        futures::executor::block_on(async {
            for (k, body) in [
                ("novels/site/book/a.txt", b"a-direct".as_slice()),
                ("novels/site/book/b.txt", b"b-shadowed".as_slice()),
                ("novels/site/book/d.txt", b"d-direct".as_slice()),
                (".narou-transfer/.data/blob-b", b"b-blob".as_slice()),
                (".narou-transfer/.data/blob-c", b"c-blob-ref".as_slice()),
            ] {
                let key = ObjectKey::try_new(k).unwrap();
                ObjectStore::write_small(&store, &key, body.to_vec()).await.unwrap();
            }
        });
        {
            let guard = conn.lock();
            for (logical, blob, size) in [
                ("novels/site/book/b.txt", ".narou-transfer/.data/blob-b", 6i64),
                ("novels/site/book/c.txt", ".narou-transfer/.data/blob-c", 10i64),
            ] {
                let generation: String = guard
                    .query_row("SELECT generation FROM objects WHERE object_key=?", [blob], |r| r.get(0))
                    .unwrap();
                guard
                    .execute(
                        "INSERT INTO transfer_object_refs(object_key,blob_key,size,sha256,backend,updated_at,blob_generation)
                         VALUES(?,?,?,'00','d1','t',?)",
                        rusqlite::params![logical, blob, size, generation],
                    )
                    .unwrap();
            }
        }
        // FS-only file must NOT be included by list_sqlite_objects_page.
        std::fs::create_dir_all(dir.path().join("site/book")).unwrap();
        std::fs::write(dir.path().join("site/book/e.txt"), b"fs-only").unwrap();

        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = store.list_sqlite_objects_page(cursor.as_deref(), 2).unwrap();
            all.extend(page.objects.into_iter().map(|(k, s)| (k.to_string(), s)));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(
            all,
            vec![
                (".narou-transfer/.data/blob-b".to_string(), 6),
                (".narou-transfer/.data/blob-c".to_string(), 10),
                ("novels/site/book/a.txt".to_string(), 8),
                ("novels/site/book/b.txt".to_string(), 6),
                ("novels/site/book/c.txt".to_string(), 10),
                ("novels/site/book/d.txt".to_string(), 8),
            ]
        );
    }

    /// Ruby 版が書いた末尾空白付きキー (`本文/65 テスト .yaml`) だけが
    /// `native_logical_objects` に残っている小説でも、`.../本文/<index> *.yaml`
    /// のフォールバック解決で読めること。ミラー無効 (DB のみ) を固定する。
    #[test]
    fn section_reads_resolve_ruby_trailing_space_keys_without_mirror() {
        use crate::application::novel_access::NovelReadLease;
        use crate::downloader::persistence::PersistenceService;
        use crate::platform::NovelObjectKeys;

        let dir = tempfile::tempdir().unwrap();
        let mut conn = Connection::open_in_memory().unwrap();
        crate::native::sqlite::migrations::apply(&mut conn).unwrap();
        let mirror = NativeObjectStore::from_root(dir.path().to_path_buf()).unwrap();
        let store = SqliteObjectStore::with_mirror_files(Arc::new(Mutex::new(conn)), mirror, false);
        let objects: Arc<dyn ObjectStore> = Arc::new(store.clone());
        let keys = NovelObjectKeys::new("site", "title", false).unwrap();
        let raw_key = ObjectKey::try_new("novels/site/title/本文/65 テスト .yaml").unwrap();
        let canonical = keys.section("65", "テスト ");
        assert_ne!(raw_key, canonical);
        let section_yaml =
            "index: '65'\nhref: '65'\nsubtitle: 'テスト '\nfile_subtitle: 'テスト '\nsubdate: ''\nelement:\n  body: 末尾空白の本文\n";

        futures::executor::block_on(async {
            ObjectStore::write_small(&store, &raw_key, section_yaml.as_bytes().to_vec())
                .await
                .unwrap();
            assert!(
                !dir.path().join("site/title/本文").exists(),
                "all objects are DB-only"
            );

            // 正規キーの read_small も同じフォールバックで解決される。
            assert_eq!(
                ObjectStore::read_small(&store, &canonical).await.unwrap().unwrap(),
                section_yaml.as_bytes()
            );

            let service = PersistenceService::new(objects.clone());
            let subtitle = crate::downloader::SubtitleInfo {
                index: "65".into(),
                href: "65".into(),
                chapter: String::new(),
                subchapter: String::new(),
                subtitle: "テスト ".into(),
                file_subtitle: "テスト ".into(),
                subdate: String::new(),
                subupdate: None,
                download_time: None,
                download_revision: 0,
            };

            let mut lease = NovelReadLease::admit_for(&objects, &keys, "legacy-section").await.unwrap();
            let loaded = service.load_section(&mut lease, &keys, &subtitle).await.unwrap();
            assert_eq!(loaded.unwrap().element.body, "末尾空白の本文");

            let toc = crate::downloader::TocFile {
                title: "title".into(),
                author: "author".into(),
                toc_url: "https://example.test/".into(),
                story: None,
                subtitles: vec![subtitle],
                novel_type: None,
            };
            ObjectStore::write_small(
                &store,
                &keys.toc(),
                crate::downloader::persistence::serialize_toc(&toc).unwrap().into_bytes(),
            )
            .await
            .unwrap();
            let snapshot_lease =
                NovelReadLease::admit_for(&objects, &keys, "legacy-section-snapshot").await.unwrap();
            let mut stream = service.stream_sections_snapshot(snapshot_lease, keys.clone(), toc);
            let streamed = futures::StreamExt::next(&mut stream).await.unwrap().unwrap();
            assert_eq!(streamed.element.body, "末尾空白の本文");
            assert!(futures::StreamExt::next(&mut stream).await.is_none());
        });
    }

    /// `renew_reader` 分類が FS / Memory と一致することを固定する:
    /// close→open 後の stale トークンは `novel_recovering`、同一 epoch の
    /// 欠損行は `novel_lease_expired`。
    #[test]
    fn renew_reader_classifies_stale_epoch_and_expired_lease() {
        let (_dir, store) = temp_store();
        let prefix = ObjectPrefix::new("novels/site/book").unwrap();
        futures::executor::block_on(async {
            let stale = store.admit_reader(&prefix, "stale", 30).await.unwrap();
            let epoch = store.close_gate(&prefix, "transfer").await.unwrap();
            store.open_gate(&prefix, "transfer", epoch).await.unwrap();
            store.admit_reader(&prefix, "other", 30).await.unwrap();
            let error = store.renew_reader(&prefix, &stale, 30).await.unwrap_err();
            assert!(error.to_string().contains("novel_recovering"), "unexpected: {error}");

            let live = store.admit_reader(&prefix, "live", 30).await.unwrap();
            store.release_reader(&prefix, &live).await.unwrap();
            let error = store.renew_reader(&prefix, &live, 30).await.unwrap_err();
            assert!(error.to_string().contains("novel_lease_expired"), "unexpected: {error}");
        });
    }
}

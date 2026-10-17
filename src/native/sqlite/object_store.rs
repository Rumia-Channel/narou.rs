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
    NativeObjectStore, content_type_for_path, logical_key_for_native_path, run_blocking, run_scoped_blocking,
};

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
}

impl SqliteObjectStore {
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
        }
    }

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
        let mut statement = conn
            .prepare("SELECT size, content_type, updated_at FROM objects WHERE object_key = ?")
            .map_err(super::sqlite_error)?;
        let mut rows = statement
            .query(rusqlite::params![key.as_ref()])
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
                    content_type,
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
            for table in ["object_chunks", "objects"] {
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
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM objects WHERE object_key = ?",
            rusqlite::params![key.as_ref()],
        )
        .map_err(super::sqlite_error)?;
        Ok(())
    }

    /// Remove the filesystem copy of `key`; `Ok(())` when it is already gone.
    fn remove_file_copy(&self, key: &ObjectKey) -> Result<()> {
        let path = self.mirror.resolve_path(key)?;
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
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
        self.delete_blocking(key)
    }

    fn copy_blocking(&self, source: &ObjectKey, destination: &ObjectKey) -> Result<()> {
        match self.read_blocking(source, None)? {
            Some(data) => self.write_blocking(destination, &data),
            None => Err(NarouError::Platform(format!(
                "copy source not found: {source}"
            ))),
        }
    }

    /// 保存済みオブジェクトを小説ディレクトリへ実ファイルとして取り出す (同期)。
    ///
    /// 変換がファイルパスを要求するときに使う。既にあるファイルは触らない。
    /// ミラー無効時だけスコープに登録し、使い終わったら消す。ミラー有効時は
    /// 削除されたミラーの復元として残す。
    /// `skip_segment` をパス要素に持つキー (挿絵など) は対象外。
    pub(crate) fn materialize_into(
        &self,
        novel_dir: &Path,
        prefix: &ObjectKey,
        skip_segment: &str,
        scope: &crate::native::materialization::MaterializationScope,
    ) -> Result<Vec<PathBuf>> {
        let mut request = ObjectListRequest::new(
            ObjectPrefix::new(prefix.as_ref())?,
            std::num::NonZeroUsize::new(4096).expect("nonzero"),
        );
        let mut written = Vec::new();
        loop {
            let page = self.list_blocking(&request)?;
            for metadata in &page.objects {
                let key = metadata.key.as_ref();
                if key.split('/').any(|segment| segment == skip_segment) {
                    continue;
                }
                let relative = key
                    .strip_prefix(prefix.as_ref())
                    .map(|rest| rest.trim_start_matches('/'))
                    .unwrap_or_default();
                if relative.is_empty() {
                    continue;
                }
                let path = novel_dir.join(relative);
                if path.is_file() {
                    continue;
                }
                let Some(data) = self.read_blocking(&metadata.key, None)? else {
                    continue;
                };
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let parent = path.parent().ok_or_else(|| NarouError::Platform("materialized file has no parent".to_string()))?;
                let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
                std::io::Write::write_all(&mut temporary, &data)?;
                match temporary.persist_noclobber(&path) {
                    Ok(_) => {
                        if !self.mirror_files {
                            scope.register(&path);
                        }
                        written.push(path);
                    }
                    Err(error) if path.is_file() => drop(error),
                    Err(error) => return Err(error.error.into()),
                }
            }
            match page.next_cursor {
                Some(cursor) => request = request.after(cursor),
                None => break,
            }
        }
        Ok(written)
    }

    pub(crate) fn list_direct_files(&self, prefix: &ObjectKey) -> Result<Vec<ObjectKey>> {
        let start = format!("{prefix}/");
        let end = prefix_upper_bound(&start);
        let mut keys = Vec::new();
        {
            let conn = self.conn.lock();
            let mut statement = conn.prepare(
                "SELECT object_key FROM objects
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
        // Restrict the scan to the prefix's primary-key range (plus the exact
        // key) instead of reading the whole table for every page; the
        // `ObjectPrefix::matches` check below stays as the authority.
        let base = request.prefix.as_ref().trim_end_matches('/').to_string();
        let lower = format!("{base}/");
        let upper = prefix_upper_bound(&lower);
        {
            let conn = self.conn.lock();
            let mut statement = conn
                .prepare(
                    "SELECT object_key, size, content_type, updated_at FROM objects
                     WHERE ?1 = '' OR object_key = ?1 OR (object_key >= ?2 AND object_key < ?3)
                     ORDER BY object_key",
                )
                .map_err(super::sqlite_error)?;
            let mut rows = statement
                .query(rusqlite::params![base, lower, upper])
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
        let prefix_dir = self.mirror.resolve_prefix_dir(request.prefix.as_ref())?;
        if prefix_dir.is_dir() {
            let mut paths = Vec::new();
            NativeObjectStore::recursive_files(self.mirror.root(), &prefix_dir, &mut paths)?;
            for path in paths {
                let Some(key) = logical_key_for_native_path(self.mirror.root(), &path) else {
                    continue;
                };
                if !request.prefix.matches(&key) || merged.contains_key(key.as_ref()) {
                    continue;
                }
                if let Some(metadata) = self.mirror.metadata_for_path(&key, &path)? {
                    merged.insert(key.as_ref().to_string(), metadata);
                }
            }
        }
        let objects: Vec<ObjectMetadata> = merged.into_values().collect();
        let (page, next_cursor) = crate::platform::paginate_object_listing(
            objects,
            request.cursor.as_deref(),
            request.limit.get(),
            |item| item.key.as_ref(),
        );
        Ok(ObjectListPage {
            objects: page,
            next_cursor,
        })
    }
}

struct StoredPayload {
    data: Vec<u8>,
    encoding: ObjectEncoding,
    crc32: u32,
    size: u64,
}

fn read_stored_payload(
    conn: &Connection,
    key: &ObjectKey,
    limit: Option<u64>,
) -> Result<Option<StoredPayload>> {
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
        let this = self.clone();
        let key = key.clone();
        run_blocking(move || {
            if data.len() as u64 > crate::native::object_store::MAX_SMALL_OBJECT_BYTES {
                return Err(NarouError::Platform(format!(
                    "small object exceeds {} bytes: {key}",
                    crate::native::object_store::MAX_SMALL_OBJECT_BYTES
                )));
            }
            this.write_blocking(&key, &data)?;
            if this.mirror_files {
                this.mirror.write_bytes_blocking(&key, &data)?;
            }
            Ok(())
        })
    }

    fn update_small<'a, 'data: 'a>(
        &'a self,
        key: &'a ObjectKey,
        mutate: &'a mut SmallObjectMutator<'data>,
    ) -> PlatformFuture<'a, Result<ObjectUpdateResult>> {
        Box::pin(async move {
            run_scoped_blocking(|| self.update_small_blocking(key, mutate))
        })
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let key = key.clone();
        run_blocking(move || this.delete_both(&key))
    }

    fn delete_prefix<'a>(&'a self, prefix: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let prefix = prefix.clone();
        run_blocking(move || this.delete_prefix_blocking(&prefix))
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
        let this = self.clone();
        let key = key.clone();
        Box::pin(async move {
            let data = run_blocking({
                let this = this.clone();
                let key = key.clone();
                move || -> Result<Option<Vec<u8>>> {
                    match this.read_blocking(&key, None)? {
                        Some(data) => Ok(Some(data)),
                        None => {
                            let path = this.mirror.resolve_read_path(&key)?;
                            match std::fs::read(&path) {
                                Ok(data) => Ok(Some(data)),
                                Err(error)
                                    if error.kind() == std::io::ErrorKind::NotFound =>
                                {
                                    Ok(None)
                                }
                                Err(error) => Err(error.into()),
                            }
                        }
                    }
                }
            })
            .await?;
            let Some(data) = data else {
                return Ok(None);
            };
            let stream: AssetStream = Box::pin(futures::stream::iter(
                data.chunks(64 * 1024)
                    .map(|chunk| Ok(chunk.to_vec()))
                    .collect::<Vec<_>>(),
            ));
            Ok(Some(stream))
        })
    }

    fn write_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
        mut stream: AssetStream,
    ) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let key = key.clone();
        Box::pin(async move {
            let mut data = Vec::new();
            while let Some(chunk) = stream.next().await {
                data.extend_from_slice(&chunk?);
            }
            run_blocking(move || {
                this.write_blocking(&key, &data)?;
                if this.mirror_files {
                    this.mirror.write_bytes_blocking(&key, &data)?;
                }
                Ok(())
            })
            .await
        })
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        ObjectStore::delete(self, key)
    }

    fn copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let source = source.clone();
        let destination = destination.clone();
        run_blocking(move || {
            this.copy_blocking(&source, &destination)?;
            if !this.mirror_files {
                return Ok(());
            }
            let source_path = this.mirror.resolve_path(&source)?;
            let destination_path = this.mirror.resolve_path(&destination)?;
            if source_path.exists() {
                this.mirror.ensure_parent_dir(&destination_path)?;
                std::fs::copy(&source_path, &destination_path)?;
            }
            Ok(())
        })
    }

    fn move_or_copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let source = source.clone();
        let destination = destination.clone();
        run_blocking(move || {
            this.copy_blocking(&source, &destination)?;
            // The moved-away source must stop being readable from either copy;
            // with the mirror on the file itself moves (falling back to
            // copy+remove), without it a stale file is simply removed.
            let source_path = this.mirror.resolve_path(&source)?;
            if this.mirror_files && source_path.is_file() {
                let destination_path = this.mirror.resolve_path(&destination)?;
                this.mirror.ensure_parent_dir(&destination_path)?;
                if std::fs::rename(&source_path, &destination_path).is_err() {
                    std::fs::copy(&source_path, &destination_path)?;
                    this.remove_file_copy(&source)?;
                }
            } else {
                this.remove_file_copy(&source)?;
            }
            this.delete_blocking(&source)
        })
    }
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

/// Preserve only empty version directories in the recognized novel cache
/// layout, not arbitrary empty folders. Existing marker files are ordinary
/// files and retain their payloads through the importer.
fn collect_archive_import_entries(
    root: &Path,
    current: &Path,
    output: &mut Vec<ArchiveImportEntry>,
) -> Result<()> {
    let mut has_entries = false;
    for entry in std::fs::read_dir(current)? {
        let path = crate::db::paths::ensure_within_archive_root(&entry?.path(), root)?;
        has_entries = true;
        if path.is_dir() {
            collect_archive_import_entries(root, &path, output)?;
        } else if path.is_file() {
            output.push(ArchiveImportEntry::File(path));
        }
    }
    if !has_entries {
        let novel_dir = current.parent().and_then(Path::parent).and_then(Path::parent);
        if let Some(novel_dir) = novel_dir
            && let (Some(prefix), Some(marker)) = (
                logical_key_for_native_path(root, novel_dir),
                logical_key_for_native_path(root, &current.join(".version")),
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
        collect_archive_import_entries(&root, &root, &mut entries)?;
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
            let mut stored = Vec::new();
            while let Some((chunk_key, data)) = chunk.take() {
                if chunk_key.as_str() < meta_key.as_str() {
                    // Leftover chunks without an object row: ignored, as before.
                    chunk = next_verify_chunk(&mut chunk_rows)?;
                    continue;
                }
                if chunk_key == meta_key {
                    stored.extend_from_slice(&data);
                    chunk = next_verify_chunk(&mut chunk_rows)?;
                    continue;
                }
                // Belongs to the next object; keep it for the next round.
                chunk = Some((chunk_key, data));
                break;
            }
            if let Err(reason) = verify_stored_payload(&stored, &encoding, crc32) {
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::ObjectPrefix;
    use std::num::NonZeroUsize;

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
            let versions = read_diff_cache_catalog(&store, keys.prefix(), &[]).await.unwrap();
            assert_eq!(versions.len(), 2);
            assert_eq!(versions[0].entries, vec![("2".to_string(), "保存済み".to_string())]);
            assert!(!versions[0].only_latest_update);
            assert!(versions[1].only_latest_update);

            store.conn_handle().lock().execute(
                "UPDATE objects SET crc32 = crc32 + 1 WHERE object_key = ?",
                [cached.as_ref()],
            ).unwrap();
            assert!(store.read_small(&cached).await.is_err(), "exercise a real stored-payload read failure");
            let corrupt = read_diff_cache_catalog(&store, keys.prefix(), &[]).await.unwrap();
            assert!(corrupt[0].entries.is_empty());
            assert!(!corrupt[0].only_latest_update, "failed I/O cannot become an empty version");
            for key in plan_diff_cache_clean(&store, keys.prefix()).await.unwrap() {
                ObjectStore::delete(&store, &key).await.unwrap();
            }
            assert!(read_diff_cache_catalog(&store, keys.prefix(), &[]).await.unwrap().is_empty());
            assert_eq!(store.read_small(&keys.section("2", "現在")).await.unwrap().unwrap(), b"current");
            assert!(!dir.path().join("site/title").exists(), "catalog/clean must not materialize DB-only files");
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

        // 必要なときだけ実体化し、返したパスを消せば元に戻る。
        let prefix = ObjectKey::try_new("novels/site/title").unwrap();
        let novel_dir = dir.path().join("site/title");
        let scope = crate::native::materialization::MaterializationScope::acquire(dir.path(), &novel_dir).unwrap();
        let written = store.materialize_into(&novel_dir, &prefix, "挿絵", &scope).unwrap();
        assert_eq!(written, vec![mirrored.clone()]);
        assert!(mirrored.is_file());
        drop(scope);
        assert!(!mirrored.exists());
    }

    #[test]
    fn materialize_reads_past_a_full_page_of_skipped_illustrations() {
        let (dir, mut store) = temp_store();
        store.mirror_files = false;
        let prefix = ObjectKey::try_new("novels/site/title").unwrap();
        // Illustrations sort before 本文 and fill the entire first page.
        for index in 0..4096 {
            let key = prefix
                .join("挿絵")
                .unwrap()
                .join(format!("{index:04}.png"))
                .unwrap();
            store.write_blocking(&key, b"image").unwrap();
        }
        let body = prefix.join("本文").unwrap().join("1 body.yaml").unwrap();
        store.write_blocking(&body, b"body").unwrap();
        let existing = prefix.join("本文").unwrap().join("2 existing.yaml").unwrap();
        store.write_blocking(&existing, b"stored").unwrap();
        let novel_dir = dir.path().join("site/title");
        let existing_path = novel_dir.join("本文/2 existing.yaml");
        std::fs::create_dir_all(existing_path.parent().unwrap()).unwrap();
        std::fs::write(&existing_path, b"keep local").unwrap();

        let scope = crate::native::materialization::MaterializationScope::acquire(dir.path(), &novel_dir).unwrap();
        let written = store.materialize_into(&novel_dir, &prefix, "挿絵", &scope).unwrap();
        let body_path = novel_dir.join("本文/1 body.yaml");
        assert_eq!(written, vec![body_path.clone()]);
        assert_eq!(std::fs::read(&body_path).unwrap(), b"body");
        assert!(!novel_dir.join("挿絵").exists());
        let guard = crate::native::object_store::MaterializedNovelFiles::from_paths(written, scope);
        drop(guard);
        assert!(!body_path.exists());
        assert_eq!(std::fs::read(&existing_path).unwrap(), b"keep local");
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
}

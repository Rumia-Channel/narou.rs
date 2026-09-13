//! SQLite-backed ObjectStore/AssetStore for `storage-backend = sqlite`.
//!
//! The `objects`/`object_chunks` tables are authoritative for reads and
//! listings; every mutation is mirrored to the native filesystem layout so a
//! rollback to YAML mode (or narou.rb) sees the same bytes without an export
//! step. Reads fall back to the filesystem for files written before the
//! one-shot import or by paths that bypass the store (e.g. the converter's
//! legacy illustration directory handling).

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use rusqlite::Connection;

use crate::error::{NarouError, Result};
use crate::platform::{
    AssetStore, AssetStream, ObjectKey, ObjectListPage, ObjectListRequest, ObjectMetadata,
    ObjectStore, PlatformFuture,
};

use crate::native::object_store::{
    NativeObjectStore, content_type_for_path, logical_key_for_native_path, run_blocking,
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
}

impl SqliteObjectStore {
    pub fn new(conn: Arc<Mutex<Connection>>, mirror: NativeObjectStore) -> Self {
        Self { conn, mirror }
    }

    fn now_rfc3339() -> String {
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
    }

    fn stat_blocking(&self, key: &ObjectKey) -> Result<Option<ObjectMetadata>> {
        let conn = self.conn.lock().expect("sqlite mutex poisoned");
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
    fn read_blocking(&self, key: &ObjectKey) -> Result<Option<Vec<u8>>> {
        let conn = self.conn.lock().expect("sqlite mutex poisoned");
        let mut statement = conn
            .prepare("SELECT data, size, encoding, crc32 FROM objects WHERE object_key = ?")
            .map_err(super::sqlite_error)?;
        let mut rows = statement
            .query(rusqlite::params![key.as_ref()])
            .map_err(super::sqlite_error)?;
        let Some(row) = rows.next().map_err(super::sqlite_error)? else {
            return Ok(None);
        };
        let inline: Option<Vec<u8>> = row.get(0).map_err(super::sqlite_error)?;
        let size: i64 = row.get(1).map_err(super::sqlite_error)?;
        let encoding: String = row.get(2).map_err(super::sqlite_error)?;
        let crc32: i64 = row.get(3).map_err(super::sqlite_error)?;
        let encoding = crate::platform::ObjectEncoding::parse(&encoding)?;
        let stored = if let Some(bytes) = inline {
            bytes
        } else {
            drop(rows);
            drop(statement);
            let mut statement = conn
                .prepare("SELECT data FROM object_chunks WHERE object_key = ? ORDER BY seq")
                .map_err(super::sqlite_error)?;
            let mut rows = statement
                .query(rusqlite::params![key.as_ref()])
                .map_err(super::sqlite_error)?;
            let mut stored = Vec::new();
            while let Some(row) = rows.next().map_err(super::sqlite_error)? {
                let chunk: Vec<u8> = row.get(0).map_err(super::sqlite_error)?;
                stored.extend_from_slice(&chunk);
            }
            stored
        };
        let data = crate::platform::decompress_object_payload(&stored, encoding)?;
        crate::platform::verify_object_crc32(&data, crc32 as u32)?;
        debug_assert_eq!(data.len() as i64, size);
        Ok(Some(data))
    }

    /// Insert or replace one object (inline or chunked) in a single
    /// transaction. Compression runs on the whole payload before chunking so
    /// chunk boundaries never split a brotli stream; `size`/`crc32` describe
    /// the uncompressed payload.
    fn write_blocking(&self, key: &ObjectKey, data: &[u8]) -> Result<()> {
        let (stored, encoding) = crate::platform::compress_object_payload(data);
        let crc32 = crate::platform::object_crc32(data) as i64;
        let mut conn = self.conn.lock().expect("sqlite mutex poisoned");
        let tx = conn.transaction().map_err(super::sqlite_error)?;
        let content_type = content_type_for_key(key);
        if stored.len() <= OBJECT_CHUNK_BYTES {
            tx.execute(
                "INSERT INTO objects (object_key, size, content_type, updated_at, encoding, crc32, data)
                 VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(object_key) DO UPDATE SET
                     size = excluded.size,
                     content_type = excluded.content_type,
                     updated_at = excluded.updated_at,
                     encoding = excluded.encoding,
                     crc32 = excluded.crc32,
                     data = excluded.data",
                rusqlite::params![
                    key.as_ref(),
                    data.len() as i64,
                    content_type,
                    Self::now_rfc3339(),
                    encoding.as_str(),
                    crc32,
                    stored
                ],
            )
            .map_err(super::sqlite_error)?;
            tx.execute(
                "DELETE FROM object_chunks WHERE object_key = ?",
                rusqlite::params![key.as_ref()],
            )
            .map_err(super::sqlite_error)?;
        } else {
            tx.execute(
                "INSERT INTO objects (object_key, size, content_type, updated_at, encoding, crc32, data)
                 VALUES (?, ?, ?, ?, ?, ?, NULL)
                 ON CONFLICT(object_key) DO UPDATE SET
                     size = excluded.size,
                     content_type = excluded.content_type,
                     updated_at = excluded.updated_at,
                     encoding = excluded.encoding,
                     crc32 = excluded.crc32,
                     data = NULL",
                rusqlite::params![
                    key.as_ref(),
                    data.len() as i64,
                    content_type,
                    Self::now_rfc3339(),
                    encoding.as_str(),
                    crc32
                ],
            )
            .map_err(super::sqlite_error)?;
            tx.execute(
                "DELETE FROM object_chunks WHERE object_key = ?",
                rusqlite::params![key.as_ref()],
            )
            .map_err(super::sqlite_error)?;
            let mut statement = tx
                .prepare("INSERT INTO object_chunks (object_key, seq, data) VALUES (?, ?, ?)")
                .map_err(super::sqlite_error)?;
            for (seq, chunk) in stored.chunks(OBJECT_CHUNK_BYTES).enumerate() {
                statement
                    .execute(rusqlite::params![key.as_ref(), seq as i64, chunk])
                    .map_err(super::sqlite_error)?;
            }
        }
        tx.commit().map_err(super::sqlite_error)
    }

    fn delete_blocking(&self, key: &ObjectKey) -> Result<()> {
        let conn = self.conn.lock().expect("sqlite mutex poisoned");
        conn.execute(
            "DELETE FROM objects WHERE object_key = ?",
            rusqlite::params![key.as_ref()],
        )
        .map_err(super::sqlite_error)?;
        Ok(())
    }

    fn copy_blocking(&self, source: &ObjectKey, destination: &ObjectKey) -> Result<()> {
        match self.read_blocking(source)? {
            Some(data) => self.write_blocking(destination, &data),
            None => Err(NarouError::Platform(format!(
                "copy source not found: {source}"
            ))),
        }
    }

    /// Merge DB metadata with filesystem entries not yet imported. DB rows
    /// win on conflict; the union keeps listings correct for files written
    /// before the import or through paths that bypass the store.
    fn list_blocking(&self, request: &ObjectListRequest) -> Result<ObjectListPage> {
        let mut merged: BTreeMap<String, ObjectMetadata> = BTreeMap::new();
        {
            let conn = self.conn.lock().expect("sqlite mutex poisoned");
            let mut statement = conn
                .prepare(
                    "SELECT object_key, size, content_type, updated_at FROM objects ORDER BY object_key",
                )
                .map_err(super::sqlite_error)?;
            let mut rows = statement.query([]).map_err(super::sqlite_error)?;
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
        // Filesystem union: only for keys the DB does not already know.
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
        let start = request
            .cursor
            .as_deref()
            .map(|cursor| {
                objects
                    .iter()
                    .position(|item| item.key.as_ref() > cursor)
                    .unwrap_or(objects.len())
            })
            .unwrap_or(0);
        let end = (start + request.limit.get()).min(objects.len());
        let page = objects[start.min(objects.len())..end].to_vec();
        let next_cursor = if end < objects.len() {
            page.last().map(|item| item.key.as_ref().to_string())
        } else {
            None
        };
        Ok(ObjectListPage {
            objects: page,
            next_cursor,
        })
    }
}

fn content_type_for_key(key: &ObjectKey) -> Option<String> {
    // Reuse the extension-based mapping; the key's last component carries the
    // same extension the mirrored file would have.
    let name = key.as_ref().rsplit('/').next().unwrap_or_default();
    content_type_for_path(std::path::Path::new(name))
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
            match this.read_blocking(&key)? {
                Some(data) => {
                    if data.len() as u64 > crate::native::object_store::MAX_SMALL_OBJECT_BYTES {
                        return Err(NarouError::Platform(format!(
                            "small object exceeds {} bytes: {key}",
                            crate::native::object_store::MAX_SMALL_OBJECT_BYTES
                        )));
                    }
                    Ok(Some(data))
                }
                None => {
                    let path = this.mirror.resolve_read_path(&key)?;
                    match std::fs::read(&path) {
                        Ok(data) => Ok(Some(data)),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                        Err(error) => Err(error.into()),
                    }
                }
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
            this.mirror.write_bytes_blocking(&key, &data)
        })
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        let this = self.clone();
        let key = key.clone();
        run_blocking(move || {
            this.delete_blocking(&key)?;
            let path = this.mirror.resolve_path(&key)?;
            match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            }
        })
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
                    match this.read_blocking(&key)? {
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
                this.mirror.write_bytes_blocking(&key, &data)
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
            this.delete_blocking(&source)?;
            let source_path = this.mirror.resolve_path(&source)?;
            let destination_path = this.mirror.resolve_path(&destination)?;
            if source_path.exists() {
                this.mirror.ensure_parent_dir(&destination_path)?;
                std::fs::rename(&source_path, &destination_path)?;
            }
            Ok(())
        })
    }
}

/// Import every file under the archive root into `objects`/`object_chunks`.
/// Runs once when the table is empty; `INSERT OR REPLACE` keeps a partial
/// import resumable on the next start.
pub(crate) fn import_archive_into_objects(
    conn: &Arc<Mutex<Connection>>,
    mirror: &NativeObjectStore,
) -> Result<usize> {
    let root = mirror.root().to_path_buf();
    let mut paths = Vec::new();
    if root.is_dir() {
        NativeObjectStore::recursive_files(&root, &root, &mut paths)?;
    }
    if paths.is_empty() {
        return Ok(0);
    }
    let store = SqliteObjectStore::new(conn.clone(), mirror.clone());
    let mut imported = 0usize;
    for path in paths {
        let Some(key) = logical_key_for_native_path(&root, &path) else {
            continue;
        };
        let data = std::fs::read(&path)?;
        store.write_blocking(&key, &data)?;
        imported += 1;
    }
    Ok(imported)
}

/// Move legacy `body_yaml` payloads into the content-addressed
/// `section_bodies` table and link rows via `body_hash`. Runs inside
/// `migrations::apply` between 0009 (adds the columns) and 0010 (drops
/// `body_yaml`). Idempotent: rows already carrying a hash are skipped, and
/// `INSERT OR IGNORE` keeps reruns cheap.
pub(crate) fn migrate_section_bodies(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction().map_err(super::sqlite_error)?;
    // Both tables are WITHOUT ROWID: identify rows by their primary key.
    for (table, pk1, pk2) in [
        ("novel_sections", "novel_id", "idx"),
        ("novel_version_sections", "version_id", "idx"),
    ] {
        let select = format!("SELECT {pk1}, {pk2}, body_yaml FROM {table} WHERE body_hash IS NULL");
        let update = format!("UPDATE {table} SET body_hash = ? WHERE {pk1} = ? AND {pk2} = ?");
        let mut statement = tx.prepare(&select).map_err(super::sqlite_error)?;
        let rows: Vec<(i64, String, String)> = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(super::sqlite_error)?
            .collect::<std::result::Result<_, _>>()
            .map_err(super::sqlite_error)?;
        drop(statement);
        for (pk1, pk2, body) in rows {
            let hash = super::content::store_body(&tx, &body)?;
            tx.execute(&update, rusqlite::params![hash, pk1, pk2])
                .map_err(super::sqlite_error)?;
        }
    }
    tx.commit().map_err(super::sqlite_error)
}


/// Verify every stored payload against its CRC-32 after decompression.
/// Returns the number of corrupted rows; used by `narou db verify`.
/// Skips silently when the encoding/crc32 columns predate the schema.
pub(crate) fn verify_payload_crc32(conn: &Connection) -> Result<usize> {
    let has_encoding: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('objects') WHERE name = 'encoding'")
        .and_then(|mut s| s.exists([]))
        .unwrap_or(false);
    if !has_encoding {
        return Ok(0);
    }
    let mut bad = 0usize;
    // objects: inline or chunked payloads.
    {
        let mut statement = conn
            .prepare("SELECT object_key, data, encoding, crc32 FROM objects")
            .map_err(super::sqlite_error)?;
        let rows: Vec<(String, Option<Vec<u8>>, String, i64)> = statement
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .map_err(super::sqlite_error)?
            .collect::<std::result::Result<_, _>>()
            .map_err(super::sqlite_error)?;
        drop(statement);
        for (key, inline, encoding, crc32) in rows {
            let stored = match inline {
                Some(bytes) => bytes,
                None => {
                    let mut stored = Vec::new();
                    let mut chunks = conn
                        .prepare("SELECT data FROM object_chunks WHERE object_key = ? ORDER BY seq")
                        .map_err(super::sqlite_error)?;
                    let mut chunk_rows =
                        chunks.query(rusqlite::params![key]).map_err(super::sqlite_error)?;
                    while let Some(row) = chunk_rows.next().map_err(super::sqlite_error)? {
                        let chunk: Vec<u8> = row.get(0).map_err(super::sqlite_error)?;
                        stored.extend_from_slice(&chunk);
                    }
                    stored
                }
            };
            let ok = crate::platform::ObjectEncoding::parse(&encoding)
                .and_then(|enc| crate::platform::decompress_object_payload(&stored, enc))
                .and_then(|data| crate::platform::verify_object_crc32(&data, crc32 as u32))
                .is_ok();
            if !ok {
                bad += 1;
            }
        }
    }
    // section_bodies: always inline.
    {
        let mut statement = conn
            .prepare("SELECT data, encoding, crc32 FROM section_bodies")
            .map_err(super::sqlite_error)?;
        let rows: Vec<(Vec<u8>, String, i64)> = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(super::sqlite_error)?
            .collect::<std::result::Result<_, _>>()
            .map_err(super::sqlite_error)?;
        for (stored, encoding, crc32) in rows {
            let ok = crate::platform::ObjectEncoding::parse(&encoding)
                .and_then(|enc| crate::platform::decompress_object_payload(&stored, enc))
                .and_then(|data| crate::platform::verify_object_crc32(&data, crc32 as u32))
                .is_ok();
            if !ok {
                bad += 1;
            }
        }
    }
    Ok(bad)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::ObjectPrefix;
    use std::num::NonZeroUsize;

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

        let conn = store.conn.lock().expect("sqlite mutex poisoned");
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
}

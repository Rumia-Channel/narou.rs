//! D1-backed ObjectStore/AssetStore for the Worker runtime.
//!
//! Payloads use BLOB columns. Values up to 512 KiB live inline; larger stored
//! payloads use ordered chunk rows. Native and Worker share the same codecs,
//! uncompressed sizes and CRC semantics.
//!
//! Payloads up to 512 KiB live inline in `objects.data`; larger payloads set
//! `objects.data = NULL` and store all bytes in `object_chunks` (seq 0..N,
//! 512 KiB each), matching the native `SqliteObjectStore` layout.



use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{
    AssetStore, AssetStream, ObjectKey, ObjectListPage, ObjectListRequest, ObjectMetadata,
    ObjectStore, PlatformFuture, content_type_for_key, paginate_object_listing,
    ObjectUpdateResult, SmallObjectMutator,
};
use serde::Deserialize;
use worker::{D1PreparedStatement, wasm_bindgen::{JsCast, JsValue}};

use crate::db_handle::DbHandle;

pub(crate) mod update;

fn blob_view(value: &JsValue) -> worker::js_sys::Uint8Array {
    value.dyn_ref::<worker::js_sys::Uint8Array>().cloned()
        .unwrap_or_else(|| worker::js_sys::Uint8Array::new(value))
}

fn blob_binding(value: &JsValue) -> JsValue {
    if value.is_null() || value.is_undefined() { JsValue::NULL }
    else { JsValue::from(blob_view(value)) }
}

fn bounded_blob_bytes(value: &JsValue, limit: usize) -> Result<Vec<u8>> {
    if value.is_null() || value.is_undefined() { return Ok(Vec::new()); }
    let bytes = blob_view(value);
    if bytes.length() as usize > limit {
        return Err(NarouError::Platform("stored object exceeds the read cap".into()));
    }
    Ok(bytes.to_vec())
}

#[derive(Deserialize)]
struct GuardCommit {
    committed: i64,
}
/// Payloads at or below this size are stored inline in `objects.data`.
const INLINE_MAX: usize = 512 * 1024;
/// Chunk size for payloads above `INLINE_MAX`.
const CHUNK_SIZE: usize = 512 * 1024;
/// `read_small`/`write_small` cap, matching the native store's contract.
const SMALL_CAP: u64 = 16 * 1024 * 1024;

fn worker_error(error: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(format!("Worker storage error: {error}"))
}

fn js_value_to_bytes(value: &JsValue) -> Result<Vec<u8>> {
    if value.is_null() || value.is_undefined() {
        return Ok(Vec::new());
    }
    // D1 returns BLOB columns as ArrayBuffer; Uint8Array::new accepts both
    // ArrayBuffer and typed-array views.
    Ok(blob_view(value).to_vec())
}

fn js_value_to_string(value: &JsValue) -> Option<String> {
    value.as_string()
}

fn js_value_to_i64(value: &JsValue) -> i64 {
    value.as_f64().map(|v| v as i64).unwrap_or(0)
}

#[derive(Debug, Deserialize)]
struct StatRow {
    size: i64,
    updated_at: String,
    content_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListRow {
    object_key: String,
    size: i64,
    updated_at: String,
    content_type: Option<String>,
}


fn metadata_from(key: &ObjectKey, size: i64, updated_at: &str, content_type: Option<String>) -> ObjectMetadata {
    ObjectMetadata {
        key: key.clone(),
        size: size.max(0) as u64,
        etag: None,
        content_type,
        last_modified: chrono::DateTime::parse_from_rfc3339(updated_at)
            .map(|value| value.with_timezone(&chrono::Utc))
            .ok(),
    }
}

#[derive(Clone)]
pub struct D1ObjectStore {
    db: DbHandle,
}

impl D1ObjectStore {
    pub fn new(db: DbHandle) -> Self {
        Self { db }
    }

    fn prepare(&self, sql: &str, values: Vec<JsValue>) -> Result<D1PreparedStatement> {
        self.db.prepare(sql).bind(&values).map_err(worker_error)
    }

    async fn read_snapshot(&self, key: &ObjectKey) -> Result<(update::Snapshot, Vec<JsValue>)> {
        let rows = self.prepare(update::READ_SQL, vec![JsValue::from_str(key.as_ref())])?
            .raw_js_value().await.map_err(worker_error)?;
        let mut snapshot = update::Snapshot::default();
        let mut stored_length = 0usize;
        for (index, row) in rows.iter().enumerate() {
            let columns = row.dyn_ref::<worker::js_sys::Array>()
                .ok_or_else(|| worker_error("invalid object snapshot row"))?;
            if index == 0 {
                let size = js_value_to_i64(&columns.get(0));
                if size < 0 || size as u64 > SMALL_CAP {
                    return Err(NarouError::Platform("object exceeds the small-read cap".into()));
                }
                let inline = columns.get(5);
                snapshot.header = Some(update::Header {
                    size,
                    updated_at: js_value_to_string(&columns.get(1)).unwrap_or_default(),
                    content_type: js_value_to_string(&columns.get(2)),
                    encoding: js_value_to_string(&columns.get(3)).unwrap_or_else(|| "none".into()),
                    crc32: js_value_to_i64(&columns.get(4)),
                    data: if inline.is_null() || inline.is_undefined() { None }
                        else { Some(bounded_blob_bytes(&inline, SMALL_CAP as usize)?) },
                });
            }
            let sequence = columns.get(6);
            if !sequence.is_null() && !sequence.is_undefined() {
                let data = bounded_blob_bytes(&columns.get(7), SMALL_CAP as usize - stored_length)?;
                stored_length += data.len();
                snapshot.chunks.push(update::Chunk { seq: js_value_to_i64(&sequence), data });
            }
        }
        Ok((snapshot, rows))
    }

    fn append_write_statements(
        &self, key: &ObjectKey, data: &[u8], guard: Option<&str>,
        statements: &mut Vec<D1PreparedStatement>,
    ) -> Result<()> {
        let (stored, encoding) = narou_rs::platform::encode_object_payload(data);
        let now = chrono::Utc::now().to_rfc3339();
        let content_type = content_type_for_key(key);
        let inline = stored.len() <= INLINE_MAX;
        let mut values = Vec::with_capacity(8);
        values.extend([
            JsValue::from_str(key.as_ref()),
            JsValue::from_f64(data.len() as f64),
            JsValue::from_str(&now),
            content_type.as_deref().map(JsValue::from_str).unwrap_or(JsValue::NULL),
            JsValue::from_str(encoding.as_str()),
            JsValue::from_f64(narou_rs::platform::object_crc32(data) as f64),
        ]);
        if inline { values.push(JsValue::from(worker::js_sys::Uint8Array::from(stored.as_ref()))); }
        if let Some(guard) = guard { values.push(JsValue::from_str(guard)); }
        let sql = match (inline, guard.is_some()) {
            (true, true) => update::WRITE_INLINE_SQL,
            (false, true) => update::WRITE_CHUNKED_SQL,
            (true, false) =>
                "INSERT INTO objects(object_key, size, updated_at, content_type, encoding, crc32, data)
                 VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(object_key) DO UPDATE SET size = excluded.size, updated_at = excluded.updated_at,
                   content_type = excluded.content_type, encoding = excluded.encoding,
                   crc32 = excluded.crc32, data = excluded.data",
            (false, false) =>
                "INSERT INTO objects(object_key, size, updated_at, content_type, encoding, crc32, data)
                 VALUES (?, ?, ?, ?, ?, ?, NULL)
                 ON CONFLICT(object_key) DO UPDATE SET size = excluded.size, updated_at = excluded.updated_at,
                   content_type = excluded.content_type, encoding = excluded.encoding,
                   crc32 = excluded.crc32, data = NULL",
        };
        statements.push(self.prepare(sql, values)?);
        let mut values = Vec::with_capacity(2);
        values.push(JsValue::from_str(key.as_ref()));
        if let Some(guard) = guard { values.push(JsValue::from_str(guard)); }
        statements.push(self.prepare(
            if guard.is_some() { update::DELETE_CHUNKS_SQL }
                else { "DELETE FROM object_chunks WHERE object_key = ?" },
            values,
        )?);
        if !inline {
            for (seq, chunk) in stored.chunks(CHUNK_SIZE).enumerate() {
                let mut values = Vec::with_capacity(4);
                values.extend([
                    JsValue::from_str(key.as_ref()),
                    JsValue::from_f64(seq as f64),
                    JsValue::from(worker::js_sys::Uint8Array::from(chunk)),
                ]);
                if let Some(guard) = guard { values.push(JsValue::from_str(guard)); }
                statements.push(self.prepare(
                    if guard.is_some() { update::WRITE_CHUNK_SQL }
                        else { "INSERT INTO object_chunks(object_key, seq, data) VALUES (?, ?, ?)" },
                    values,
                )?);
            }
        }
        Ok(())
    }

    async fn update_control<'data>(&self, key: &ObjectKey, mutate: &mut SmallObjectMutator<'data>) -> Result<ObjectUpdateResult> {
        let primary = Self::new(self.db.primary_view());
        let guard = format!("object_write_guard/{}", key.as_ref());
        for _ in 0..4 {
            let (snapshot, rows) = primary.read_snapshot(key).await?;
            let current = snapshot.payload(SMALL_CAP as usize)?;
            let next = mutate(current.as_deref())?;
            drop(current);
            if next.as_ref().is_some_and(|data| data.len() as u64 > SMALL_CAP) {
                return Err(NarouError::Platform("object exceeds the small-write cap".into()));
            }
            let mut values = Vec::with_capacity(9);
            values.extend([
                JsValue::from_str(&guard), JsValue::from_str(key.as_ref()),
                JsValue::from_f64(if snapshot.header.is_some() { 1.0 } else { 0.0 }),
            ]);
            if let Some(header) = &snapshot.header {
                let columns = rows[0].unchecked_ref::<worker::js_sys::Array>();
                values.extend([
                    JsValue::from_f64(header.size as f64),
                    JsValue::from_str(&header.updated_at),
                    header.content_type.as_deref().map(JsValue::from_str).unwrap_or(JsValue::NULL),
                    JsValue::from_str(&header.encoding),
                    JsValue::from_f64(header.crc32 as f64),
                    blob_binding(&columns.get(5)),
                ]);
            } else {
                values.extend((0..6).map(|_| JsValue::NULL));
            }
            let write_capacity = next.as_ref().map_or(0, |data| 2 + data.len().div_ceil(CHUNK_SIZE));
            let mut statements = Vec::with_capacity(4 + snapshot.chunks.len() + write_capacity);
            statements.push(primary.prepare(update::RESET_SQL, vec![JsValue::from_str(&guard)])?);
            statements.push(primary.prepare(update::AUTHORIZE_SQL, values)?);
            statements.push(primary.prepare(update::CHECK_CHUNK_COUNT_SQL, vec![
                JsValue::from_str(&guard), JsValue::from_str(key.as_ref()),
                JsValue::from_f64(snapshot.chunks.len() as f64),
            ])?);
            for (chunk, row) in snapshot.chunks.iter().zip(&rows) {
                let columns = row.unchecked_ref::<worker::js_sys::Array>();
                statements.push(primary.prepare(update::CHECK_CHUNK_SQL, vec![
                    JsValue::from_str(&guard), JsValue::from_str(key.as_ref()),
                    JsValue::from_f64(chunk.seq as f64), blob_binding(&columns.get(7)),
                ])?);
            }
            if let Some(data) = &next {
                primary.append_write_statements(key, data, Some(&guard), &mut statements)?;
            }
            statements.push(primary.prepare(update::FINISH_SQL, vec![JsValue::from_str(&guard)])?);
            drop(snapshot);
            drop(rows);
            drop(next);
            let results = primary.db.batch(statements).await.map_err(worker_error)?;
            crate::d1_repository::ensure_batch_success(&results)?;
            let committed = results.last().ok_or_else(|| worker_error("missing object update result"))?
                .results::<GuardCommit>().map_err(worker_error)?;
            if committed.iter().any(|result| result.committed == 1) {
                return Ok(ObjectUpdateResult::Committed);
            }
        }
        Ok(ObjectUpdateResult::Conflict)
    }

    async fn stat_row(&self, key: &ObjectKey) -> Result<Option<ObjectMetadata>> {
        let statement = self.prepare(
            "SELECT size, updated_at, content_type FROM objects WHERE object_key = ?",
            vec![JsValue::from_str(key.as_ref())],
        )?;
        let row = statement
            .first::<StatRow>(None)
            .await
            .map_err(worker_error)?;
        Ok(row.map(|row| metadata_from(key, row.size, &row.updated_at, row.content_type)))
    }
    async fn read_all(&self, key: &ObjectKey) -> Result<Option<Vec<u8>>> {
        // raw_js_value keeps BLOB columns as ArrayBuffer; the typed-row
        // `first::<T>` path cannot deserialize them.
        let statement = self.prepare(
            "SELECT data, size, encoding, crc32 FROM objects WHERE object_key = ?",
            vec![JsValue::from_str(key.as_ref())],
        )?;
        let rows = statement.raw_js_value().await.map_err(worker_error)?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        let columns = worker::js_sys::Array::from(&row);
        let inline = js_value_to_bytes(&columns.get(0))?;
        let size = js_value_to_i64(&columns.get(1));
        let encoding = js_value_to_string(&columns.get(2)).unwrap_or_else(|| "none".into());
        let crc32 = js_value_to_i64(&columns.get(3)) as u32;
        let encoding = narou_rs::platform::ObjectEncoding::parse(&encoding)?;
        let stored = if !inline.is_empty() || size == 0 {
            inline
        } else {
            let chunk_rows = self
                .prepare(
                    "SELECT data FROM object_chunks WHERE object_key = ? ORDER BY seq",
                    vec![JsValue::from_str(key.as_ref())],
                )?
                .raw_js_value()
                .await
                .map_err(worker_error)?;
            let mut stored = Vec::new();
            for chunk_row in chunk_rows {
                let cols = worker::js_sys::Array::from(&chunk_row);
                stored.extend_from_slice(&js_value_to_bytes(&cols.get(0))?);
            }
            stored
        };
        let data = narou_rs::platform::decompress_object_payload(&stored, encoding)?;
        narou_rs::platform::verify_object_crc32(&data, crc32)?;
        Ok(Some(data))
    }

    async fn write_all(&self, key: &ObjectKey, data: &[u8]) -> Result<()> {
        let mut statements = Vec::with_capacity(2 + data.len().div_ceil(CHUNK_SIZE));
        self.append_write_statements(key, data, None, &mut statements)?;
        let results = self.db.batch(statements).await.map_err(worker_error)?;
        crate::d1_repository::ensure_batch_success(&results)?;
        Ok(())
    }

    async fn delete_all(&self, key: &ObjectKey) -> Result<()> {
        let statements = vec![
            self.prepare(
                "DELETE FROM object_chunks WHERE object_key = ?",
                vec![JsValue::from_str(key.as_ref())],
            )?,
            self.prepare(
                "DELETE FROM objects WHERE object_key = ?",
                vec![JsValue::from_str(key.as_ref())],
            )?,
        ];
        // D1 のバッチは 1 文ずつ成否を確認しないと失敗を握りつぶす。
        let results = self.db.batch(statements).await.map_err(worker_error)?;
        crate::d1_repository::ensure_batch_success(&results)?;
        Ok(())
    }

    /// All keys under `prefix` (inclusive range scan + `matches` filter),
    /// sorted ascending. D1 returns whole result sets, so pagination is
    /// applied in memory after the filtered sort.
    async fn list_keys(&self, prefix: &str) -> Result<Vec<(String, ObjectMetadata)>> {
        let upper = narou_rs::platform::prefix_upper_bound(prefix);
        let statement = self.prepare(
            "SELECT object_key, size, updated_at, content_type FROM objects \
             WHERE object_key >= ? AND object_key < ? ORDER BY object_key",
            vec![JsValue::from_str(prefix), JsValue::from_str(&upper)],
        )?;
        let rows = statement
            .all()
            .await
            .map_err(worker_error)?
            .results::<ListRow>()
            .map_err(worker_error)?;
        let mut out = Vec::new();
        for row in rows {
            let Ok(key) = ObjectKey::try_new(row.object_key.clone()) else {
                continue;
            };
            // `prefix` here is the raw string; use ObjectPrefix::matches for
            // the boundary check (prefix must end at a '/' boundary).
            let prefix_ok = narou_rs::platform::ObjectPrefix::new(prefix)
                .map(|p| p.matches(&key))
                .unwrap_or(false);
            if !prefix_ok {
                continue;
            }
            out.push((
                row.object_key,
                metadata_from(&key, row.size, &row.updated_at, row.content_type),
            ));
        }
        Ok(out)
    }
}

impl ObjectStore for D1ObjectStore {
    fn stat<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        Box::pin(async move { self.stat_row(key).await })
    }

    fn read_small<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(async move {
            let (snapshot, rows) = self.read_snapshot(key).await?;
            drop(rows);
            snapshot.into_payload(SMALL_CAP as usize)
        })
    }

    fn write_small<'a>(
        &'a self,
        key: &'a ObjectKey,
        data: Vec<u8>,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            if data.len() as u64 > SMALL_CAP {
                return Err(NarouError::Platform(format!(
                    "Object {} exceeds small-write cap ({} bytes)",
                    key.as_ref(),
                    SMALL_CAP
                )));
            }
            self.write_all(key, &data).await
        })
    }

    fn update_small<'a, 'data: 'a>(
        &'a self,
        key: &'a ObjectKey,
        mutate: &'a mut SmallObjectMutator<'data>,
    ) -> PlatformFuture<'a, Result<ObjectUpdateResult>> {
        Box::pin(async move { self.update_control(key, mutate).await })
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move { self.delete_all(key).await })
    }

    fn list_page<'a>(
        &'a self,
        request: &'a ObjectListRequest,
    ) -> PlatformFuture<'a, Result<ObjectListPage>> {
        Box::pin(async move {
            let rows = self.list_keys(request.prefix.as_ref()).await?;
            let (objects, next_cursor) = paginate_object_listing(
                rows,
                request.cursor.as_deref(),
                request.limit.get(),
                |(key, _)| key,
            );
            let objects = objects.into_iter().map(|(_, meta)| meta).collect();
            Ok(ObjectListPage {
                objects,
                next_cursor,
            })
        })
    }
}

impl AssetStore for D1ObjectStore {
    fn stat<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<ObjectMetadata>>> {
        Box::pin(async move { self.stat_row(key).await })
    }

    fn read_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        Box::pin(async move {
            let Some(data) = self.read_all(key).await? else {
                return Ok(None);
            };
            let stream = futures::stream::once(async move { Ok(data) });
            Ok(Some(Box::pin(stream) as AssetStream))
        })
    }

    fn write_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
        stream: AssetStream,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            use futures::StreamExt;
            let mut stream = stream;
            let mut data = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                data.extend_from_slice(&chunk);
            }
            self.write_all(key, &data).await
        })
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move { self.delete_all(key).await })
    }

    fn copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            if let Some(data) = self.read_all(source).await? {
                self.write_all(destination, &data).await?;
            }
            Ok(())
        })
    }

    fn move_or_copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            if let Some(data) = self.read_all(source).await? {
                self.write_all(destination, &data).await?;
                self.delete_all(source).await?;
            }
            Ok(())
        })
    }
}

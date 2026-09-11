//! D1-backed ObjectStore/AssetStore for the Worker runtime.
//!
//! The `objects`/`object_chunks` tables (migration 0008) replace the Wasabi
//! bucket. Payloads are stored base64-encoded in TEXT columns so the row
//! format is identical to the native SQLite store and crosses the
//! serde-wasm-bindgen boundary as plain strings — no BLOB/Uint8Array
//! conversion ambiguity on either side.
//!
//! Payloads up to 512 KiB live inline in `objects.data`; larger payloads set
//! `objects.data = NULL` and store all bytes in `object_chunks` (seq 0..N,
//! 512 KiB each), matching the native `SqliteObjectStore` layout.

use std::sync::Arc;

use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{
    AssetStore, AssetStream, ObjectKey, ObjectListPage, ObjectListRequest, ObjectMetadata,
    ObjectStore, PlatformFuture,
};
use serde::Deserialize;
use worker::{D1Database, D1PreparedStatement, wasm_bindgen::JsValue};

/// Payloads at or below this size are stored inline in `objects.data`.
const INLINE_MAX: usize = 512 * 1024;
/// Chunk size for payloads above `INLINE_MAX`.
const CHUNK_SIZE: usize = 512 * 1024;
/// `read_small`/`write_small` cap, matching the native store's contract.
const SMALL_CAP: u64 = 16 * 1024 * 1024;

fn worker_error(error: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(format!("Worker storage error: {error}"))
}

fn encode_payload(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn decode_payload(encoded: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| NarouError::Platform(format!("invalid base64 object payload: {error}")))
}

#[derive(Debug, Deserialize)]
struct StatRow {
    size: i64,
    updated_at: String,
    content_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DataRow {
    data: Option<String>,
    size: i64,
    encoding: String,
}

#[derive(Debug, Deserialize)]
struct ChunkRow {
    data: String,
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
    db: Arc<D1Database>,
}

impl D1ObjectStore {
    pub fn new(db: Arc<D1Database>) -> Self {
        Self { db }
    }

    fn prepare(&self, sql: &str, values: Vec<JsValue>) -> Result<D1PreparedStatement> {
        self.db.prepare(sql).bind(&values).map_err(worker_error)
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
        let statement = self.prepare(
            "SELECT data, size, encoding FROM objects WHERE object_key = ?",
            vec![JsValue::from_str(key.as_ref())],
        )?;
        let row = statement
            .first::<DataRow>(None)
            .await
            .map_err(worker_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let encoding = narou_rs::platform::ObjectEncoding::parse(&row.encoding)?;
        let stored = if let Some(encoded) = row.data {
            decode_payload(&encoded)?
        } else {
            let chunks = self
                .prepare(
                    "SELECT data FROM object_chunks WHERE object_key = ? ORDER BY seq",
                    vec![JsValue::from_str(key.as_ref())],
                )?
                .all()
                .await
                .map_err(worker_error)?
                .results::<ChunkRow>()
                .map_err(worker_error)?;
            let mut encoded = String::new();
            for chunk in chunks {
                encoded.push_str(&chunk.data);
            }
            decode_payload(&encoded)?
        };
        Ok(Some(narou_rs::platform::decompress_object_payload(
            &stored, encoding,
        )?))
    }

    async fn write_all(&self, key: &ObjectKey, data: &[u8]) -> Result<()> {
        let (stored, encoding) = narou_rs::platform::compress_object_payload(data);
        let encoded = encode_payload(&stored);
        let now = chrono::Utc::now().to_rfc3339();
        let content_type = content_type_for_key(key);
        let mut statements = Vec::new();
        if encoded.len() <= INLINE_MAX {
            statements.push(self.prepare(
                "INSERT INTO objects (object_key, size, updated_at, content_type, encoding, data) \
                 VALUES (?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(object_key) DO UPDATE SET \
                 size = excluded.size, updated_at = excluded.updated_at, \
                 content_type = excluded.content_type, encoding = excluded.encoding, \
                 data = excluded.data",
                vec![
                    JsValue::from_str(key.as_ref()),
                    JsValue::from_f64(data.len() as f64),
                    JsValue::from_str(&now),
                    match &content_type {
                        Some(ct) => JsValue::from_str(ct),
                        None => JsValue::NULL,
                    },
                    JsValue::from_str(encoding.as_str()),
                    JsValue::from_str(&encoded),
                ],
            )?);
            statements.push(self.prepare(
                "DELETE FROM object_chunks WHERE object_key = ?",
                vec![JsValue::from_str(key.as_ref())],
            )?);
        } else {
            statements.push(self.prepare(
                "INSERT INTO objects (object_key, size, updated_at, content_type, encoding, data) \
                 VALUES (?, ?, ?, ?, ?, NULL) \
                 ON CONFLICT(object_key) DO UPDATE SET \
                 size = excluded.size, updated_at = excluded.updated_at, \
                 content_type = excluded.content_type, encoding = excluded.encoding, \
                 data = NULL",
                vec![
                    JsValue::from_str(key.as_ref()),
                    JsValue::from_f64(data.len() as f64),
                    JsValue::from_str(&now),
                    match &content_type {
                        Some(ct) => JsValue::from_str(ct),
                        None => JsValue::NULL,
                    },
                    JsValue::from_str(encoding.as_str()),
                ],
            )?);
            statements.push(self.prepare(
                "DELETE FROM object_chunks WHERE object_key = ?",
                vec![JsValue::from_str(key.as_ref())],
            )?);
            for (seq, chunk) in encoded.as_bytes().chunks(CHUNK_SIZE).enumerate() {
                let chunk = std::str::from_utf8(chunk)
                    .map_err(|error| NarouError::Platform(error.to_string()))?;
                statements.push(self.prepare(
                    "INSERT INTO object_chunks (object_key, seq, data) VALUES (?, ?, ?)",
                    vec![
                        JsValue::from_str(key.as_ref()),
                        JsValue::from_f64(seq as f64),
                        JsValue::from_str(chunk),
                    ],
                )?);
            }
        }
        self.db.batch(statements).await.map_err(worker_error)?;
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
        self.db.batch(statements).await.map_err(worker_error)?;
        Ok(())
    }

    /// All keys under `prefix` (inclusive range scan + `matches` filter),
    /// sorted ascending. D1 returns whole result sets, so pagination is
    /// applied in memory after the filtered sort.
    async fn list_keys(&self, prefix: &str) -> Result<Vec<(String, ObjectMetadata)>> {
        let upper = format!("{prefix}0");
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

fn content_type_for_key(key: &ObjectKey) -> Option<String> {
    match key.as_ref().rsplit('.').next() {
        Some("yaml") | Some("yml") => Some("application/yaml".to_string()),
        Some("txt") => Some("text/plain; charset=utf-8".to_string()),
        Some("ini") => Some("text/plain; charset=utf-8".to_string()),
        Some("html") => Some("text/html; charset=utf-8".to_string()),
        Some("epub") => Some("application/epub+zip".to_string()),
        Some("zip") => Some("application/zip".to_string()),
        Some("jpg") | Some("jpeg") => Some("image/jpeg".to_string()),
        Some("png") => Some("image/png".to_string()),
        Some("gif") => Some("image/gif".to_string()),
        _ => None,
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
            let Some(data) = self.read_all(key).await? else {
                return Ok(None);
            };
            if data.len() as u64 > SMALL_CAP {
                return Err(NarouError::Platform(format!(
                    "Object {} exceeds small-read cap ({} bytes)",
                    key.as_ref(),
                    SMALL_CAP
                )));
            }
            Ok(Some(data))
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

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move { self.delete_all(key).await })
    }

    fn list_page<'a>(
        &'a self,
        request: &'a ObjectListRequest,
    ) -> PlatformFuture<'a, Result<ObjectListPage>> {
        Box::pin(async move {
            let rows = self.list_keys(request.prefix.as_ref()).await?;
            let start = match &request.cursor {
                Some(cursor) => rows
                    .iter()
                    .position(|(key, _)| key.as_str() > cursor.as_str())
                    .unwrap_or(rows.len()),
                None => 0,
            };
            let mut objects = Vec::new();
            let mut next_cursor = None;
            for (key, meta) in rows.into_iter().skip(start) {
                if objects.len() >= request.limit.get() {
                    next_cursor = Some(key);
                    break;
                }
                objects.push(meta);
            }
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

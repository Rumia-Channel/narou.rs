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

#[path = "d1_asset_stream.rs"]
mod asset_stream;

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
    stage_leases: std::sync::Arc<narou_rs::platform::staging::StageLeaseCache>,
}

impl D1ObjectStore {
    pub fn new(db: DbHandle) -> Self {
        Self { db: db.primary_view(), stage_leases: std::sync::Arc::new(narou_rs::platform::staging::StageLeaseCache::default()) }
    }

    fn prepare(&self, sql: &str, values: Vec<JsValue>) -> Result<D1PreparedStatement> {
        self.db.prepare(sql).bind(&values).map_err(worker_error)
    }

    /// Same as `db.batch` but the error names the calling line, so a guard
    /// abort (`abs(INT64_MIN)` -> "integer overflow") records which write
    /// path rejected it instead of an opaque D1 message.
    #[track_caller]
    async fn checked_batch(&self, statements: Vec<D1PreparedStatement>) -> Result<Vec<worker::D1Result>> {
        let site = std::panic::Location::caller();
        let results = self.db.batch(statements).await.map_err(|error| {
            worker_error(format!("D1 batch failed at {site}: {error}"))
        })?;
        crate::d1_repository::ensure_batch_success(&results).map_err(|error| {
            worker_error(format!("guard batch rejected at {site}: {error}"))
        })?;
        Ok(results)
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
            content_type.map(JsValue::from_str).unwrap_or(JsValue::NULL),
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

    async fn update_control<'data>(&self, key: &ObjectKey, mutate: &mut SmallObjectMutator<'data>,fence:Option<&WriteFence>) -> Result<ObjectUpdateResult> {
        let primary = Self::new(self.db.primary_view());
        let guard = format!("object_write_guard/{}", key.as_ref());
        for _ in 0..4 {
            if let Some(reference)=primary.transfer_reference(key).await?{
                let current=ObjectStore::read_small(&primary,key).await?;let next=mutate(current.as_deref())?;
                if next.as_ref().is_some_and(|bytes|bytes.len() as u64>SMALL_CAP){return Err(worker_error("atomic control write exceeds cap"));}
                let token=crate::content_store::nonce()?;let mut statements=Vec::new();if let Some(fence)=fence{if !fence.prefix.matches(key){return Err(worker_error("atomic update outside fence"));}statements.push(primary.open_guard(fence,&token)?);}
                statements.push(primary.prepare("SELECT CASE WHEN EXISTS(SELECT 1 FROM transfer_object_refs WHERE object_key=? AND blob_key=? AND sha256=? AND blob_generation=?) THEN 1 ELSE abs(-9223372036854775808) END",vec![key.as_ref().into(),reference.blob_key.into(),reference.sha256.into(),reference.blob_generation.into()])?);
                if let Some(next)=next{primary.append_write_statements(key,&next,None,&mut statements)?;}
                if fence.is_some(){statements.push(primary.finish_publication_guard(&token)?);}primary.checked_batch(statements).await?;return Ok(ObjectUpdateResult::Committed);
            }
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
            let mut statements = Vec::with_capacity(6 + snapshot.chunks.len() + write_capacity);
            let gate_token=crate::content_store::nonce()?;if let Some(fence)=fence{if !fence.prefix.matches(key){return Err(worker_error("atomic update outside fence"));}statements.push(primary.open_guard(fence,&gate_token)?);}
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
            let finish_index=statements.len();
            statements.push(primary.prepare(update::FINISH_SQL, vec![JsValue::from_str(&guard)])?);
            if fence.is_some(){statements.push(primary.finish_publication_guard(&gate_token)?);}
            drop(snapshot);
            drop(rows);
            drop(next);
            let results = primary.checked_batch(statements).await?;
            let committed = results.get(finish_index).ok_or_else(|| worker_error("missing object update result"))?
                .results::<GuardCommit>().map_err(worker_error)?;
            if committed.iter().any(|result| result.committed == 1) {
                return Ok(ObjectUpdateResult::Committed);
            }
        }
        Ok(ObjectUpdateResult::Conflict)
    }

    async fn logical_stream_range(&self,key:&ObjectKey,range:std::ops::Range<u64>)->Result<Option<AssetStream>>{
        use futures::StreamExt;
        let Some(row)=self.transfer_reference(key).await?else{return self.read_asset_stream_range(key,range).await;};
        if row.backend=="deleted"{return Ok(None);}if row.backend!="d1"{return Err(worker_error("S3 reference requires the composed reference store"));}
        let end=if range.end==u64::MAX{row.size}else{range.end};if range.start>end||end>row.size{return Err(worker_error("reference range exceeds object"));}let full=range.start==0&&end==row.size;let size=end-range.start;
        let stream=self.read_bound_reference(&row,range.start..end).await?.ok_or_else(||worker_error("referenced D1 bytes are missing"))?;
        Ok(Some(Box::pin(futures::stream::try_unfold((stream,Sha256::new(),0u64,row.sha256),move|(mut stream,mut hash,seen,sha)|async move{
            match stream.next().await{Some(chunk)=>{let bytes=chunk?;let next=seen.checked_add(bytes.len() as u64).filter(|value|*value<=size).ok_or_else(||worker_error("reference range overflow"))?;if full{hash.update(&bytes);}Ok(Some((bytes,(stream,hash,next,sha))))},None=>{if seen!=size||(full&&hex::encode(hash.finalize())!=sha){return Err(worker_error("D1 reference size/SHA mismatch"));}Ok(None)}}
        }))))
    }
    async fn stat_row(&self, key: &ObjectKey) -> Result<Option<ObjectMetadata>> {
        if let Some(row)=self.transfer_reference(key).await?{if row.backend=="deleted"{return Ok(None);}return Ok(Some(metadata_from(key,row.size as i64,&row.updated_at,content_type_for_key(key).map(str::to_owned))));}
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

    async fn write_all(&self, key: &ObjectKey, data: &[u8]) -> Result<()> {
        let mut statements = Vec::with_capacity(2 + data.len().div_ceil(CHUNK_SIZE));
        self.append_write_statements(key, data, None, &mut statements)?;
        self.checked_batch(statements).await?;
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
        self.checked_batch(statements).await?;
        self.stage_leases.forget(key);
        Ok(())
    }

    /// Read one indexed metadata page plus one lookahead row, not the entire
    /// prefix on every page. Exact-prefix and descendant keys share the same
    /// boundary/cursor contract as Native SQLite.
    async fn list_keys(&self, request: &ObjectListRequest) -> Result<Vec<(String, ObjectMetadata)>> {
        let prefix = request.prefix.as_ref().trim_end_matches('/');
        let lower = if prefix.is_empty() { String::new() } else { format!("{prefix}/") };
        let upper = narou_rs::platform::prefix_upper_bound(&lower);
        let cap = request.limit.get().saturating_add(1).min(i64::MAX as usize);
        let rows = self.prepare(
            narou_rs::db::object_query::LIST_PAGE_SQL,
            vec![prefix.into(), lower.into(), upper.into(),
                request.cursor.as_deref().unwrap_or("").into(), JsValue::from_f64(cap as f64)],
        )?.all().await.map_err(worker_error)?.results::<ListRow>().map_err(worker_error)?;
        rows.into_iter().map(|row| {
            let key = ObjectKey::try_new(row.object_key.clone())?;
            Ok((row.object_key, metadata_from(&key, row.size, &row.updated_at, row.content_type)))
        }).collect()
    }
}

use narou_rs::platform::{transfer_stage as ts,access_fence as af,TransferStageStore,NovelAccessFence,ObjectPrefix,StageCursor,SealedAsset,StageLease,GateLease,GateInfo,GateState,WriteFence};
use sha2::{Digest,Sha256};

#[derive(Clone,serde::Deserialize,serde::Serialize)]
pub struct TransferReference {pub object_key:String,pub blob_key:String,pub size:u64,pub sha256:String,pub backend:String,pub updated_at:String,#[serde(default)]pub blob_generation:String}
#[derive(Deserialize)]struct StageRow{record_json:String,revision:u64}
#[derive(Deserialize)]struct StageChanged{stage_id:String}
#[derive(Deserialize)]struct DbGate{epoch:u64,closed:i64,owner:Option<String>,updated_at:i64,pending_readers:u32,#[serde(default)]content_revision:u64}

impl D1ObjectStore {
    pub(crate) async fn writer_for_key(&self,key:&ObjectKey)->Result<Option<WriteFence>>{
        if narou_rs::platform::is_reserved_key(key){return Ok(None);}
        let prefix=self.prepare("SELECT prefix FROM (SELECT prefix FROM novel_access_gates UNION SELECT prefix FROM novel_gate_bindings) WHERE ?1=prefix OR substr(?1,1,length(prefix)+1)=prefix||'/' ORDER BY length(prefix) DESC LIMIT 1",vec![key.as_ref().into()])?.first::<String>(Some("prefix")).await.map_err(worker_error)?;
        match prefix{Some(prefix)=>self.capture_writer(&ObjectPrefix::new(prefix)?).await.map(Some),None=>Ok(None)}
    }
    pub async fn transfer_reference(&self,key:&ObjectKey)->Result<Option<TransferReference>>{
        self.prepare("SELECT object_key,blob_key,size,sha256,backend,updated_at,blob_generation FROM transfer_object_refs WHERE object_key=?",vec![key.as_ref().into()])?.first(None).await.map_err(worker_error)
    }
    pub(crate) async fn read_bound_reference(&self,row:&TransferReference,range:std::ops::Range<u64>)->Result<Option<AssetStream>>{
        if row.backend!="d1"||row.blob_generation.is_empty(){return Err(worker_error("D1 reference generation proof missing"));}
        self.read_asset_stream_range_generation(&ObjectKey::try_new(row.blob_key.clone())?,range,Some(&row.blob_generation)).await
    }
    async fn sealed_object(&self,key:&ObjectKey)->Result<SealedAsset>{
        #[derive(Deserialize)]struct Proof{size:u64,sha256:String}
        let proof=self.prepare("SELECT v.size,v.sha256 FROM transfer_verified_objects v JOIN objects o ON o.object_key=v.blob_key AND o.generation=v.generation WHERE v.blob_key=?",vec![key.as_ref().into()])?.first::<Proof>(None).await.map_err(worker_error)?.ok_or_else(||worker_error("sealed stage generation proof missing"))?;
        Ok(SealedAsset{key:key.clone(),size:proof.size,sha256:proof.sha256})
    }
    pub async fn transfer_stage_referenced(&self,key:&ObjectKey)->Result<bool>{
        Ok(self.prepare("SELECT 1 AS present FROM transfer_object_refs WHERE blob_key=? LIMIT 1",vec![key.as_ref().into()])?.first::<i64>(Some("present")).await.map_err(worker_error)?.is_some())
    }
    pub fn publication_guard(&self,prefix:&ObjectPrefix,owner:&str,epoch:u64,token:&str)->Result<Vec<D1PreparedStatement>>{
        Ok(vec![self.prepare("INSERT INTO transfer_storage_guards(token,prefix,owner,epoch,closed) VALUES (?,?,?,?,1)",vec![token.into(),prefix.as_ref().trim_end_matches('/').into(),owner.into(),JsValue::from_f64(epoch as f64)])?])
    }
    pub fn finish_publication_guard(&self,token:&str)->Result<D1PreparedStatement>{self.prepare("DELETE FROM transfer_storage_guards WHERE token=? RETURNING token",vec![token.into()])}
    pub fn push_transfer_reference_statements(&self,prefix:&ObjectPrefix,owner:&str,epoch:u64,token:&str,key:&ObjectKey,sealed:&SealedAsset,statements:&mut Vec<D1PreparedStatement>)->Result<()>{
        self.push_reference(prefix,owner,epoch,token,key,sealed,"d1",statements)
    }
    pub(crate) fn push_reference(&self,prefix:&ObjectPrefix,owner:&str,epoch:u64,token:&str,key:&ObjectKey,sealed:&SealedAsset,backend:&str,statements:&mut Vec<D1PreparedStatement>)->Result<()>{
        if !prefix.matches(key)||!narou_rs::platform::is_transfer_key(&sealed.key){return Err(worker_error("reference outside publication namespace"));}
        statements.push(self.prepare("INSERT INTO transfer_object_refs(object_key,blob_key,size,sha256,backend,updated_at,blob_generation) SELECT ?1,?2,?3,?4,?5,?6,CASE WHEN ?5='d1' THEN (SELECT v.generation FROM transfer_verified_objects v JOIN objects o ON o.object_key=v.blob_key AND o.generation=v.generation WHERE v.blob_key=?2 AND v.size=?3 AND v.sha256=?4) ELSE '' END WHERE EXISTS(SELECT 1 FROM transfer_storage_guards WHERE token=?7 AND prefix=?8 AND owner=?9 AND epoch=?10) ON CONFLICT(object_key) DO UPDATE SET blob_key=excluded.blob_key,size=excluded.size,sha256=excluded.sha256,backend=excluded.backend,updated_at=excluded.updated_at,blob_generation=excluded.blob_generation",
            vec![key.as_ref().into(),sealed.key.as_ref().into(),JsValue::from_f64(sealed.size as f64),sealed.sha256.as_str().into(),backend.into(),chrono::Utc::now().to_rfc3339().into(),token.into(),prefix.as_ref().trim_end_matches('/').into(),owner.into(),JsValue::from_f64(epoch as f64)])?);Ok(())
    }
    async fn load_stage(&self,id:&str)->Result<Option<(ts::StageLedgerRecord,u64)>>{
        ts::stage_data_key(id)?;let row=self.prepare("SELECT record_json,revision FROM transfer_stage_ledger WHERE stage_id=?",vec![id.into()])?.first::<StageRow>(None).await.map_err(worker_error)?;
        row.map(|row|Ok((ts::decode_ledger(row.record_json.as_bytes())?,row.revision))).transpose()
    }
    async fn stage_cas(&self,id:&str,revision:u64,next:&ts::StageLedgerRecord,mut statements:Vec<D1PreparedStatement>,token:&str)->Result<bool>{
        let json=String::from_utf8(ts::encode_ledger(next)?).map_err(worker_error)?;let reclaim=next.lease.state==ts::StageLeaseState::Aborting;
        if next.lease.state==ts::StageLeaseState::Writing{let key=ts::stage_checkpoint_key(id,next.confirmed_bytes)?;let bytes=serde_json::to_vec(&next.cursor()).map_err(worker_error)?;
            statements.push(self.prepare("INSERT INTO objects(object_key,size,updated_at,encoding,crc32,data) SELECT ?,?,?,'none',?,? WHERE EXISTS(SELECT 1 FROM transfer_stage_ledger WHERE stage_id=? AND mutation_id=?) ON CONFLICT(object_key) DO UPDATE SET size=excluded.size,updated_at=excluded.updated_at,encoding='none',crc32=excluded.crc32,data=excluded.data",vec![key.as_ref().into(),JsValue::from_f64(bytes.len() as f64),chrono::Utc::now().to_rfc3339().into(),JsValue::from_f64(narou_rs::platform::object_crc32(&bytes) as f64),JsValue::from(worker::js_sys::Uint8Array::from(bytes.as_slice())),id.into(),token.into()])?);
        }
        let mut batch=Vec::with_capacity(statements.len()+2);
        batch.push(self.prepare("UPDATE transfer_stage_ledger SET record_json=?,revision=revision+1,mutation_id=? WHERE stage_id=? AND revision=? RETURNING stage_id",
            vec![json.into(),token.into(),id.into(),JsValue::from_f64(revision as f64)])?);batch.append(&mut statements);
        if reclaim{let lower=format!(".narou-transfer/.checkpoints/{id}/");let upper=narou_rs::platform::prefix_upper_bound(&lower);
            batch.push(self.prepare("DELETE FROM objects WHERE object_key IN (SELECT object_key FROM objects WHERE object_key>=? AND object_key<? ORDER BY object_key LIMIT 24) AND EXISTS(SELECT 1 FROM transfer_stage_ledger WHERE stage_id=? AND mutation_id=?)",vec![lower.as_str().into(),upper.as_str().into(),id.into(),token.into()])?);
            batch.push(self.prepare("DELETE FROM transfer_stage_ledger WHERE stage_id=?1 AND mutation_id=?2 AND NOT EXISTS(SELECT 1 FROM objects WHERE object_key>=?3 AND object_key<?4) AND NOT EXISTS(SELECT 1 FROM objects WHERE object_key=?5)",vec![id.into(),token.into(),lower.as_str().into(),upper.as_str().into(),ts::stage_data_key(id)?.as_ref().into()])?);
        }
        let result=self.checked_batch(batch).await?;
        Ok(result[0].results::<StageChanged>().map_err(worker_error)?.iter().any(|row|row.stage_id==id))
    }
    fn stage_cleanup_statements(&self,id:&str,token:&str)->Result<Vec<D1PreparedStatement>>{
        let key=ts::stage_data_key(id)?;
        Ok(vec![self.prepare("DELETE FROM object_chunks WHERE object_key=?1 AND seq IN (SELECT seq FROM object_chunks WHERE object_key=?1 ORDER BY seq LIMIT 16) AND NOT EXISTS(SELECT 1 FROM transfer_object_refs WHERE blob_key=?1) AND EXISTS(SELECT 1 FROM transfer_stage_ledger WHERE stage_id=?2 AND mutation_id=?3)",vec![key.as_ref().into(),id.into(),token.into()])?,
            self.prepare("DELETE FROM objects WHERE object_key=?1 AND NOT EXISTS(SELECT 1 FROM object_chunks WHERE object_key=?1) AND NOT EXISTS(SELECT 1 FROM transfer_object_refs WHERE blob_key=?1) AND EXISTS(SELECT 1 FROM transfer_stage_ledger WHERE stage_id=?2 AND mutation_id=?3)",vec![key.as_ref().into(),id.into(),token.into()])?])
    }
    async fn gate_row(&self,prefix:&ObjectPrefix)->Result<DbGate>{
        let key=prefix.as_ref().trim_end_matches('/');
        let results=self.checked_batch(vec![self.prepare("INSERT OR IGNORE INTO novel_access_gates(prefix,updated_at) VALUES (?,?)",vec![key.into(),JsValue::from_f64(ts::now_secs() as f64)])?,
            self.prepare("SELECT epoch,closed,owner,updated_at,content_revision,(SELECT count(*) FROM novel_reader_leases r WHERE r.prefix=g.prefix AND r.expires_at>unixepoch()) AS pending_readers FROM novel_access_gates g WHERE prefix=?",vec![key.into()])?]).await?;
        results[1].results::<DbGate>().map_err(worker_error)?.into_iter().next().ok_or_else(||worker_error("missing gate"))
    }
    pub(crate) fn open_guard(&self,fence:&WriteFence,token:&str)->Result<D1PreparedStatement>{
        if fence.epoch>9_007_199_254_740_991{return Err(worker_error("writer epoch exceeds exact D1 range"));}
        if let Some(owner)=fence.job_owner(){
            self.prepare("INSERT INTO transfer_storage_guards(token,prefix,owner,epoch,closed) VALUES (?1,?2,'',CASE WHEN EXISTS(SELECT 1 FROM worker_jobs j WHERE j.job_id=?4 AND j.execution_token=?5 AND j.status='running' AND julianday(j.lease_until)>julianday('now')) THEN ?3 ELSE abs(-9223372036854775808) END,0)",vec![token.into(),fence.prefix.as_ref().trim_end_matches('/').into(),JsValue::from_f64(fence.epoch as f64),owner.job_id().into(),owner.execution_token().into()])
        }else{
            self.prepare("INSERT INTO transfer_storage_guards(token,prefix,owner,epoch,closed) VALUES (?1,?2,'',CASE WHEN NOT EXISTS(SELECT 1 FROM novel_namespace_claims WHERE prefix=?2 AND job_id IS NOT NULL) THEN ?3 ELSE abs(-9223372036854775808) END,0)",vec![token.into(),fence.prefix.as_ref().trim_end_matches('/').into(),JsValue::from_f64(fence.epoch as f64)])
        }
    }
}

impl TransferStageStore for D1ObjectStore {
    fn stage_open<'a>(&'a self,id:&'a str,owner:&'a str,epoch:u64,ttl:i64)->PlatformFuture<'a,Result<StageCursor>>{Box::pin(async move{
        if let Some((record,_))=self.load_stage(id).await?{ts::require_owner(&record,owner,epoch)?;if record.lease.state==ts::StageLeaseState::Aborting{return Err(worker_error("stage_aborted"));}return Ok(record.cursor());}
        let record=ts::StageLedgerRecord::new(id,owner,epoch,ttl)?;let token=crate::content_store::nonce()?;let json=String::from_utf8(ts::encode_ledger(&record)?).map_err(worker_error)?;let key=ts::stage_data_key(id)?;
        self.checked_batch(vec![self.prepare("INSERT OR IGNORE INTO transfer_stage_ledger(stage_id,record_json,mutation_id) VALUES (?,?,?)",vec![id.into(),json.into(),token.as_str().into()])?,
            self.prepare("INSERT INTO objects(object_key,size,updated_at,encoding,crc32,data) SELECT ?,0,?,'none',0,NULL WHERE EXISTS(SELECT 1 FROM transfer_stage_ledger WHERE stage_id=? AND mutation_id=?)",vec![key.as_ref().into(),chrono::Utc::now().to_rfc3339().into(),id.into(),token.as_str().into()])?]).await?;
        let(record,_)=self.load_stage(id).await?.ok_or_else(||worker_error("stage reservation disappeared"))?;ts::require_owner(&record,owner,epoch)?;Ok(record.cursor())
    })}
    fn stage_cursor<'a>(&'a self,id:&'a str)->PlatformFuture<'a,Result<Option<StageCursor>>>{Box::pin(async move{let Some((record,_))=self.load_stage(id).await?else{return Ok(None);};if record.lease.state==ts::StageLeaseState::Aborting&&self.transfer_stage_referenced(&ts::stage_data_key(id)?).await?{return Ok(None);}Ok(Some(record.cursor()))})}
    fn stage_append<'a>(&'a self,id:&'a str,owner:&'a str,epoch:u64,offset:u64,payload:&'a [u8],last:bool)->PlatformFuture<'a,Result<StageCursor>>{Box::pin(async move{
        let(mut record,revision)=self.load_stage(id).await?.ok_or_else(||worker_error("stage not open"))?;record.append(owner,epoch,offset,payload,last)?;let key=ts::stage_data_key(id)?;let token=crate::content_store::nonce()?;let mut statements=Vec::new();let mut position=offset;let mut input=payload;
        while !input.is_empty(){let seq=position/CHUNK_SIZE as u64;let inside=(position%CHUNK_SIZE as u64) as usize;let length=input.len().min(CHUNK_SIZE-inside);
            if inside==0{statements.push(self.prepare("INSERT INTO object_chunks(object_key,seq,data) SELECT ?,?,? WHERE EXISTS(SELECT 1 FROM transfer_stage_ledger WHERE stage_id=? AND mutation_id=?) ON CONFLICT(object_key,seq) DO UPDATE SET data=excluded.data",vec![key.as_ref().into(),JsValue::from_f64(seq as f64),JsValue::from(worker::js_sys::Uint8Array::from(&input[..length])),id.into(),token.as_str().into()])?);}
            else{statements.push(self.prepare("UPDATE object_chunks SET data=CAST(substr(data,1,?) || ? AS BLOB) WHERE object_key=? AND seq=? AND length(data)>=? AND EXISTS(SELECT 1 FROM transfer_stage_ledger WHERE stage_id=? AND mutation_id=?)",vec![JsValue::from_f64(inside as f64),JsValue::from(worker::js_sys::Uint8Array::from(&input[..length])),key.as_ref().into(),JsValue::from_f64(seq as f64),JsValue::from_f64(inside as f64),id.into(),token.as_str().into()])?);}
            position+=length as u64;input=&input[length..];}
        statements.push(self.prepare("UPDATE objects SET size=?,crc32=?,updated_at=? WHERE object_key=? AND EXISTS(SELECT 1 FROM transfer_stage_ledger WHERE stage_id=? AND mutation_id=?)",vec![JsValue::from_f64(record.confirmed_bytes as f64),JsValue::from_f64(record.crc32 as f64),chrono::Utc::now().to_rfc3339().into(),key.as_ref().into(),id.into(),token.as_str().into()])?);
        if !self.stage_cas(id,revision,&record,statements,&token).await?{return Err(worker_error("stage_offset_conflict"));}Ok(record.cursor())
    })}
    fn stage_rewind<'a>(&'a self,id:&'a str,owner:&'a str,epoch:u64,checkpoint:&'a StageCursor)->PlatformFuture<'a,Result<StageCursor>>{Box::pin(async move{
        let(mut record,revision)=self.load_stage(id).await?.ok_or_else(||worker_error("stage not open"))?;
        let trusted=if checkpoint.confirmed_bytes==0{ts::StageLedgerRecord::new(id,owner,epoch,1)?.cursor()}else{let key=ts::stage_checkpoint_key(id,checkpoint.confirmed_bytes)?;let bytes=ObjectStore::read_small(self,&key).await?.ok_or_else(||worker_error("rewind checkpoint missing"))?;let cursor:StageCursor=serde_json::from_slice(&bytes).map_err(worker_error)?;if cursor.prefix_sha256()!=checkpoint.prefix_sha256(){return Err(worker_error("rewind SHA conflict"));}cursor};
        record.rewind(owner,epoch,&trusted)?;let key=ts::stage_data_key(id)?;let token=crate::content_store::nonce()?;let size=checkpoint.confirmed_bytes;let mut statements=vec![
            self.prepare("DELETE FROM object_chunks WHERE object_key=? AND seq>=? AND EXISTS(SELECT 1 FROM transfer_stage_ledger WHERE stage_id=? AND mutation_id=?)",vec![key.as_ref().into(),JsValue::from_f64(size.div_ceil(CHUNK_SIZE as u64) as f64),id.into(),token.as_str().into()])?,
            self.prepare("UPDATE objects SET size=?,crc32=? WHERE object_key=? AND EXISTS(SELECT 1 FROM transfer_stage_ledger WHERE stage_id=? AND mutation_id=?)",vec![JsValue::from_f64(size as f64),JsValue::from_f64(record.crc32 as f64),key.as_ref().into(),id.into(),token.as_str().into()])?];
        if size%CHUNK_SIZE as u64!=0{statements.push(self.prepare("UPDATE object_chunks SET data=substr(data,1,?) WHERE object_key=? AND seq=? AND EXISTS(SELECT 1 FROM transfer_stage_ledger WHERE stage_id=? AND mutation_id=?)",vec![JsValue::from_f64((size%CHUNK_SIZE as u64) as f64),key.as_ref().into(),JsValue::from_f64((size/CHUNK_SIZE as u64) as f64),id.into(),token.as_str().into()])?);}
        if !self.stage_cas(id,revision,&record,statements,&token).await?{return Err(worker_error("stage_offset_conflict"));}Ok(record.cursor())
    })}
    fn stage_read<'a>(&'a self,id:&'a str,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<AssetStream>>>{Box::pin(async move{if !self.stage_cursor(id).await?.is_some_and(|cursor|cursor.sealed){return Ok(None);}self.read_stream_range(&ts::stage_data_key(id)?,range).await})}
    fn stage_touch<'a>(&'a self,id:&'a str,owner:&'a str,epoch:u64,ttl:i64)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        let(mut record,revision)=self.load_stage(id).await?.ok_or_else(||worker_error("stage not open"))?;ts::require_owner(&record,owner,epoch)?;if record.lease.state==ts::StageLeaseState::Aborting{return Err(worker_error("stage_aborted"));}record.lease.expires_at=ts::now_secs().saturating_add(ttl.max(1));
        if !self.stage_cas(id,revision,&record,Vec::new(),&crate::content_store::nonce()?).await?{return Err(worker_error("stage lease conflict"));}Ok(())
    })}
    fn stage_seal<'a>(&'a self,id:&'a str,owner:&'a str,epoch:u64,size:u64,sha:&'a str)->PlatformFuture<'a,Result<SealedAsset>>{Box::pin(async move{
        let(mut record,revision)=self.load_stage(id).await?.ok_or_else(||worker_error("stage not open"))?;let sealed=record.verify_seal(owner,epoch,size,sha)?;
        if record.lease.state==ts::StageLeaseState::Sealed{let proof=self.sealed_object(&sealed.key).await?;if proof.size!=size||!proof.sha256.eq_ignore_ascii_case(sha){return Err(worker_error("sealed proof changed"));}return Ok(sealed);}
        record.lease.state=ts::StageLeaseState::Sealed;let token=crate::content_store::nonce()?;
        let statements=vec![self.prepare("SELECT CASE WHEN EXISTS(SELECT 1 FROM objects WHERE object_key=? AND size=? AND encoding='none') THEN 1 ELSE abs(-9223372036854775808) END",vec![sealed.key.as_ref().into(),JsValue::from_f64(size as f64)])?,
            self.prepare("INSERT INTO transfer_verified_objects(blob_key,size,sha256,generation) SELECT object_key,size,?,generation FROM objects WHERE object_key=? AND EXISTS(SELECT 1 FROM transfer_stage_ledger WHERE stage_id=? AND mutation_id=?) ON CONFLICT(blob_key) DO UPDATE SET size=excluded.size,sha256=excluded.sha256,generation=excluded.generation",vec![sha.into(),sealed.key.as_ref().into(),id.into(),token.as_str().into()])?];
        if !self.stage_cas(id,revision,&record,statements,&token).await?{return Err(worker_error("stage seal conflict"));}Ok(sealed)
    })}
    fn stage_abort<'a>(&'a self,id:&'a str,owner:&'a str,epoch:u64)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        let Some((mut record,revision))=self.load_stage(id).await?else{return Ok(());};ts::require_owner(&record,owner,epoch)?;record.lease.state=ts::StageLeaseState::Aborting;let token=crate::content_store::nonce()?;
        if !self.stage_cas(id,revision,&record,self.stage_cleanup_statements(id,&token)?,&token).await?{return Err(worker_error("stage lease changed"));}Ok(())
    })}
    fn stage_lease_page<'a>(&'a self,cursor:Option<&'a str>,limit:usize)->PlatformFuture<'a,Result<(Vec<StageLease>,Option<String>)>>{Box::pin(async move{
        #[derive(Deserialize)]struct Row{stage_id:String,record_json:String}let cap=limit.clamp(1,100);let rows=self.prepare("SELECT stage_id,record_json FROM transfer_stage_ledger WHERE stage_id>? ORDER BY stage_id LIMIT ?",vec![cursor.unwrap_or("").into(),JsValue::from_f64((cap+1) as f64)])?.all().await.map_err(worker_error)?.results::<Row>().map_err(worker_error)?;let next=if rows.len()>cap{Some(rows[cap-1].stage_id.clone())}else{None};let out=rows.into_iter().take(cap).map(|row|Ok(ts::decode_ledger(row.record_json.as_bytes())?.lease)).collect::<Result<_>>()?;Ok((out,next))
    })}
    fn stage_reclaim<'a>(&'a self,id:&'a str,owner:&'a str,epoch:u64)->PlatformFuture<'a,Result<bool>>{Box::pin(async move{
        let Some((mut record,revision))=self.load_stage(id).await?else{return Ok(true);};if record.lease.owner!=owner||record.lease.epoch!=epoch||(record.lease.state!=ts::StageLeaseState::Aborting&&record.lease.expires_at>ts::now_secs()){return Ok(false);}record.lease.state=ts::StageLeaseState::Aborting;
        let token=crate::content_store::nonce()?;let statements=self.stage_cleanup_statements(id,&token)?;
        if !self.stage_cas(id,revision,&record,statements,&token).await?{return Ok(false);}Ok(self.load_stage(id).await?.is_none())
    })}
}

impl NovelAccessFence for D1ObjectStore {
    fn retire_writer<'a>(&'a self,fence:&'a WriteFence,owner:&'a str)->PlatformFuture<'a,Result<u64>>{Box::pin(async move{
        let(id,_)=af::parse_removal_owner(owner)?;if id.0<0||id.0>9_007_199_254_740_991{return Err(worker_error("removal ID exceeds exact D1 integer range"));}let next=fence.epoch.checked_add(1).filter(|value|*value<=9_007_199_254_740_991).ok_or_else(||worker_error("gate epoch overflow"))?;let key=fence.prefix.as_ref().trim_end_matches('/');let ancestors=crate::d1_repository::ancestors(key)?;let lower=format!("{key}/");let upper=format!("{key}0");#[derive(Deserialize)]struct Epoch{epoch:u64}
        let mut statements=vec![self.prepare("INSERT OR IGNORE INTO novel_access_gates(prefix,updated_at) VALUES (?,?)",vec![key.into(),JsValue::from_f64(ts::now_secs() as f64)])?];
        if let Some(actor)=fence.job_owner(){statements.push(self.prepare("SELECT CASE WHEN EXISTS(SELECT 1 FROM novel_access_gates WHERE prefix=?1 AND closed=1 AND owner=?2 AND epoch=?3) OR EXISTS(SELECT 1 FROM worker_jobs WHERE job_id=?4 AND execution_token=?5 AND status='running' AND julianday(lease_until)>julianday('now')) THEN 1 ELSE abs(-9223372036854775808) END",vec![key.into(),owner.into(),JsValue::from_f64(next as f64),actor.job_id().into(),actor.execution_token().into()])?);}
        statements.push(self.prepare("SELECT CASE WHEN EXISTS(SELECT 1 FROM novel_access_gates WHERE prefix=?1 AND closed=1 AND owner=?6 AND epoch=?7) OR (EXISTS(SELECT 1 FROM novels n JOIN novel_gate_bindings b ON b.novel_id=n.id JOIN novel_access_gates g ON g.prefix=b.prefix WHERE n.id=?2 AND b.prefix=?1 AND (g.origin_url_fold='' OR g.origin_url_fold=n.toc_url_fold)) AND NOT EXISTS(SELECT 1 FROM novel_gate_bindings b JOIN novels n ON n.id=b.novel_id WHERE n.id<>?2 AND (b.prefix IN (SELECT value FROM json_each(?3)) OR (b.prefix>=?4 AND b.prefix<?5))) AND NOT EXISTS(SELECT 1 FROM novel_namespace_claims WHERE prefix IN (SELECT value FROM json_each(?3)) OR (prefix>=?4 AND prefix<?5)) AND NOT EXISTS(SELECT 1 FROM novel_access_gates WHERE prefix<>?1 AND (closed=1 OR origin_url_fold<>'') AND (prefix IN (SELECT value FROM json_each(?3)) OR (prefix>=?4 AND prefix<?5)))) THEN 1 ELSE abs(-9223372036854775808) END",vec![key.into(),JsValue::from_f64(id.0 as f64),ancestors.as_str().into(),lower.as_str().into(),upper.as_str().into(),owner.into(),JsValue::from_f64(next as f64)])?);
        statements.push(self.prepare("UPDATE novel_access_gates SET closed=1,owner=?1,epoch=?2,updated_at=?3,origin_url_fold=COALESCE(NULLIF(origin_url_fold,''),(SELECT toc_url_fold FROM novels WHERE id=?6),'') WHERE prefix=?4 AND closed=0 AND epoch=?5",vec![owner.into(),JsValue::from_f64(next as f64),JsValue::from_f64(ts::now_secs() as f64),key.into(),JsValue::from_f64(fence.epoch as f64),JsValue::from_f64(id.0 as f64)])?);
        statements.push(self.prepare("SELECT epoch FROM novel_access_gates WHERE prefix=? AND closed=1 AND owner=? AND epoch=?",vec![key.into(),owner.into(),JsValue::from_f64(next as f64)])?);let results=self.checked_batch(statements).await?;
        results.last().ok_or_else(||worker_error("missing retirement result"))?.results::<Epoch>().map_err(worker_error)?.into_iter().next().map(|row|row.epoch).ok_or_else(||af::gate_closed_error(&fence.prefix))
    })}
    fn pending_removals<'a>(&'a self,cursor:Option<&'a str>,limit:usize)->PlatformFuture<'a,Result<(Vec<(ObjectPrefix,String,u64)>,Option<String>)>>{Box::pin(async move{
        #[derive(Deserialize)]struct Row{prefix:String,owner:Option<String>,epoch:u64,closed:i64}let cap=limit.clamp(1,100);let rows=self.prepare("SELECT prefix,owner,epoch,closed FROM novel_access_gates WHERE prefix>? ORDER BY prefix LIMIT ?",vec![cursor.unwrap_or("").into(),JsValue::from_f64((cap+1) as f64)])?.all().await.map_err(worker_error)?.results::<Row>().map_err(worker_error)?;let next=(rows.len()>cap).then(||rows[cap-1].prefix.clone());let mut out=Vec::new();
        for row in rows.into_iter().take(cap){if row.closed!=0&&let Some(owner)=row.owner.filter(|owner|owner.starts_with("remove-v1:")){af::parse_removal_owner(&owner)?;out.push((ObjectPrefix::new(row.prefix)?,owner,row.epoch));}}Ok((out,next))
    })}
    fn rebind_writer<'a>(&'a self,prefix:&'a ObjectPrefix,expected_epoch:u64)->PlatformFuture<'a,Result<WriteFence>>{Box::pin(async move{
        #[derive(Deserialize)]struct Epoch{epoch:u64}let key=prefix.as_ref().trim_end_matches('/');let results=self.db.primary_view().batch(vec![self.prepare("INSERT OR IGNORE INTO novel_access_gates(prefix,updated_at) VALUES (?,?)",vec![key.into(),JsValue::from_f64(ts::now_secs() as f64)])?,self.prepare("SELECT epoch FROM novel_access_gates WHERE prefix=? AND closed=0 AND epoch=?",vec![key.into(),JsValue::from_f64(expected_epoch as f64)])?]).await.map_err(worker_error)?;crate::d1_repository::ensure_batch_success(&results)?;
        let epoch=results[1].results::<Epoch>().map_err(worker_error)?.into_iter().next().ok_or_else(||af::gate_closed_error(prefix))?.epoch;Ok(WriteFence::new(prefix.clone(),epoch))
    })}
    fn gate_info<'a>(&'a self,prefix:&'a ObjectPrefix)->PlatformFuture<'a,Result<GateInfo>>{Box::pin(async move{let row=self.gate_row(prefix).await?;Ok(GateInfo{epoch:row.epoch,content_revision:row.content_revision,state:if row.closed==0{GateState::Open}else{GateState::Closed},owner:row.owner,updated_at:row.updated_at,pending_readers:row.pending_readers})})}
    fn admit_reader<'a>(&'a self,prefix:&'a ObjectPrefix,id:&'a str,ttl:i64)->PlatformFuture<'a,Result<GateLease>>{Box::pin(async move{
        #[derive(Deserialize)]struct Lease{epoch:u64,expires_at:i64}let key=prefix.as_ref().trim_end_matches('/');let now=ts::now_secs();let expires=now.saturating_add(ttl.clamp(1,af::READER_LEASE_SECS));
        // Sweep this prefix's expired rows in the same batch so a busy prefix
        // cannot accumulate dead leases (`novel_reader_leases_expiry` index).
        let results=self.checked_batch(vec![self.prepare("INSERT OR IGNORE INTO novel_access_gates(prefix,updated_at) VALUES (?,?)",vec![key.into(),JsValue::from_f64(now as f64)])?,
            self.prepare("INSERT INTO novel_reader_leases(prefix,reader_id,epoch,expires_at) SELECT ?,?,epoch,? FROM novel_access_gates WHERE prefix=? AND closed=0 ON CONFLICT(prefix,reader_id) DO UPDATE SET epoch=excluded.epoch,expires_at=excluded.expires_at RETURNING epoch,expires_at",vec![key.into(),id.into(),JsValue::from_f64(expires as f64),key.into()])?,
            self.prepare("DELETE FROM novel_reader_leases WHERE prefix=? AND expires_at<=?",vec![key.into(),JsValue::from_f64(now as f64)])?]).await?;
        let lease=results[1].results::<Lease>().map_err(worker_error)?.into_iter().next().ok_or_else(||af::gate_closed_error(prefix))?;Ok(GateLease{reader_id:id.into(),epoch:lease.epoch,expires_at:lease.expires_at})
    })}
    fn renew_reader<'a>(&'a self,prefix:&'a ObjectPrefix,lease:&'a GateLease,ttl:i64)->PlatformFuture<'a,Result<GateLease>>{Box::pin(async move{
        #[derive(Deserialize)]struct Renewed{expires_at:i64}#[derive(Deserialize)]struct LeaseEpoch{epoch:u64}#[derive(Deserialize)]struct GateRow{closed:i64,epoch:u64}let now=ts::now_secs();let expires=now.saturating_add(ttl.clamp(1,af::READER_LEASE_SECS));let key=prefix.as_ref().trim_end_matches('/');
        let result=self.prepare("UPDATE novel_reader_leases SET expires_at=? WHERE prefix=? AND reader_id=? AND epoch=? AND expires_at>? AND EXISTS(SELECT 1 FROM novel_access_gates WHERE prefix=? AND closed=0 AND epoch=?) RETURNING expires_at",vec![JsValue::from_f64(expires as f64),key.into(),lease.reader_id.as_str().into(),JsValue::from_f64(lease.epoch as f64),JsValue::from_f64(now as f64),key.into(),JsValue::from_f64(lease.epoch as f64)])?.run().await.map_err(worker_error)?;crate::d1_repository::ensure_batch_success(std::slice::from_ref(&result))?;
        let Some(row)=result.results::<Renewed>().map_err(worker_error)?.into_iter().next()else{
            // No live row matched. Distinguish the gate boundary from a dead
            // admission: a closed/advanced gate (or a same-id row at another
            // epoch) is a stale token (`novel_recovering`), while a gate still
            // open at this epoch with a missing/expired row is a dead lease
            // (`novel_lease_expired`). Read both rows in one batch so the
            // classification cannot race a close that lands between probes.
            let probe=self.db.primary_view().batch(vec![
                self.prepare("SELECT closed,epoch FROM novel_access_gates WHERE prefix=?",vec![key.into()])?,
                self.prepare("SELECT epoch FROM novel_reader_leases WHERE prefix=? AND reader_id=?",vec![key.into(),lease.reader_id.as_str().into()])?,
            ]).await.map_err(worker_error)?;crate::d1_repository::ensure_batch_success(&probe)?;
            let gate=probe[0].results::<GateRow>().map_err(worker_error)?.into_iter().next();
            match gate {
                Some(gate) if gate.closed==0&&gate.epoch==lease.epoch => {
                    let row=probe[1].results::<LeaseEpoch>().map_err(worker_error)?.into_iter().next();
                    match row {
                        Some(existing) if existing.epoch!=lease.epoch => return Err(af::gate_closed_error(prefix)),
                        _ => return Err(af::lease_expired_error(prefix)),
                    }
                }
                _ => return Err(af::gate_closed_error(prefix)),
            }
        };
        Ok(GateLease{reader_id:lease.reader_id.clone(),epoch:lease.epoch,expires_at:row.expires_at})
    })}
    fn release_reader<'a>(&'a self,prefix:&'a ObjectPrefix,lease:&'a GateLease)->PlatformFuture<'a,Result<()>>{Box::pin(async move{let result=self.prepare("DELETE FROM novel_reader_leases WHERE prefix=? AND ((reader_id=? AND epoch=?) OR expires_at<=?)",vec![prefix.as_ref().trim_end_matches('/').into(),lease.reader_id.as_str().into(),JsValue::from_f64(lease.epoch as f64),JsValue::from_f64(ts::now_secs() as f64)])?.run().await.map_err(worker_error)?;crate::d1_repository::ensure_batch_success(&[result])})}
    fn close_gate<'a>(&'a self,prefix:&'a ObjectPrefix,owner:&'a str)->PlatformFuture<'a,Result<u64>>{Box::pin(async move{
        let key=prefix.as_ref().trim_end_matches('/');let results=self.checked_batch(vec![self.prepare("INSERT OR IGNORE INTO novel_access_gates(prefix,updated_at) VALUES (?,?)",vec![key.into(),JsValue::from_f64(ts::now_secs() as f64)])?,
            self.prepare("UPDATE novel_access_gates SET closed=1,owner=?,epoch=epoch+1,updated_at=? WHERE prefix=? AND closed=0",vec![owner.into(),JsValue::from_f64(ts::now_secs() as f64),key.into()])?,
            self.prepare("SELECT epoch,closed,owner,updated_at,content_revision,0 AS pending_readers FROM novel_access_gates WHERE prefix=?",vec![key.into()])?]).await?;
        let row=results[2].results::<DbGate>().map_err(worker_error)?.into_iter().next().ok_or_else(||worker_error("missing closed gate"))?;if row.owner.as_deref()!=Some(owner){return Err(af::gate_owner_error(prefix,row.owner.as_deref().unwrap_or("")));}Ok(row.epoch)
    })}
    fn renew_gate<'a>(&'a self,prefix:&'a ObjectPrefix,owner:&'a str,epoch:u64)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        let result=self.prepare("UPDATE novel_access_gates SET updated_at=? WHERE prefix=? AND closed=1 AND owner=? AND epoch=? RETURNING epoch,closed,owner,updated_at,content_revision,0 AS pending_readers",vec![JsValue::from_f64(ts::now_secs() as f64),prefix.as_ref().trim_end_matches('/').into(),owner.into(),JsValue::from_f64(epoch as f64)])?.run().await.map_err(worker_error)?;crate::d1_repository::ensure_batch_success(std::slice::from_ref(&result))?;if result.results::<DbGate>().map_err(worker_error)?.is_empty(){return Err(af::gate_epoch_error(prefix,epoch,epoch));}Ok(())
    })}
    fn open_gate<'a>(&'a self,prefix:&'a ObjectPrefix,owner:&'a str,epoch:u64)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        let clear_origin=owner.starts_with("remove-v1:")&&af::parse_removal_owner(owner)?.1;let result=self.prepare("UPDATE novel_access_gates SET closed=0,owner=NULL,updated_at=?1,origin_url_fold=CASE WHEN ?5 THEN '' ELSE origin_url_fold END WHERE prefix=?2 AND closed=1 AND owner=?3 AND epoch=?4 RETURNING epoch,closed,owner,updated_at,content_revision,0 AS pending_readers",vec![JsValue::from_f64(ts::now_secs() as f64),prefix.as_ref().trim_end_matches('/').into(),owner.into(),JsValue::from_f64(epoch as f64),JsValue::from_bool(clear_origin)])?.run().await.map_err(worker_error)?;crate::d1_repository::ensure_batch_success(std::slice::from_ref(&result))?;if result.results::<DbGate>().map_err(worker_error)?.is_empty(){return Err(af::gate_epoch_error(prefix,epoch,epoch));}Ok(())
    })}
    fn commit_staged<'a>(&'a self,prefix:&'a ObjectPrefix,owner:&'a str,epoch:u64,stage:&'a ObjectKey,key:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        if !prefix.matches(key)||!narou_rs::platform::is_reserved_key(stage){return Err(worker_error("invalid fenced install namespace"));}
        let sealed=self.sealed_object(stage).await?;let token=crate::content_store::nonce()?;let mut statements=self.publication_guard(prefix,owner,epoch,&token)?;
        self.push_transfer_reference_statements(prefix,owner,epoch,&token,key,&sealed,&mut statements)?;statements.push(self.finish_publication_guard(&token)?);
        self.checked_batch(statements).await.map(|_|())
    })}
    fn remove_committed<'a>(&'a self,prefix:&'a ObjectPrefix,owner:&'a str,epoch:u64,key:&'a ObjectKey,sha:&'a str)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        if !prefix.matches(key){return Err(worker_error("rollback outside novel"));}
        let Some(reference)=self.transfer_reference(key).await?else{return Ok(());};if !reference.sha256.eq_ignore_ascii_case(sha){return Err(worker_error("recovery fingerprint conflict"));}
        let token=crate::content_store::nonce()?;let mut statements=self.publication_guard(prefix,owner,epoch,&token)?;
        statements.push(self.prepare("SELECT CASE WHEN EXISTS(SELECT 1 FROM transfer_object_refs r WHERE r.object_key=?1 AND r.blob_key=?2 AND r.sha256=?3 AND r.blob_generation=?4 AND (r.backend!='d1' OR EXISTS(SELECT 1 FROM objects o WHERE o.object_key=r.blob_key AND o.generation=r.blob_generation))) THEN 1 ELSE abs(-9223372036854775808) END",vec![key.as_ref().into(),reference.blob_key.as_str().into(),reference.sha256.as_str().into(),reference.blob_generation.as_str().into()])?);
        statements.push(self.prepare("DELETE FROM transfer_object_refs WHERE object_key=? AND blob_key=? AND sha256=? AND blob_generation=?",vec![key.as_ref().into(),reference.blob_key.as_str().into(),reference.sha256.as_str().into(),reference.blob_generation.as_str().into()])?);statements.push(self.finish_publication_guard(&token)?);self.checked_batch(statements).await.map(|_|())
    })}
}

impl ObjectStore for D1ObjectStore {
    fn delete_closed<'a>(&'a self,prefix:&'a ObjectPrefix,owner:&'a str,epoch:u64,key:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        af::parse_removal_owner(owner)?;if !prefix.matches(key){return Err(worker_error("closed deletion outside novel"));}let token=crate::content_store::nonce()?;let mut statements=self.publication_guard(prefix,owner,epoch,&token)?;
        statements.push(self.prepare("INSERT INTO content_garbage(blob_key,not_before) SELECT blob_key,? FROM content_objects WHERE object_key=? ON CONFLICT(blob_key) DO UPDATE SET not_before=excluded.not_before",vec![chrono::Utc::now().to_rfc3339().into(),key.as_ref().into()])?);
        statements.push(self.prepare("DELETE FROM content_objects WHERE object_key=?",vec![key.as_ref().into()])?);
        statements.push(self.prepare("DELETE FROM object_chunks WHERE object_key=?",vec![key.as_ref().into()])?);statements.push(self.prepare("DELETE FROM objects WHERE object_key=?",vec![key.as_ref().into()])?);
        statements.push(self.prepare("INSERT INTO transfer_object_refs(object_key,blob_key,size,sha256,backend,updated_at,blob_generation) VALUES (?,'',0,'','deleted',?,'') ON CONFLICT(object_key) DO UPDATE SET blob_key='',size=0,sha256='',backend='deleted',updated_at=excluded.updated_at,blob_generation=''",vec![key.as_ref().into(),chrono::Utc::now().to_rfc3339().into()])?);statements.push(self.finish_publication_guard(&token)?);self.checked_batch(statements).await.map(|_|())
    })}
    fn retire_legacy_guarded<'a>(&'a self,fence:&'a WriteFence,logical:&'a ObjectKey,pool:&'a ObjectKey,size:u64,sha:&'a str)->PlatformFuture<'a,Result<()>>{Box::pin(async move{let base=std::sync::Arc::new(self.clone());crate::content_store::TransferReferenceStore::new(self.db.clone(),base.clone(),base.clone(),base,None).retire_legacy_guarded(fence,logical,pool,size,sha).await})}
    fn update_small_guarded<'a,'data:'a>(&'a self,fence:&'a WriteFence,key:&'a ObjectKey,mutate:&'a mut SmallObjectMutator<'data>)->PlatformFuture<'a,Result<ObjectUpdateResult>>{Box::pin(self.update_control(key,mutate,Some(fence)))}
    fn access_fence(&self)->Option<&dyn NovelAccessFence>{Some(self)}
    fn read_stream_leased<'a>(&'a self,prefix:&'a ObjectPrefix,lease:&'a GateLease,key:&'a ObjectKey,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<AssetStream>>>{Box::pin(async move{if !prefix.matches(key){return Err(worker_error("reader outside novel"));}self.renew_reader(prefix,lease,af::READER_LEASE_SECS).await?;let stream=self.read_stream_range(key,range).await?;self.renew_reader(prefix,lease,af::READER_LEASE_SECS).await?;Ok(stream)})}
    fn write_small_guarded<'a>(&'a self,fence:&'a WriteFence,key:&'a ObjectKey,data:Vec<u8>)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        if !fence.prefix.matches(key)||data.len() as u64>SMALL_CAP{return Err(worker_error("invalid guarded small write"));}let token=crate::content_store::nonce()?;let mut statements=vec![self.open_guard(fence,&token)?];self.append_write_statements(key,&data,None,&mut statements)?;statements.push(self.finish_publication_guard(&token)?);self.checked_batch(statements).await.map(|_|())
    })}
    fn delete_guarded<'a>(&'a self,fence:&'a WriteFence,key:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        if !fence.prefix.matches(key){return Err(worker_error("delete outside fence"));}let token=crate::content_store::nonce()?;let statements=vec![self.open_guard(fence,&token)?,
            self.prepare("DELETE FROM transfer_object_refs WHERE object_key=?",vec![key.as_ref().into()])?,self.prepare("DELETE FROM object_chunks WHERE object_key=?",vec![key.as_ref().into()])?,self.prepare("DELETE FROM objects WHERE object_key=?",vec![key.as_ref().into()])?,
            self.prepare("INSERT INTO transfer_object_refs(object_key,blob_key,size,sha256,backend,updated_at) VALUES (?,'',0,'','deleted',?) ON CONFLICT(object_key) DO UPDATE SET blob_key='',size=0,sha256='',backend='deleted',updated_at=excluded.updated_at",vec![key.as_ref().into(),chrono::Utc::now().to_rfc3339().into()])?,
            self.finish_publication_guard(&token)?];self.checked_batch(statements).await.map(|_|())
    })}

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
            if let Some(reference)=self.transfer_reference(key).await?{
                use futures::StreamExt;if reference.backend=="deleted"{return Ok(None);}if reference.size>SMALL_CAP{return Err(worker_error("reference exceeds small read cap"));}
                let mut stream=self.logical_stream_range(key,0..u64::MAX).await?.ok_or_else(||worker_error("reference disappeared"))?;let mut bytes=Vec::with_capacity(reference.size as usize);while let Some(chunk)=stream.next().await{bytes.extend_from_slice(&chunk?);}return Ok(Some(bytes));
            }
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
            if let Some(sha)=ts::hash_named_digest(key){let size=data.len() as u64;return self.write_stream_once(key,Box::pin(futures::stream::once(async move{Ok(data)})),size,&sha).await.map(|_|());}
            if let Some(fence)=self.writer_for_key(key).await?{return self.write_small_guarded(&fence,key,data).await;}
            self.write_all(key, &data).await
        })
    }

    fn update_small<'a, 'data: 'a>(
        &'a self,
        key: &'a ObjectKey,
        mutate: &'a mut SmallObjectMutator<'data>,
    ) -> PlatformFuture<'a, Result<ObjectUpdateResult>> {
        Box::pin(async move { let fence=self.writer_for_key(key).await?;self.update_control(key,mutate,fence.as_ref()).await })
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move { match self.writer_for_key(key).await?{Some(fence)=>self.delete_guarded(&fence,key).await,None=>self.delete_all(key).await} })
    }

    fn list_page<'a>(
        &'a self,
        request: &'a ObjectListRequest,
    ) -> PlatformFuture<'a, Result<ObjectListPage>> {
        Box::pin(async move {
            let rows = self.list_keys(request).await?;
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
    fn write_stream_guarded<'a>(&'a self,fence:&'a WriteFence,key:&'a ObjectKey,stream:AssetStream)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        if !fence.prefix.matches(key){return Err(worker_error("write outside fence"));}let stage=self.stage_stream(stream).await?;let token=crate::content_store::nonce()?;
        let statements=vec![self.open_guard(fence,&token)?,self.prepare("INSERT INTO transfer_object_refs(object_key,blob_key,size,sha256,backend,updated_at,blob_generation) SELECT ?1,?2,?3,?4,'d1',?5,(SELECT v.generation FROM transfer_verified_objects v JOIN objects o ON o.object_key=v.blob_key AND o.generation=v.generation WHERE v.blob_key=?2 AND v.size=?3 AND v.sha256=?4) ON CONFLICT(object_key) DO UPDATE SET blob_key=excluded.blob_key,size=excluded.size,sha256=excluded.sha256,backend='d1',updated_at=excluded.updated_at,blob_generation=excluded.blob_generation",vec![key.as_ref().into(),stage.key.as_ref().into(),JsValue::from_f64(stage.size as f64),stage.sha256.as_str().into(),chrono::Utc::now().to_rfc3339().into()])?,self.finish_publication_guard(&token)?];
        self.checked_batch(statements).await.map(|_|())
    })}
    fn write_stream_once<'a>(&'a self,key:&'a ObjectKey,stream:AssetStream,expected_size:u64,sha:&'a str)->PlatformFuture<'a,Result<narou_rs::platform::WriteOnce>>{Box::pin(async move{
        use futures::StreamExt;use narou_rs::platform::WriteOnce;ts::check_hash_named_key(key,sha)?;let id=format!("once-{}",crate::content_store::nonce()?);
        let sealed=ts::stage_asset_stream(self,&id,&id,1,24*60*60,stream).await?;let size=if expected_size==u64::MAX{sealed.size}else{expected_size};
        if sealed.size!=size||!sealed.sha256.eq_ignore_ascii_case(sha){let _=self.stage_abort(&id,&id,1).await;return Err(worker_error("write-once size/SHA mismatch"));}
        #[derive(Deserialize)]struct Written{object_key:String}
        let result=self.prepare("INSERT INTO transfer_object_refs(object_key,blob_key,size,sha256,backend,updated_at,blob_generation) SELECT ?1,?2,?3,?4,'d1',?5,(SELECT v.generation FROM transfer_verified_objects v JOIN objects o ON o.object_key=v.blob_key AND o.generation=v.generation WHERE v.blob_key=?2 AND v.size=?3 AND v.sha256=?4) WHERE NOT EXISTS(SELECT 1 FROM objects WHERE object_key=?1) ON CONFLICT(object_key) DO NOTHING RETURNING object_key",vec![key.as_ref().into(),sealed.key.as_ref().into(),JsValue::from_f64(size as f64),sha.into(),chrono::Utc::now().to_rfc3339().into()])?.run().await.map_err(worker_error)?;crate::d1_repository::ensure_batch_success(std::slice::from_ref(&result))?;
        if result.results::<Written>().map_err(worker_error)?.iter().any(|row|row.object_key==key.as_ref()){return Ok(WriteOnce::Written);}
        let mut existing=self.logical_stream_range(key,0..u64::MAX).await?.ok_or_else(||worker_error("write-once collision disappeared"))?;let mut hash=Sha256::new();let mut seen=0u64;
        while let Some(chunk)=existing.next().await{let bytes=chunk?;seen=seen.checked_add(bytes.len() as u64).filter(|value|*value<=size).ok_or_else(||worker_error("write-once existing length differs"))?;hash.update(bytes);}
        self.stage_abort(&id,&id,1).await?;if seen!=size||!hex::encode(hash.finalize()).eq_ignore_ascii_case(sha){return Err(worker_error("write-once existing bytes differ"));}Ok(WriteOnce::Reused)
    })}

    fn read_stream_range<'a>(&'a self,key:&'a ObjectKey,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<AssetStream>>>{Box::pin(self.logical_stream_range(key,range))}

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
        Box::pin(self.logical_stream_range(key,0..u64::MAX))
    }

    fn read_range<'a>(
        &'a self, key: &'a ObjectKey, range: std::ops::Range<u64>,
    ) -> PlatformFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(async move{
            use futures::StreamExt;let length=narou_rs::platform::staging::validate_range(&range)?;let Some(mut stream)=self.logical_stream_range(key,range).await?else{return Ok(None);};let mut bytes=Vec::with_capacity(length);while let Some(chunk)=stream.next().await{bytes.extend_from_slice(&chunk?);}if bytes.len()!=length{return Err(worker_error("asset range truncated"));}Ok(Some(bytes))
        })
    }

    fn stage_stream<'a>(&'a self, stream: AssetStream) -> PlatformFuture<'a, Result<narou_rs::platform::StagedAsset>> {
        Box::pin(async move {
            let id=format!("ordinary-{}",crate::content_store::nonce()?);let sealed=ts::stage_asset_stream(self,&id,&id,1,narou_rs::platform::staging::STAGE_TTL_SECONDS,stream).await?;
            Ok(narou_rs::platform::StagedAsset{key:sealed.key,size:sealed.size,sha256:sealed.sha256})
        })
    }

    fn write_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
        stream: AssetStream,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move{if let Some(sha)=ts::hash_named_digest(key){return self.write_stream_once(key,stream,u64::MAX,&sha).await.map(|_|());}if let Some(fence)=self.writer_for_key(key).await?{self.write_stream_guarded(&fence,key,stream).await}else{self.write_asset(key,stream).await}})
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<()>> {
        ObjectStore::delete(self,key)
    }

    fn copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(self.copy_asset(source, destination, false))
    }

    fn move_or_copy<'a>(
        &'a self,
        source: &'a ObjectKey,
        destination: &'a ObjectKey,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(self.copy_asset(source, destination, true))
    }
}

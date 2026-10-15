//! Worker episode bodies: immutable compressed S3 revisions, canonical D1 refs.
//! Metadata and version markers remain in the original D1 object store.

use std::sync::Arc;
use chrono::{Duration, Utc};
use futures::StreamExt;
use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{
    AssetStore, AssetStream, ObjectEncoding, ObjectKey, ObjectListPage, ObjectListRequest,
    ObjectMetadata, ObjectStore, ObjectUpdateResult, PlatformFuture, SmallObjectMutator,
    content_type_for_key, decode_object_payload_limited, encode_object_payload,
    object_crc32, prefix_upper_bound, verify_object_crc32,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use worker::wasm_bindgen::{JsCast, JsValue};
use crate::{db_handle::DbHandle, d1_object_store::D1ObjectStore};

pub(crate) mod sql;
const CAP: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum ContentBackend {
    #[serde(rename = "d1")]
    D1,
    #[serde(rename = "migrating")]
    Migrating,
    #[serde(rename = "s3")]
    S3,
}
impl ContentBackend {
    pub fn as_str(self) -> &'static str {
        match self { Self::D1 => "d1", Self::Migrating => "migrating", Self::S3 => "s3" }
    }
}

fn error(value: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(format!("content storage: {value}"))
}

pub async fn backend(db: &DbHandle) -> Result<ContentBackend> {
    #[derive(Deserialize)]
    struct Row { value_json: String }
    let row = db.prepare("SELECT value_json FROM app_state WHERE scope='inv' AND key='content_backend'")
        .first::<Row>(None).await.map_err(error)?;
    let Some(row) = row else { return Ok(ContentBackend::D1); };
    let value = match row.value_json.as_str() {
        "d1" | "migrating" | "s3" => row.value_json,
        _ => serde_json::from_str::<String>(&row.value_json).map_err(|_| error("invalid content_backend marker"))?,
    };
    match value.as_str() {
        "d1" => Ok(ContentBackend::D1),
        "migrating" => Ok(ContentBackend::Migrating),
        "s3" => Ok(ContentBackend::S3),
        _ => Err(error("content_backend must be d1, migrating, or s3")),
    }
}

pub fn is_content_key(key: &ObjectKey) -> bool {
    let value = key.as_ref();
    if !value.starts_with("novels/") || !value.ends_with(".yaml") { return false; }
    let mut tail = value.rsplit('/');
    tail.next();
    let parent = tail.next();
    if parent == Some("本文") { return tail.count() >= 3; }
    matches!(tail.next(), Some("cache" | ".cache")) && tail.next() == Some("本文") && tail.count() >= 3
}

pub fn nonce() -> Result<String> {
    let crypto = js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("crypto")).map_err(|_| error("Web Crypto unavailable"))?;
    let random = js_sys::Reflect::get(&crypto, &JsValue::from_str("getRandomValues"))
        .map_err(|_| error("Web Crypto unavailable"))?.dyn_into::<js_sys::Function>()
        .map_err(|_| error("Web Crypto unavailable"))?;
    let bytes = js_sys::Uint8Array::new_with_length(16);
    random.call1(&crypto, &bytes).map_err(|_| error("cannot generate content revision"))?;
    Ok(hex::encode(bytes.to_vec()))
}

#[derive(Clone, Deserialize)]
struct Reference {
    object_key: String,
    blob_key: String,
    size: u64,
    stored_size: u64,
    encoding: String,
    crc32: u32,
    sha256: String,
    updated_at: String,
    content_type: Option<String>,
    verified: i64,
}
#[derive(Deserialize)]
struct MetadataRow { object_key: String, size: u64, updated_at: String, content_type: Option<String> }
impl MetadataRow {
    fn metadata(self) -> Result<ObjectMetadata> {
        Ok(ObjectMetadata {
            key: ObjectKey::try_new(self.object_key)?, size: self.size, etag: None,
            last_modified: chrono::DateTime::parse_from_rfc3339(&self.updated_at).ok().map(|time| time.with_timezone(&Utc)),
            content_type: self.content_type,
        })
    }
}
#[derive(Deserialize)]
struct KeyRow { object_key: String }
#[derive(Deserialize)]
struct CommitRow { committed: i64 }

#[derive(Clone)]
pub struct ContentStore {
    db: DbHandle,
    legacy: Arc<D1ObjectStore>,
    remote: Arc<dyn ObjectStore>,
    remote_assets: Arc<dyn AssetStore>,
}

impl ContentStore {
    pub fn new(db: DbHandle, remote: Arc<dyn ObjectStore>, remote_assets: Arc<dyn AssetStore>) -> Self {
        // Keep the TOC and its body references off independently lagging replicas.
        let db = db.primary_view();
        Self { legacy: Arc::new(D1ObjectStore::new(db.clone())), db, remote, remote_assets }
    }
    fn statement(&self, query: &str, values: &[JsValue]) -> Result<worker::D1PreparedStatement> {
        self.db.prepare(query).bind(values).map_err(error)
    }
    async fn reference(&self, key: &ObjectKey) -> Result<Option<Reference>> {
        self.statement("SELECT * FROM content_objects WHERE object_key=?", &[key.as_ref().into()])?
            .first(None).await.map_err(error)
    }
    async fn read_reference(&self, reference: &Reference) -> Result<Vec<u8>> {
        if reference.size > CAP as u64 || reference.stored_size > CAP as u64 { return Err(error("episode exceeds read cap")); }
        let stored = self.remote.read_small(&ObjectKey::try_new(reference.blob_key.clone())?).await?
            .ok_or_else(|| error("referenced S3 episode is missing"))?;
        if stored.len() as u64 != reference.stored_size { return Err(error("stored episode size mismatch")); }
        let decoded = decode_object_payload_limited(&stored, ObjectEncoding::parse(&reference.encoding)?, CAP)?;
        if decoded.len() as u64 != reference.size { return Err(error("episode size mismatch")); }
        verify_object_crc32(&decoded, reference.crc32)?;
        if hex::encode(Sha256::digest(&decoded)) != reference.sha256 { return Err(error("episode SHA-256 mismatch")); }
        Ok(decoded.into_owned())
    }

    // expected: '*' = unconditional; '' = absent; otherwise exact immutable blob.
    async fn publish(&self, key: &ObjectKey, data: Vec<u8>, expected: &str, legacy_time: &str, verified: bool) -> Result<bool> {
        if data.len() > CAP { return Err(error("episode exceeds write cap")); }
        let size = data.len();
        let crc = object_crc32(&data);
        let digest = hex::encode(Sha256::digest(&data));
        let (encoded, encoding) = encode_object_payload(&data);
        let encoding = encoding.as_str();
        let stored = match encoded { std::borrow::Cow::Owned(bytes) => bytes, std::borrow::Cow::Borrowed(_) => data };
        let (blob, now) = self.reserve_revision().await?;
        let stored_size = stored.len();
        self.remote.write_small(&blob, stored).await?;
        let reference = Reference {
            object_key: key.to_string(), blob_key: blob.to_string(), size: size as u64,
            stored_size: stored_size as u64, encoding: encoding.to_string(), crc32: crc,
            sha256: digest, updated_at: now, content_type: content_type_for_key(key).map(str::to_owned),
            verified: i64::from(verified),
        };
        self.commit_reference(&reference, expected, legacy_time).await
    }

    async fn reserve_revision(&self) -> Result<(ObjectKey, String)> {
        let blob = ObjectKey::try_new(format!("content/{}.bin", nonce()?))?;
        let reserved_until = (Utc::now() + Duration::hours(1)).to_rfc3339();
        self.statement("INSERT INTO content_garbage(blob_key,not_before) VALUES (?,?)", &[blob.as_ref().into(), reserved_until.into()])?
            .run().await.map_err(error)?;
        Ok((blob, Utc::now().to_rfc3339()))
    }

    async fn commit_reference(&self, row: &Reference, expected: &str, legacy_time: &str) -> Result<bool> {
        let blob = row.blob_key.as_str();
        let values = [row.object_key.as_str().into(), blob.into(), JsValue::from_f64(row.size as f64),
            JsValue::from_f64(row.stored_size as f64), row.encoding.as_str().into(), JsValue::from_f64(row.crc32 as f64),
            row.sha256.as_str().into(), row.updated_at.as_str().into(),
            row.content_type.as_deref().map(JsValue::from_str).unwrap_or(JsValue::NULL),
            JsValue::from_f64(row.verified as f64), expected.into(), legacy_time.into()];
        let results = self.db.batch(vec![
            self.statement(sql::QUEUE_PREVIOUS, &values)?,
            self.statement(sql::PUBLISH, &values)?,
            self.statement("DELETE FROM content_garbage WHERE blob_key=? AND EXISTS(SELECT 1 FROM content_objects WHERE blob_key=?)", &[blob.into(),blob.into()])?,
            self.statement("SELECT COUNT(*) AS committed FROM content_objects WHERE object_key=? AND blob_key=?", &[row.object_key.as_str().into(),blob.into()])?,
        ]).await.map_err(error)?;
        crate::d1_repository::ensure_batch_success(&results)?;
        let rows = results.last().ok_or_else(|| error("missing publication result"))?.results::<CommitRow>().map_err(error)?;
        let committed = rows.iter().any(|row| row.committed == 1);
        if !committed {
            // No reference can point at this unique unpublished revision.
            self.statement("INSERT INTO content_garbage(blob_key,not_before) VALUES (?,?) ON CONFLICT(blob_key) DO UPDATE SET not_before=excluded.not_before", &[blob.into(),Utc::now().to_rfc3339().into()])?
                .run().await.map_err(error)?;
        }
        Ok(committed)
    }

    async fn remove_content(&self, key: &ObjectKey, expected: &str) -> Result<bool> {
        let now = Utc::now().to_rfc3339();
        let values = [key.as_ref().into(),expected.into(),now.into()];
        let one = [JsValue::from_str(key.as_ref())];
        let results = self.db.batch(vec![
            self.statement(sql::QUEUE_DELETED, &values)?,
            self.statement("INSERT INTO content_deletions(object_key) SELECT ?1 WHERE ?2='*' OR COALESCE((SELECT blob_key FROM content_objects WHERE object_key=?1),'')=?2", &values[..2])?,
            self.statement("DELETE FROM object_chunks WHERE object_key=? AND EXISTS(SELECT 1 FROM content_deletions WHERE object_key=?)", &[one[0].clone(),one[0].clone()])?,
            self.statement("DELETE FROM objects WHERE object_key=? AND EXISTS(SELECT 1 FROM content_deletions WHERE object_key=?)", &[one[0].clone(),one[0].clone()])?,
            self.statement("DELETE FROM content_objects WHERE object_key=? AND EXISTS(SELECT 1 FROM content_deletions WHERE object_key=?)", &[one[0].clone(),one[0].clone()])?,
            self.statement("DELETE FROM content_deletions WHERE object_key=? RETURNING object_key", &one)?,
        ]).await.map_err(error)?;
        crate::d1_repository::ensure_batch_success(&results)?;
        Ok(results.last().ok_or_else(|| error("missing deletion result"))?.results::<KeyRow>().map_err(error)?.iter().any(|row| row.object_key == key.as_ref()))
    }

    pub async fn begin_migration(&self) -> Result<()> {
        backend(&self.db).await?;
        self.db.prepare("INSERT INTO app_state(scope,key,value_json,value_yaml) VALUES ('inv','content_backend','\"migrating\"','migrating') ON CONFLICT(scope,key) DO UPDATE SET value_json=excluded.value_json,value_yaml=excluded.value_yaml WHERE CASE WHEN json_valid(app_state.value_json) THEN json_extract(app_state.value_json,'$') ELSE app_state.value_json END = 'd1'")
            .run().await.map_err(error)?;
        Ok(())
    }
    pub async fn legacy_page(&self, cursor: Option<&str>, limit: usize) -> Result<Vec<ObjectMetadata>> {
        self.page_from("legacy_content_objects",cursor,limit).await
    }
    pub async fn reference_page(&self, cursor: Option<&str>, limit: usize) -> Result<Vec<ObjectMetadata>> {
        self.page_from("content_objects",cursor,limit).await
    }
    async fn page_from(&self, table: &str, cursor: Option<&str>, limit: usize) -> Result<Vec<ObjectMetadata>> {
        let query = format!("SELECT object_key,size,updated_at,content_type FROM {table} WHERE object_key>? ORDER BY object_key LIMIT ?");
        let result = self.statement(&query,&[cursor.unwrap_or("").into(),JsValue::from_f64(limit as f64)])?
            .all().await.map_err(error)?;
        result.results::<MetadataRow>().map_err(error)?.into_iter().map(MetadataRow::metadata).collect()
    }
    pub async fn copy_legacy(&self, key: &ObjectKey) -> Result<bool> {
        let primary = self;
        if primary.reference(key).await?.is_some() { return Ok(false); }
        let row = primary.statement("SELECT object_key,size,updated_at,content_type FROM legacy_content_objects WHERE object_key=?", &[key.as_ref().into()])?
            .first::<MetadataRow>(None).await.map_err(error)?;
        let Some(row) = row else { return Ok(false); };
        let Some(data) = ObjectStore::read_small(primary.legacy.as_ref(),key).await? else { return Ok(false); };
        // Migration mode SQL guards make the legacy payload immutable. Concurrent
        // downloads may publish refs, but this absent-CAS must never replace them.
        primary.publish(key,data,"",&row.updated_at,false).await
    }
    pub async fn verify_reference(&self, key: &ObjectKey) -> Result<()> {
        let primary = self;
        let Some(reference) = primary.reference(key).await? else { return Ok(()); };
        primary.read_reference(&reference).await?;
        primary.statement("UPDATE content_objects SET verified=1 WHERE object_key=? AND blob_key=?", &[key.as_ref().into(),reference.blob_key.into()])?
            .run().await.map_err(error)?;
        Ok(())
    }
    pub async fn cutover(&self) -> Result<()> {
        if backend(&self.db).await? == ContentBackend::S3 { return Ok(()); }
        let result = self.db.prepare(sql::CUTOVER).all().await.map_err(error)?;
        if result.results::<KeyRow>().map_err(error)?.is_empty() { return Err(error("content cutover requires complete verified references")); }
        Ok(())
    }
    pub async fn purge_legacy_page(&self, limit: usize) -> Result<u64> {
        if backend(&self.db).await? != ContentBackend::S3 { return Err(error("legacy purge requires S3 cutover")); }
        let page = self.legacy_page(None,limit).await?;
        let mut removed = 0;
        for metadata in page {
            let key = metadata.key.as_ref();
            let values = [JsValue::from_str(key)];
            let results = self.db.batch(vec![
                self.statement("INSERT INTO content_deletions(object_key) SELECT ? WHERE EXISTS(SELECT 1 FROM content_objects WHERE object_key=? AND verified=1)", &[key.into(),key.into()])?,
                self.statement("DELETE FROM object_chunks WHERE object_key=? AND EXISTS(SELECT 1 FROM content_deletions WHERE object_key=?)", &[key.into(),key.into()])?,
                self.statement("DELETE FROM objects WHERE object_key=? AND EXISTS(SELECT 1 FROM content_deletions WHERE object_key=?) RETURNING object_key", &[key.into(),key.into()])?,
                self.statement("DELETE FROM content_deletions WHERE object_key=?", &values)?,
            ]).await.map_err(error)?;
            crate::d1_repository::ensure_batch_success(&results)?;
            removed += results[2].results::<KeyRow>().map_err(error)?.len() as u64;
        }
        Ok(removed)
    }
    pub async fn collect_garbage(&self, limit: usize) -> Result<u64> {
        #[derive(Deserialize)]
        struct Garbage { blob_key: String }
        let rows = self.statement("SELECT blob_key FROM content_garbage g WHERE julianday(not_before)<=julianday('now') AND NOT EXISTS(SELECT 1 FROM content_objects c WHERE c.blob_key=g.blob_key) ORDER BY blob_key LIMIT ?", &[JsValue::from_f64(limit as f64)])?
            .all().await.map_err(error)?.results::<Garbage>().map_err(error)?;
        let mut removed = 0;
        for row in rows {
            self.remote.delete(&ObjectKey::try_new(row.blob_key.clone())?).await?;
            self.statement("DELETE FROM content_garbage WHERE blob_key=? AND NOT EXISTS(SELECT 1 FROM content_objects WHERE blob_key=?)", &[row.blob_key.clone().into(),row.blob_key.into()])?
                .run().await.map_err(error)?;
            removed += 1;
        }
        Ok(removed)
    }
    async fn transfer(&self, source: &ObjectKey, destination: &ObjectKey, remove: bool) -> Result<()> {
        if !is_content_key(source) && !is_content_key(destination) {
            return if remove { self.legacy.move_or_copy(source,destination).await } else { self.legacy.copy(source,destination).await };
        }
        if is_content_key(source) != is_content_key(destination) { return Err(error("cross-kind content copy is unsupported")); }
        if source == destination { return Ok(()); }
        let primary = self;
        if backend(&primary.db).await? == ContentBackend::D1 {
            return if remove { primary.legacy.move_or_copy(source,destination).await } else { primary.legacy.copy(source,destination).await };
        }
        let reference = primary.reference(source).await?;
        let expected = reference.as_ref().map(|row| row.blob_key.as_str()).unwrap_or("");
        let committed = if let Some(row) = &reference {
            if row.verified == 0 { primary.read_reference(row).await?; }
            let (blob, now) = primary.reserve_revision().await?;
            primary.remote_assets.copy(&ObjectKey::try_new(row.blob_key.clone())?, &blob).await?;
            let mut copied = row.clone();
            copied.object_key = destination.to_string();
            copied.blob_key = blob.to_string();
            copied.updated_at = now;
            copied.verified = 1;
            primary.commit_reference(&copied, "*", "").await?
        } else {
            let Some(data) = ObjectStore::read_small(primary,source).await? else { return Ok(()); };
            primary.publish(destination,data,"*","",true).await?
        };
        if !committed { return Err(error("history publication conflict")); }
        if remove && !primary.remove_content(source,expected).await? { return Err(error("source changed during history move")); }
        primary.collect_garbage(4).await?;
        Ok(())
    }
}

impl ObjectStore for ContentStore {
    fn stat<'a>(&'a self,key:&'a ObjectKey) -> PlatformFuture<'a,Result<Option<ObjectMetadata>>> {
        Box::pin(async move {
            if is_content_key(key) {
                let primary = self;
                if let Some(row)=primary.reference(key).await? {
                    return MetadataRow { object_key:row.object_key,size:row.size,updated_at:row.updated_at,content_type:row.content_type }.metadata().map(Some);
                }
                if backend(&primary.db).await? == ContentBackend::S3 { return Ok(None); }
                return ObjectStore::stat(primary.legacy.as_ref(),key).await;
            }
            ObjectStore::stat(self.legacy.as_ref(),key).await
        })
    }
    fn read_small<'a>(&'a self,key:&'a ObjectKey) -> PlatformFuture<'a,Result<Option<Vec<u8>>>> {
        Box::pin(async move {
            if is_content_key(key) {
                let primary = self;
                if let Some(row)=primary.reference(key).await? { return primary.read_reference(&row).await.map(Some); }
                if backend(&primary.db).await? == ContentBackend::S3 { return Ok(None); }
                return primary.legacy.read_small(key).await;
            }
            self.legacy.read_small(key).await
        })
    }
    fn write_small<'a>(&'a self,key:&'a ObjectKey,data:Vec<u8>) -> PlatformFuture<'a,Result<()>> {
        Box::pin(async move {
            if !is_content_key(key) { return self.legacy.write_small(key,data).await; }
            let primary=self;
            if backend(&primary.db).await? == ContentBackend::D1 { return primary.legacy.write_small(key,data).await; }
            if !primary.publish(key,data,"*","",true).await? { return Err(error("episode publication conflict")); }
            primary.collect_garbage(4).await?;
            Ok(())
        })
    }
    fn update_small<'a,'data:'a>(&'a self,key:&'a ObjectKey,mutate:&'a mut SmallObjectMutator<'data>) -> PlatformFuture<'a,Result<ObjectUpdateResult>> {
        Box::pin(async move {
            if !is_content_key(key) { return self.legacy.update_small(key,mutate).await; }
            let primary=self;
            if backend(&primary.db).await? == ContentBackend::D1 { return primary.legacy.update_small(key,mutate).await; }
            let reference=primary.reference(key).await?;
            let legacy_time = if reference.is_none() {
                primary.statement("SELECT updated_at FROM legacy_content_objects WHERE object_key=?", &[key.as_ref().into()])?
                    .first::<String>(Some("updated_at")).await.map_err(error)?.unwrap_or_default()
            } else { String::new() };
            let current=match &reference { Some(row)=>Some(primary.read_reference(row).await?),None=>primary.read_small(key).await? };
            let Some(next)=mutate(current.as_deref())? else { return Ok(ObjectUpdateResult::Committed); };
            let committed=primary.publish(key,next.into_owned(),reference.as_ref().map(|row|row.blob_key.as_str()).unwrap_or(""),&legacy_time,true).await?;
            if committed { primary.collect_garbage(4).await?; }
            Ok(if committed { ObjectUpdateResult::Committed } else { ObjectUpdateResult::Conflict })
        })
    }
    fn delete<'a>(&'a self,key:&'a ObjectKey) -> PlatformFuture<'a,Result<()>> {
        Box::pin(async move {
            if !is_content_key(key) { return ObjectStore::delete(self.legacy.as_ref(),key).await; }
            let primary=self;
            if backend(&primary.db).await? == ContentBackend::D1 { return ObjectStore::delete(primary.legacy.as_ref(),key).await; }
            primary.remove_content(key,"*").await?;
            primary.collect_garbage(4).await?;
            Ok(())
        })
    }
    fn list_page<'a>(&'a self,request:&'a ObjectListRequest) -> PlatformFuture<'a,Result<ObjectListPage>> {
        Box::pin(async move {
            let mode=backend(&self.db.primary_view()).await?;
            let prefix=request.prefix.as_ref().trim_end_matches('/');
            let upper=prefix_upper_bound(prefix);
            let values=[prefix.into(),upper.into(),request.cursor.as_deref().unwrap_or("").into(),mode.as_str().into(),JsValue::from_f64(request.limit.get().saturating_add(1) as f64)];
            let mut rows=self.statement(sql::LIST,&values)?.all().await.map_err(error)?.results::<MetadataRow>().map_err(error)?;
            let mut objects=Vec::with_capacity(request.limit.get());
            let more=rows.len()>request.limit.get();
            rows.truncate(request.limit.get());
            for row in rows { objects.push(row.metadata()?); }
            let next_cursor=if more { objects.last().map(|row|row.key.to_string()) } else { None };
            Ok(ObjectListPage { objects,next_cursor })
        })
    }
}

impl AssetStore for ContentStore {
    fn stat<'a>(&'a self,key:&'a ObjectKey) -> PlatformFuture<'a,Result<Option<ObjectMetadata>>> { ObjectStore::stat(self,key) }
    fn read_stream<'a>(&'a self,key:&'a ObjectKey) -> PlatformFuture<'a,Result<Option<AssetStream>>> {
        Box::pin(async move {
            if !is_content_key(key) { return self.legacy.read_stream(key).await; }
            Ok(self.read_small(key).await?.map(|data| Box::pin(futures::stream::once(async move { Ok(data) })) as AssetStream))
        })
    }
    fn write_stream<'a>(&'a self,key:&'a ObjectKey,mut stream:AssetStream) -> PlatformFuture<'a,Result<()>> {
        Box::pin(async move {
            if !is_content_key(key) { return self.legacy.write_stream(key,stream).await; }
            let mut data=Vec::new();
            while let Some(chunk)=stream.next().await { let chunk=chunk?; if data.len().saturating_add(chunk.len())>CAP { return Err(error("episode exceeds stream cap")); } data.extend_from_slice(&chunk); }
            self.write_small(key,data).await
        })
    }
    fn delete<'a>(&'a self,key:&'a ObjectKey) -> PlatformFuture<'a,Result<()>> { ObjectStore::delete(self,key) }
    fn copy<'a>(&'a self,source:&'a ObjectKey,destination:&'a ObjectKey) -> PlatformFuture<'a,Result<()>> { Box::pin(async move { self.transfer(source,destination,false).await }) }
    fn move_or_copy<'a>(&'a self,source:&'a ObjectKey,destination:&'a ObjectKey) -> PlatformFuture<'a,Result<()>> { Box::pin(async move { self.transfer(source,destination,true).await }) }
}

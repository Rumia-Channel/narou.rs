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
    ObjectFingerprint, ObjectFingerprintMismatch,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use worker::wasm_bindgen::{JsCast, JsValue};
use crate::{db_handle::DbHandle, d1_object_store::D1ObjectStore};
use narou_rs::transfer::projection::{classify, ClassifyContext, FileRole};

pub(crate) fn metadata_in_d1(role: FileRole) -> bool {
    matches!(role, FileRole::Toc | FileRole::Metadata | FileRole::Settings | FileRole::IllustrationIndex)
}

fn role_for(fence: &narou_rs::platform::WriteFence, key: &ObjectKey) -> Result<FileRole> {
    let relative = key.as_ref().strip_prefix(fence.prefix.as_ref().trim_end_matches('/'))
        .ok_or_else(|| error("write outside captured fence"))?.trim_start_matches('/');
    Ok(classify(relative, false, &ClassifyContext::default()))
}

pub(crate) mod sql;

/// Outermost logical-ref view. Resolve transfer versions before SplitStore
/// routes illustrations or legacy content to their ordinary tiers.
#[derive(Clone)]
pub struct TransferReferenceStore{
    db:DbHandle,ordinary_objects:Arc<dyn ObjectStore>,ordinary_assets:Arc<dyn AssetStore>,
    d1:Arc<D1ObjectStore>,remote:Option<Arc<dyn AssetStore>>,
}
impl TransferReferenceStore{
    pub fn new(db:DbHandle,ordinary_objects:Arc<dyn ObjectStore>,ordinary_assets:Arc<dyn AssetStore>,d1:Arc<D1ObjectStore>,remote:Option<Arc<dyn AssetStore>>)->Self{
        Self{db:db.primary_view(),ordinary_objects,ordinary_assets,d1,remote}
    }
    fn storage(&self,backend:&str)->Result<&dyn AssetStore>{match backend{
        "d1"=>Ok(self.d1.as_ref()),
        "s3"=>self.remote.as_deref().ok_or_else(||error("referenced S3 storage is unavailable")),
        _=>Err(error("unknown transfer reference backend")),
    }}
    async fn reference_stream(&self,row:crate::d1_object_store::TransferReference,range:std::ops::Range<u64>)->Result<Option<AssetStream>>{
        if row.backend=="deleted"{return Ok(None);}
        let end=if range.end==u64::MAX{row.size}else{range.end};if range.start>end||end>row.size{return Err(error("reference range exceeds object"));}
        let full=range.start==0&&end==row.size;let expected=end-range.start;
        let stream=if row.backend=="d1"{self.d1.read_bound_reference(&row,range.start..end).await?}else{self.storage(&row.backend)?.read_stream_range(&ObjectKey::try_new(row.blob_key)?,range.start..end).await?}.ok_or_else(||error("immutable transfer reference is missing"))?;
        Ok(Some(Box::pin(futures::stream::try_unfold((stream,Sha256::new(),0u64,row.sha256),move|(mut stream,mut hash,seen,sha)|async move{
            match stream.next().await{Some(chunk)=>{let chunk=chunk?;let next=seen.checked_add(chunk.len() as u64).filter(|value|*value<=expected).ok_or_else(||error("reference range overflow"))?;if full{hash.update(&chunk);}Ok(Some((chunk,(stream,hash,next,sha))))},
                None=>{if seen!=expected||(full&&hex::encode(hash.finalize())!=sha){return Err(error("immutable reference size/SHA mismatch"));}Ok(None)}}
        }))))
    }
    async fn writer_for_key(&self,key:&ObjectKey)->Result<Option<narou_rs::platform::WriteFence>>{
        self.d1.writer_for_key(key).await
    }
    async fn publish_stream(&self,fence:&narou_rs::platform::WriteFence,key:&ObjectKey,stream:AssetStream)->Result<()>{
        if self.publish_stream_cas(fence,key,stream,None).await?{Ok(())}else{Err(error("immutable reference write conflict"))}
    }
    async fn publish_stream_cas(&self,fence:&narou_rs::platform::WriteFence,key:&ObjectKey,stream:AssetStream,expected:Option<&str>)->Result<bool>{
        if !fence.prefix.matches(key){return Err(error("write outside captured fence"));}
        let role = role_for(fence, key)?;
        let metadata = metadata_in_d1(role);
        let remote = self.remote.as_ref().filter(|_| !metadata);
        let token=nonce()?;let(blob,size,sha,backend)=if let Some(remote)=remote{
            if role==FileRole::Illustration{
                // Hash-addressed pool: the physical key derives from the
                // content SHA-256, so identical images share one immutable
                // object across novels. Pool objects are shared and exempt
                // from content_garbage; a failed ref commit keeps them.
                if let Some(claimed_sha)=narou_rs::platform::transfer_stage::hash_named_digest(key){
                    // The logical `挿絵/<sha256-hex>.<ext>` name already
                    // declares the content hash; `write_stream_once` rejects a
                    // stream whose bytes differ, so re-staging here would only
                    // re-copy what the downloader stage already wrote. Meter
                    // the stream anyway so the D1 reference row carries the
                    // measured size and SHA, never the claimed name alone.
                    let pool=narou_rs::platform::transfer_stage::illustration_pool_key(key)?;
                    let state=std::sync::Arc::new(std::sync::Mutex::new((sha2::Sha256::new(),0u64)));
                    let progress=state.clone();
                    let measured:AssetStream=Box::pin(stream.map(move|chunk|{
                        if let Ok(bytes)=&chunk{let mut state=progress.lock().expect("illustration meter mutex");
                            let Some(next)=state.1.checked_add(bytes.len() as u64)else{return Err(error("illustration asset length overflow"));};
                            state.0.update(bytes);state.1=next;}
                        chunk
                    }));
                    remote.write_stream_once(&pool,measured,u64::MAX,&claimed_sha).await?;
                    let (hash,size)={let state=state.lock().expect("illustration meter mutex");(hex::encode(state.0.clone().finalize()),state.1)};
                    if !hash.eq_ignore_ascii_case(&claimed_sha){return Err(error("illustration bytes differ from their hash-named key"));}
                    (pool,size,hash,"s3")
                }else{
                    let stage=remote.stage_stream(stream).await?;
                    let outcome=async{
                        let mut digest=[0u8;32];hex::decode_to_slice(&stage.sha256,&mut digest).map_err(|error|NarouError::Platform(error.to_string()))?;
                        let extension=key.as_ref().rsplit_once('.').map(|(_,extension)|extension).ok_or_else(||error("illustration extension missing"))?;
                        let pool=ObjectKey::try_new(format!("illustrations/{}.{}",base64::display::Base64Display::new(&digest,&base64::engine::general_purpose::URL_SAFE_NO_PAD),extension))?;
                        let stream=remote.read_stream(&stage.key).await?.ok_or_else(||error("illustration stage missing"))?;
                        remote.write_stream_once(&pool,stream,stage.size,&stage.sha256).await?;
                        Ok::<_,NarouError>(pool)
                    }.await;
                    let _=remote.delete(&stage.key).await;
                    (outcome?,stage.size,stage.sha256,"s3")
                }
            }else{
                let blob=ObjectKey::try_new(format!("content/{}.bin",nonce()?))?;
                self.db.prepare("INSERT INTO content_garbage(blob_key,not_before) VALUES (?,?)").bind(&[blob.as_ref().into(),(Utc::now()+Duration::hours(24)).to_rfc3339().into()]).map_err(error)?.run().await.map_err(error)?;
                let stage=remote.stage_stream(stream).await?;remote.copy(&stage.key,&blob).await?;remote.delete(&stage.key).await?;(blob,stage.size,stage.sha256,"s3")
            }
        }else{let id=format!("ordinary-{token}");let stage=narou_rs::platform::transfer_stage::stage_asset_stream(self.d1.as_ref(),&id,&id,1,24*60*60,stream).await?;(stage.key,stage.size,stage.sha256,"d1")};
        let prepare=|sql:&str,values:&[JsValue]|self.db.prepare(sql).bind(values).map_err(error);
        let statements=vec![prepare("INSERT INTO transfer_storage_guards(token,prefix,owner,epoch,closed) VALUES (?,?,'',?,0)",&[token.as_str().into(),fence.prefix.as_ref().trim_end_matches('/').into(),JsValue::from_f64(fence.epoch as f64)])?,
            prepare("SELECT CASE WHEN ?='d1' OR ? GLOB 'illustrations/*' OR EXISTS(SELECT 1 FROM content_garbage WHERE blob_key=? AND julianday(not_before)>julianday('now')) THEN 1 ELSE abs(-9223372036854775808) END",&[backend.into(),blob.as_ref().into(),blob.as_ref().into()])?,
            prepare("INSERT INTO transfer_object_refs(object_key,blob_key,size,sha256,backend,updated_at,blob_generation) SELECT ?1,?2,?3,?4,?5,?6,CASE WHEN ?5='d1' THEN (SELECT v.generation FROM transfer_verified_objects v JOIN objects o ON o.object_key=v.blob_key AND o.generation=v.generation WHERE v.blob_key=?2 AND v.size=?3 AND v.sha256=?4) ELSE '' END WHERE ?7 IS NULL OR COALESCE((SELECT blob_key FROM transfer_object_refs WHERE object_key=?1),'')=?7 ON CONFLICT(object_key) DO UPDATE SET blob_key=excluded.blob_key,size=excluded.size,sha256=excluded.sha256,backend=excluded.backend,updated_at=excluded.updated_at,blob_generation=excluded.blob_generation RETURNING object_key",&[key.as_ref().into(),blob.as_ref().into(),JsValue::from_f64(size as f64),sha.as_str().into(),backend.into(),Utc::now().to_rfc3339().into(),expected.map(JsValue::from_str).unwrap_or(JsValue::NULL)])?,
            prepare("DELETE FROM content_garbage WHERE blob_key=? AND EXISTS(SELECT 1 FROM transfer_object_refs WHERE blob_key=?)",&[blob.as_ref().into(),blob.as_ref().into()])?,self.d1.finish_publication_guard(&token)?];
        let results=self.db.batch(statements).await.map_err(|e|error(format!("D1 batch failed (content publish guard): {e}")))?;crate::d1_repository::ensure_batch_success(&results)?;
        #[derive(Deserialize)]struct Committed{object_key:String}
        Ok(results[2].results::<Committed>().map_err(error)?.iter().any(|row|row.object_key==key.as_ref()))
    }
    pub async fn reclaim_legacy_retirements(&self,cursor:Option<&str>,limit:usize)->Result<Option<String>>{
        #[derive(Deserialize)]struct Row{object_key:String,pool_key:String,etag:Option<String>,backend:String}
        let cap=limit.clamp(1,16);let rows=self.db.prepare("SELECT object_key,pool_key,etag,backend FROM legacy_object_retirements WHERE object_key>? ORDER BY object_key LIMIT ?").bind(&[cursor.unwrap_or("").into(),JsValue::from_f64((cap+1) as f64)]).map_err(error)?.all().await.map_err(error)?.results::<Row>().map_err(error)?;
        let next=(rows.len()>cap).then(||rows[cap-1].object_key.clone());
        for row in rows.into_iter().take(cap){if row.backend=="s3"&&let Some(etag)=&row.etag{self.remote.as_ref().ok_or_else(||error("legacy S3 GC storage missing"))?.delete_legacy_if_match(&ObjectKey::try_new(row.object_key.clone())?,&ObjectKey::try_new(row.pool_key)?,etag).await?;}
            let result=self.db.prepare("DELETE FROM legacy_object_retirements WHERE object_key=? AND etag IS ?").bind(&[row.object_key.as_str().into(),row.etag.as_deref().map(JsValue::from_str).unwrap_or(JsValue::NULL)]).map_err(error)?.run().await.map_err(error)?;crate::d1_repository::ensure_batch_success(std::slice::from_ref(&result))?;
        }Ok(next)
    }
}

impl ObjectStore for TransferReferenceStore{
    fn delete_closed<'a>(&'a self,prefix:&'a narou_rs::platform::ObjectPrefix,owner:&'a str,epoch:u64,key:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{self.d1.delete_closed(prefix,owner,epoch,key)}
    fn retire_legacy_guarded<'a>(&'a self,fence:&'a narou_rs::platform::WriteFence,logical:&'a ObjectKey,pool:&'a ObjectKey,size:u64,sha:&'a str)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        if !fence.prefix.matches(logical){return Err(error("legacy retirement outside captured fence"));}
        let(physical,backend,generation,etag)=if let Some(remote)=&self.remote{
            narou_rs::platform::transfer_stage::verify_illustration_pool(remote.as_ref(),logical,pool,size,sha).await?;
            (pool.to_string(),"s3",String::new(),remote.legacy_etag(logical).await?)
        }else{
            narou_rs::platform::transfer_stage::verify_illustration_pool(self.d1.as_ref(),logical,pool,size,sha).await?;
            let reference=self.d1.transfer_reference(pool).await?.filter(|reference|reference.backend=="d1"&&reference.size==size&&reference.sha256.eq_ignore_ascii_case(sha)&&!reference.blob_generation.is_empty()).ok_or_else(||error("D1 pool generation proof missing"))?;
            (reference.blob_key,"d1",reference.blob_generation,None)
        };
        let token=nonce()?;let prepare=|sql:&str,values:&[JsValue]|self.db.prepare(sql).bind(values).map_err(error);
        let statements=vec![self.d1.open_guard(fence,&token)?,
            prepare("INSERT INTO legacy_object_retirements(object_key,legacy_key,pool_key,size,sha256,etag,backend,prefix,epoch,updated_at) VALUES (?1,?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(object_key) DO UPDATE SET legacy_key=excluded.legacy_key,pool_key=excluded.pool_key,size=excluded.size,sha256=excluded.sha256,etag=excluded.etag,backend=excluded.backend,prefix=excluded.prefix,epoch=excluded.epoch,updated_at=excluded.updated_at",&[logical.as_ref().into(),pool.as_ref().into(),JsValue::from_f64(size as f64),sha.into(),etag.as_deref().map(JsValue::from_str).unwrap_or(JsValue::NULL),backend.into(),fence.prefix.as_ref().trim_end_matches('/').into(),JsValue::from_f64(fence.epoch as f64),Utc::now().to_rfc3339().into()])?,
            prepare("DELETE FROM object_chunks WHERE object_key=?",&[logical.as_ref().into()])?,prepare("DELETE FROM objects WHERE object_key=?",&[logical.as_ref().into()])?,
            prepare("INSERT INTO transfer_object_refs(object_key,blob_key,size,sha256,backend,updated_at,blob_generation) VALUES(?,?,?,?,?,?,?) ON CONFLICT(object_key) DO UPDATE SET blob_key=excluded.blob_key,size=excluded.size,sha256=excluded.sha256,backend=excluded.backend,updated_at=excluded.updated_at,blob_generation=excluded.blob_generation",&[logical.as_ref().into(),physical.as_str().into(),JsValue::from_f64(size as f64),sha.into(),backend.into(),Utc::now().to_rfc3339().into(),generation.as_str().into()])?,self.d1.finish_publication_guard(&token)?];
        let results=self.db.batch(statements).await.map_err(error)?;crate::d1_repository::ensure_batch_success(&results)?;
        if let(Some(remote),Some(etag))=(&self.remote,etag){remote.delete_legacy_if_match(logical,pool,&etag).await?;}Ok(())
    })}
    fn update_small_guarded<'a,'data:'a>(&'a self,fence:&'a narou_rs::platform::WriteFence,key:&'a ObjectKey,mutate:&'a mut SmallObjectMutator<'data>)->PlatformFuture<'a,Result<ObjectUpdateResult>>{Box::pin(async move{
        for _ in 0..4{let Some(reference)=self.d1.transfer_reference(key).await?else{return self.ordinary_objects.update_small_guarded(fence,key,mutate).await;};
            let current=ObjectStore::read_small(self,key).await?;let Some(next)=mutate(current.as_deref())?else{return Ok(ObjectUpdateResult::Committed);};if next.len()>CAP{return Err(error("atomic control write exceeds cap"));}let bytes=next.into_owned();
            if self.publish_stream_cas(fence,key,Box::pin(futures::stream::once(async move{Ok(bytes)})),Some(&reference.blob_key)).await?{return Ok(ObjectUpdateResult::Committed);}
        }Ok(ObjectUpdateResult::Conflict)
    })}
    fn access_fence(&self)->Option<&dyn narou_rs::platform::NovelAccessFence>{Some(self.d1.as_ref())}
    fn read_stream_leased<'a>(&'a self,prefix:&'a narou_rs::platform::ObjectPrefix,lease:&'a narou_rs::platform::GateLease,key:&'a ObjectKey,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<AssetStream>>>{Box::pin(async move{
        use narou_rs::platform::NovelAccessFence;
        if !prefix.matches(key){return Err(error("reader outside novel"));}self.d1.renew_reader(prefix,lease,narou_rs::platform::READER_LEASE_SECS).await?;let stream=AssetStore::read_stream_range(self,key,range).await?;self.d1.renew_reader(prefix,lease,narou_rs::platform::READER_LEASE_SECS).await?;Ok(stream)
    })}

    /// Immutable fingerprints validate against the canonical digest rows, not
    /// fresh body downloads. A logical key bound to a `transfer_object_refs`
    /// row compares its stored SHA-256 directly; unmapped keys delegate to the
    /// ordinary tier, which batches `content_objects` digests for content
    /// keys and re-reads only mutable/legacy bytes.
    fn verify_fingerprints<'a, 'input: 'a>(
        &'a self,
        fingerprints: &'input [ObjectFingerprint],
        reader: &'a mut dyn narou_rs::platform::ObjectFingerprintReader,
    ) -> PlatformFuture<'a, Result<Option<ObjectFingerprintMismatch<'input>>>> {
        Box::pin(async move {
            #[derive(Deserialize)]
            struct TransferFingerprintRow { object_key: String, sha256: String, backend: String }
            for batch in fingerprints.chunks(FINGERPRINT_BATCH_SIZE) {
                const PREFIX: &str = "SELECT object_key,sha256,backend FROM transfer_object_refs WHERE object_key IN (";
                const SUFFIX: &str = ") ORDER BY object_key COLLATE BINARY";
                let mut query = String::with_capacity(PREFIX.len() + batch.len() * 2 + SUFFIX.len());
                query.push_str(PREFIX);
                for index in 0..batch.len() {
                    if index != 0 { query.push(','); }
                    query.push('?');
                }
                query.push_str(SUFFIX);
                let values: Vec<JsValue> = batch.iter()
                    .map(|fingerprint| JsValue::from_str(fingerprint.key.as_ref())).collect();
                let rows = self.db.prepare(&query).bind(&values).map_err(error)?
                    .all().await.map_err(error)?.results::<TransferFingerprintRow>().map_err(error)?;
                let mut ordinary_start = 0;
                for (position, fingerprint) in batch.iter().enumerate() {
                    let Ok(index) = rows.binary_search_by(|row| row.object_key.as_str().cmp(fingerprint.key.as_ref())) else {
                        continue;
                    };
                    if ordinary_start < position
                        && let Some(mismatch) = self.ordinary_objects.verify_fingerprints(&batch[ordinary_start..position], reader).await? {
                        return Ok(Some(mismatch));
                    }
                    if rows[index].backend == "deleted" {
                        return Ok(Some(ObjectFingerprintMismatch::Missing(&fingerprint.key)));
                    }
                    let mut expected = [0u8; 64];
                    hex::encode_to_slice(fingerprint.sha256.as_slice(), &mut expected).map_err(error)?;
                    if rows[index].sha256.as_bytes() != expected {
                        return Ok(Some(ObjectFingerprintMismatch::Changed(&fingerprint.key)));
                    }
                    ordinary_start = position + 1;
                }
                if ordinary_start < batch.len()
                    && let Some(mismatch) = self.ordinary_objects.verify_fingerprints(&batch[ordinary_start..], reader).await? {
                    return Ok(Some(mismatch));
                }
            }
            Ok(None)
        })
    }
    fn stat<'a>(&'a self,key:&'a ObjectKey)->PlatformFuture<'a,Result<Option<ObjectMetadata>>>{Box::pin(async move{
        if let Some(row)=self.d1.transfer_reference(key).await?{if row.backend=="deleted"{return Ok(None);}return Ok(Some(ObjectMetadata{key:key.clone(),size:row.size,etag:Some(row.sha256),content_type:content_type_for_key(key).map(str::to_owned),last_modified:chrono::DateTime::parse_from_rfc3339(&row.updated_at).ok().map(|time|time.with_timezone(&Utc))}));}self.ordinary_objects.stat(key).await
    })}
    fn read_small<'a>(&'a self,key:&'a ObjectKey)->PlatformFuture<'a,Result<Option<Vec<u8>>>>{Box::pin(async move{
        let Some(row)=self.d1.transfer_reference(key).await?else{return self.ordinary_objects.read_small(key).await;};if row.backend=="deleted"{return Ok(None);}if row.size>CAP as u64{return Err(error("logical object exceeds small read cap"));}
        let mut stream=self.reference_stream(row,0..u64::MAX).await?.ok_or_else(||error("missing reference"))?;let mut out=Vec::new();while let Some(chunk)=stream.next().await{out.extend_from_slice(&chunk?);}Ok(Some(out))
    })}
    fn write_small<'a>(&'a self,key:&'a ObjectKey,data:Vec<u8>)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        if data.len()>CAP{return Err(error("small write exceeds cap"));}
        if let Some(fence)=self.writer_for_key(key).await?{return self.write_small_guarded(&fence,key,data).await;}self.ordinary_objects.write_small(key,data).await
    })}
    fn write_small_guarded<'a>(&'a self,fence:&'a narou_rs::platform::WriteFence,key:&'a ObjectKey,data:Vec<u8>)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        if data.len()>CAP{return Err(error("small write exceeds cap"));}
        if is_content_key(key)||metadata_in_d1(role_for(fence,key)?){return self.ordinary_objects.write_small_guarded(fence,key,data).await;}
        self.publish_stream(fence,key,Box::pin(futures::stream::once(async move{Ok(data)}))).await
    })}
    fn update_small<'a,'data:'a>(&'a self,key:&'a ObjectKey,mutate:&'a mut SmallObjectMutator<'data>)->PlatformFuture<'a,Result<ObjectUpdateResult>>{Box::pin(async move{
        for _ in 0..4{
            let Some(reference)=self.d1.transfer_reference(key).await?else{return self.ordinary_objects.update_small(key,mutate).await;};
            let fence=self.writer_for_key(key).await?.ok_or_else(||error("reference has no mutation gate"))?;
            let current=ObjectStore::read_small(self,key).await?;let Some(next)=mutate(current.as_deref())?else{return Ok(ObjectUpdateResult::Committed);};
            if next.len()>CAP{return Err(error("atomic control write exceeds cap"));}let bytes=next.into_owned();
            if self.publish_stream_cas(&fence,key,Box::pin(futures::stream::once(async move{Ok(bytes)})),Some(&reference.blob_key)).await?{return Ok(ObjectUpdateResult::Committed);}
        }Ok(ObjectUpdateResult::Conflict)
    })}
    fn delete<'a>(&'a self,key:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{Box::pin(async move{if let Some(fence)=self.writer_for_key(key).await?{return self.delete_guarded(&fence,key).await;}self.ordinary_objects.delete(key).await})}
    fn delete_guarded<'a>(&'a self,fence:&'a narou_rs::platform::WriteFence,key:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{
        if is_content_key(key){self.ordinary_objects.delete_guarded(fence,key)}else{self.d1.delete_guarded(fence,key)}
    }
    fn list_page<'a>(&'a self,request:&'a ObjectListRequest)->PlatformFuture<'a,Result<ObjectListPage>>{Box::pin(async move{
        let cap=request.limit.get();let base=self.ordinary_objects.list_page(request).await?;let lower=request.prefix.as_ref().trim_end_matches('/');let descendants=format!("{lower}/");let upper=prefix_upper_bound(&descendants);
        let rows=self.db.prepare("SELECT object_key,blob_key,size,sha256,backend,updated_at,blob_generation FROM transfer_object_refs WHERE (object_key=?1 OR (object_key>=?2 AND object_key<?3) OR ?1='') AND object_key>?4 ORDER BY object_key LIMIT ?5").bind(&[lower.into(),descendants.as_str().into(),upper.as_str().into(),request.cursor.as_deref().unwrap_or("").into(),JsValue::from_f64((cap+1) as f64)]).map_err(error)?.all().await.map_err(error)?.results::<crate::d1_object_store::TransferReference>().map_err(error)?;
        let refs_more=rows.len()>cap;let cutoff=base.next_cursor.clone().into_iter().chain(if refs_more{Some(rows[cap-1].object_key.clone())}else{None}).min();let mut map=std::collections::BTreeMap::new();for metadata in base.objects{map.insert(metadata.key.to_string(),metadata);}
        for row in rows{let key=ObjectKey::try_new(row.object_key)?;if row.backend=="deleted"{map.remove(key.as_ref());continue;}map.insert(key.to_string(),ObjectMetadata{key,size:row.size,etag:Some(row.sha256),content_type:None,last_modified:chrono::DateTime::parse_from_rfc3339(&row.updated_at).ok().map(|time|time.with_timezone(&Utc))});}
        if let Some(cutoff)=&cutoff{map.retain(|key,_|key<=cutoff);}
        let more=map.len()>cap||cutoff.is_some();let objects=map.into_values().take(cap).collect::<Vec<_>>();let next_cursor=if more{objects.last().map(|row|row.key.to_string()).or(cutoff)}else{None};Ok(ObjectListPage{objects,next_cursor})
    })}
}
impl AssetStore for TransferReferenceStore{
    fn stat<'a>(&'a self,key:&'a ObjectKey)->PlatformFuture<'a,Result<Option<ObjectMetadata>>>{ObjectStore::stat(self,key)}
    fn read_stream<'a>(&'a self,key:&'a ObjectKey)->PlatformFuture<'a,Result<Option<AssetStream>>>{self.read_stream_range(key,0..u64::MAX)}
    fn read_stream_range<'a>(&'a self,key:&'a ObjectKey,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<AssetStream>>>{Box::pin(async move{if let Some(row)=self.d1.transfer_reference(key).await?{self.reference_stream(row,range).await}else{self.ordinary_assets.read_stream_range(key,range).await}})}
    fn read_range<'a>(&'a self,key:&'a ObjectKey,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<Vec<u8>>>>{Box::pin(async move{let length=narou_rs::platform::staging::validate_range(&range)?;let Some(mut stream)=self.read_stream_range(key,range).await?else{return Ok(None);};let mut bytes=Vec::with_capacity(length);while let Some(chunk)=stream.next().await{bytes.extend_from_slice(&chunk?);}if bytes.len()!=length{return Err(error("short reference range"));}Ok(Some(bytes))})}
    fn write_stream<'a>(&'a self,key:&'a ObjectKey,stream:AssetStream)->PlatformFuture<'a,Result<()>>{Box::pin(async move{if let Some(fence)=self.writer_for_key(key).await?{self.write_stream_guarded(&fence,key,stream).await}else{self.ordinary_assets.write_stream(key,stream).await}})}
    fn write_stream_guarded<'a>(&'a self,fence:&'a narou_rs::platform::WriteFence,key:&'a ObjectKey,stream:AssetStream)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        if is_content_key(key){return self.ordinary_assets.write_stream_guarded(fence,key,stream).await;}
        if metadata_in_d1(role_for(fence,key)?){return self.d1.write_stream_guarded(fence,key,stream).await;}
        self.publish_stream(fence,key,stream).await
    })}
    fn write_stream_once<'a>(&'a self,key:&'a ObjectKey,stream:AssetStream,size:u64,sha:&'a str)->PlatformFuture<'a,Result<narou_rs::platform::WriteOnce>>{self.ordinary_assets.write_stream_once(key,stream,size,sha)}
    fn delete<'a>(&'a self,key:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{ObjectStore::delete(self,key)}
    fn copy<'a>(&'a self,source:&'a ObjectKey,target:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{Box::pin(async move{let stream=self.read_stream(source).await?.ok_or_else(||error("copy source missing"))?;self.write_stream(target,stream).await})}
    fn move_or_copy<'a>(&'a self,source:&'a ObjectKey,target:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{Box::pin(async move{if source==target{return Ok(());}self.copy(source,target).await?;AssetStore::delete(self,source).await})}
}

#[path = "content_asset_stream.rs"]
mod asset_stream;
const CAP: usize = 16 * 1024 * 1024;
// D1 allows at most 100 bound parameters per statement.
const FINGERPRINT_BATCH_SIZE: usize = 100;

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

#[derive(Deserialize)]
struct BackendRow { value_json: String }

fn parse_backend(value: String) -> Result<ContentBackend> {
    let value = match value.as_str() {
        "d1" | "migrating" | "s3" => value,
        _ => serde_json::from_str::<String>(&value).map_err(|_| error("invalid content_backend marker"))?,
    };
    match value.as_str() {
        "d1" => Ok(ContentBackend::D1),
        "migrating" => Ok(ContentBackend::Migrating),
        "s3" => Ok(ContentBackend::S3),
        _ => Err(error("content_backend must be d1, migrating, or s3")),
    }
}

/// Distinguish an uninitialized library from an explicit legacy opt-out.
pub async fn stored_backend(db: &DbHandle) -> Result<Option<ContentBackend>> {
    let row = db.prepare("SELECT value_json FROM app_state WHERE scope='inv' AND key='content_backend'")
        .first::<BackendRow>(None).await.map_err(error)?;
    row.map(|row| parse_backend(row.value_json)).transpose()
}

/// Report persisted migration state without initializing or modifying it.
pub async fn backend(db: &DbHandle) -> Result<ContentBackend> {
    Ok(stored_backend(db).await?.unwrap_or(ContentBackend::D1))
}

/// The backend a writer would persist for a library without a marker, without
/// writing the marker. A delivery Worker composes the same store and lets the
/// content reads fall back to the legacy D1 bodies, so an uninitialized
/// library stays readable without mutating the shared source.
pub async fn inferred_backend(db: &DbHandle) -> Result<ContentBackend> {
    Ok(if legacy_content_exists(db).await? {
        ContentBackend::Migrating
    } else {
        ContentBackend::S3
    })
}

/// Whether legacy (pre-migration) episode bodies still exist in D1.
async fn legacy_content_exists(db: &DbHandle) -> Result<bool> {
    let row = db
        .prepare("SELECT 1 AS present FROM legacy_content_objects LIMIT 1")
        .first::<i64>(Some("present"))
        .await
        .map_err(error)?;
    Ok(row.is_some())
}

/// Call only after constructing the S3 store successfully. The legacy-content
/// check and marker transition are one statement, so no old body is stranded.
pub async fn initialize_s3_default(db: &DbHandle, required: bool) -> Result<ContentBackend> {
    let row = db.prepare(sql::INITIALIZE_S3_DEFAULT)
        .bind(&[JsValue::from_f64(if required { 1.0 } else { 0.0 })]).map_err(error)?
        .first::<BackendRow>(None).await.map_err(error)?;
    match row {
        Some(row) => parse_backend(row.value_json),
        None => backend(db).await,
    }
}

pub fn is_content_key(key: &ObjectKey) -> bool {
    let value = key.as_ref();
    if value.starts_with("novels/") && value.ends_with("/novel.txt") { return true; }
    if !value.starts_with("novels/") || !value.ends_with(".yaml") { return false; }
    let mut tail = value.rsplit('/');
    // A subdirectory-layout novel may itself be named 本文. Its root YAML
    // metadata must remain in D1; episode filenames include their index.
    if matches!(tail.next(), Some("toc.yaml" | "converter.yaml" | ".illustration_cache.yaml")) { return false; }
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
struct FingerprintRow { object_key: String, sha256: String }
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
    async fn publish(&self,key:&ObjectKey,data:Vec<u8>,expected:&str,legacy_time:&str,verified:bool)->Result<bool>{self.publish_guarded(key,data,expected,legacy_time,verified,None).await}
    async fn publish_guarded(&self, key: &ObjectKey, data: Vec<u8>, expected: &str, legacy_time: &str, verified: bool,fence:Option<&narou_rs::platform::WriteFence>) -> Result<bool> {
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
        self.commit_reference_guarded(&reference,expected,legacy_time,fence).await
    }

    async fn reserve_revision(&self) -> Result<(ObjectKey, String)> {
        let blob = ObjectKey::try_new(format!("content/{}.bin", nonce()?))?;
        let reserved_until = (Utc::now() + Duration::hours(1)).to_rfc3339();
        self.statement("INSERT INTO content_garbage(blob_key,not_before) VALUES (?,?)", &[blob.as_ref().into(), reserved_until.into()])?
            .run().await.map_err(error)?;
        Ok((blob, Utc::now().to_rfc3339()))
    }

    async fn commit_reference(&self,row:&Reference,expected:&str,legacy_time:&str)->Result<bool>{self.commit_reference_guarded(row,expected,legacy_time,None).await}
    async fn commit_reference_guarded(&self, row: &Reference, expected: &str, legacy_time: &str,fence:Option<&narou_rs::platform::WriteFence>) -> Result<bool> {
        let blob = row.blob_key.as_str();
        let values = [row.object_key.as_str().into(), blob.into(), JsValue::from_f64(row.size as f64),
            JsValue::from_f64(row.stored_size as f64), row.encoding.as_str().into(), JsValue::from_f64(row.crc32 as f64),
            row.sha256.as_str().into(), row.updated_at.as_str().into(),
            row.content_type.as_deref().map(JsValue::from_str).unwrap_or(JsValue::NULL),
            JsValue::from_f64(row.verified as f64), expected.into(), legacy_time.into()];
        let token=nonce()?;let mut statements=Vec::with_capacity(6);
        if let Some(fence)=fence{if !fence.prefix.matches(&ObjectKey::try_new(row.object_key.clone())?){return Err(error("content write outside fence"));}statements.push(self.legacy.open_guard(fence,&token)?);}
        statements.push(self.statement(sql::QUEUE_PREVIOUS,&values)?);statements.push(self.statement(sql::PUBLISH,&values)?);
        if expected=="*"{statements.push(self.statement("DELETE FROM transfer_object_refs WHERE object_key=? AND EXISTS(SELECT 1 FROM content_objects WHERE object_key=? AND blob_key=?)",&[row.object_key.as_str().into(),row.object_key.as_str().into(),blob.into()])?);}
        statements.push(self.statement("DELETE FROM content_garbage WHERE blob_key=? AND EXISTS(SELECT 1 FROM content_objects WHERE blob_key=?)",&[blob.into(),blob.into()])?);
        let receipt=statements.len();statements.push(self.statement("SELECT COUNT(*) AS committed FROM content_objects WHERE object_key=? AND blob_key=?",&[row.object_key.as_str().into(),blob.into()])?);
        if fence.is_some(){statements.push(self.legacy.finish_publication_guard(&token)?);}
        let results=self.db.batch(statements).await.map_err(error)?;
        crate::d1_repository::ensure_batch_success(&results)?;
        let rows = results.get(receipt).ok_or_else(|| error("missing publication result"))?.results::<CommitRow>().map_err(error)?;
        let committed = rows.iter().any(|row| row.committed == 1);
        if !committed {
            // No reference can point at this unique unpublished revision.
            self.statement("INSERT INTO content_garbage(blob_key,not_before) VALUES (?,?) ON CONFLICT(blob_key) DO UPDATE SET not_before=excluded.not_before", &[blob.into(),Utc::now().to_rfc3339().into()])?
                .run().await.map_err(error)?;
        }
        Ok(committed)
    }

    async fn remove_content(&self, key: &ObjectKey, expected: &str) -> Result<bool> {
        self.remove_content_guarded(key,expected,None).await
    }
    async fn remove_content_guarded(&self, key: &ObjectKey, expected: &str, fence: Option<&narou_rs::platform::WriteFence>) -> Result<bool> {
        let now = Utc::now().to_rfc3339();
        let values = [key.as_ref().into(),expected.into(),now.into()];
        let one = [JsValue::from_str(key.as_ref())];
        let token=nonce()?;
        let mut statements=Vec::with_capacity(9);
        if let Some(fence)=fence{
            if !fence.prefix.matches(key){return Err(error("content deletion outside fence"));}
            statements.push(self.legacy.open_guard(fence,&token)?);
        }
        statements.push(self.statement(sql::QUEUE_DELETED, &values)?);
        statements.extend([
            self.statement("INSERT INTO content_deletions(object_key) SELECT ?1 WHERE ?2='*' OR COALESCE((SELECT blob_key FROM content_objects WHERE object_key=?1),'')=?2", &values[..2])?,
            self.statement("DELETE FROM object_chunks WHERE object_key=? AND EXISTS(SELECT 1 FROM content_deletions WHERE object_key=?)", &[one[0].clone(),one[0].clone()])?,
            self.statement("DELETE FROM objects WHERE object_key=? AND EXISTS(SELECT 1 FROM content_deletions WHERE object_key=?)", &[one[0].clone(),one[0].clone()])?,
            self.statement("DELETE FROM transfer_object_refs WHERE object_key=? AND EXISTS(SELECT 1 FROM content_deletions WHERE object_key=?)", &[one[0].clone(),one[0].clone()])?,
            self.statement("DELETE FROM content_objects WHERE object_key=? AND EXISTS(SELECT 1 FROM content_deletions WHERE object_key=?)", &[one[0].clone(),one[0].clone()])?,
        ]);
        let receipt=statements.len();
        statements.push(self.statement("DELETE FROM content_deletions WHERE object_key=? RETURNING object_key", &one)?);
        if fence.is_some(){statements.push(self.legacy.finish_publication_guard(&token)?);}
        let results=self.db.batch(statements).await.map_err(error)?;
        crate::d1_repository::ensure_batch_success(&results)?;
        Ok(results.get(receipt).ok_or_else(|| error("missing deletion result"))?.results::<KeyRow>().map_err(error)?.iter().any(|row| row.object_key == key.as_ref()))
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
    pub async fn copy_legacy_guarded(&self,key:&ObjectKey,fence:&narou_rs::platform::WriteFence,lease:&narou_rs::platform::GateLease)->Result<bool>{
        if let Some(reference)=self.legacy.transfer_reference(key).await?{
            if reference.backend=="s3"||reference.backend=="deleted"{return Ok(false);}
            let base=Arc::new(self.clone());let wrapper=TransferReferenceStore::new(self.db.clone(),base.clone(),base,self.legacy.clone(),Some(self.remote_assets.clone()));
            let stream=wrapper.read_stream_leased(&fence.prefix,lease,key,0..u64::MAX).await?.ok_or_else(||error("migration source disappeared"))?;wrapper.write_stream_guarded(fence,key,stream).await?;return Ok(true);
        }
        if self.reference(key).await?.is_some(){return Ok(false);}
        let row=self.statement("SELECT object_key,size,updated_at,content_type FROM legacy_content_objects WHERE object_key=?",&[key.as_ref().into()])?.first::<MetadataRow>(None).await.map_err(error)?;let Some(row)=row else{return Ok(false);};
        if row.size>CAP as u64{return Err(error("legacy episode exceeds metadata codec cap"));}
        let Some(mut stream)=self.legacy.read_stream_leased(&fence.prefix,lease,key,0..u64::MAX).await?else{return Ok(false);};let mut bytes=Vec::with_capacity(row.size as usize);
        while let Some(chunk)=stream.next().await{let chunk=chunk?;if chunk.len()>CAP-bytes.len(){return Err(error("legacy episode exceeds read cap"));}bytes.extend_from_slice(&chunk);}
        self.publish_guarded(key,bytes,"",&row.updated_at,false,Some(fence)).await
    }
    pub async fn verify_reference_guarded(&self,key:&ObjectKey,fence:&narou_rs::platform::WriteFence,lease:&narou_rs::platform::GateLease)->Result<()>{
        if let Some(reference)=self.legacy.transfer_reference(key).await?{
            if reference.backend=="deleted"{return Ok(());}
            let base=Arc::new(self.clone());let wrapper=TransferReferenceStore::new(self.db.clone(),base.clone(),base,self.legacy.clone(),Some(self.remote_assets.clone()));
            let mut stream=wrapper.read_stream_leased(&fence.prefix,lease,key,0..u64::MAX).await?.ok_or_else(||error("verified reference disappeared"))?;while let Some(chunk)=stream.next().await{chunk?;}return Ok(());
        }
        let Some(reference)=self.reference(key).await?else{return Ok(());};
        use narou_rs::platform::NovelAccessFence;self.legacy.renew_reader(&fence.prefix,lease,narou_rs::platform::READER_LEASE_SECS).await?;self.read_reference(&reference).await?;
        let token=nonce()?;let results=self.db.batch(vec![self.legacy.open_guard(fence,&token)?,self.statement("UPDATE content_objects SET verified=1 WHERE object_key=? AND blob_key=?",&[key.as_ref().into(),reference.blob_key.into()])?,self.legacy.finish_publication_guard(&token)?]).await.map_err(error)?;crate::d1_repository::ensure_batch_success(&results)
    }
    pub async fn purge_legacy_guarded(&self,key:&ObjectKey,fence:&narou_rs::platform::WriteFence)->Result<u64>{
        if !fence.prefix.matches(key){return Err(error("legacy purge outside fence"));}if backend(&self.db).await?!=ContentBackend::S3{return Err(error("legacy purge requires S3 cutover"));}
        let token=nonce()?;let values=[key.as_ref().into(),key.as_ref().into()];
        let results=self.db.batch(vec![self.legacy.open_guard(fence,&token)?,
            self.statement("INSERT INTO content_deletions(object_key) SELECT ?1 WHERE EXISTS(SELECT 1 FROM content_objects WHERE object_key=?1 AND verified=1) OR EXISTS(SELECT 1 FROM transfer_object_refs WHERE object_key=?1 AND backend IN ('s3','deleted'))",&values[..1])?,
            self.statement("DELETE FROM object_chunks WHERE object_key=? AND EXISTS(SELECT 1 FROM content_deletions WHERE object_key=?)",&values)?,
            self.statement("DELETE FROM objects WHERE object_key=? AND EXISTS(SELECT 1 FROM content_deletions WHERE object_key=?) RETURNING object_key",&values)?,
            self.statement("DELETE FROM content_deletions WHERE object_key=?",&values[..1])?,self.legacy.finish_publication_guard(&token)?]).await.map_err(error)?;crate::d1_repository::ensure_batch_success(&results)?;Ok(results[3].results::<KeyRow>().map_err(error)?.len() as u64)
    }

    pub async fn cutover(&self) -> Result<()> {
        if backend(&self.db).await? == ContentBackend::S3 { return Ok(()); }
        let result = self.db.prepare(sql::CUTOVER).all().await.map_err(error)?;
        if result.results::<KeyRow>().map_err(error)?.is_empty() { return Err(error("content cutover requires complete verified references")); }
        Ok(())
    }
    pub async fn collect_garbage(&self, limit: usize) -> Result<u64> {
        #[derive(Deserialize)]
        struct Garbage { blob_key: String }
        let rows = self.statement("SELECT blob_key FROM content_garbage g WHERE julianday(not_before)<=julianday('now') AND NOT EXISTS(SELECT 1 FROM content_objects c WHERE c.blob_key=g.blob_key) AND NOT EXISTS(SELECT 1 FROM transfer_object_refs r WHERE r.blob_key=g.blob_key) ORDER BY blob_key LIMIT ?", &[JsValue::from_f64(limit as f64)])?
            .all().await.map_err(error)?.results::<Garbage>().map_err(error)?;
        let mut removed = 0;
        for row in rows {
            self.remote.delete(&ObjectKey::try_new(row.blob_key.clone())?).await?;
            self.statement("DELETE FROM content_garbage WHERE blob_key=?1 AND NOT EXISTS(SELECT 1 FROM content_objects WHERE blob_key=?1) AND NOT EXISTS(SELECT 1 FROM transfer_object_refs WHERE blob_key=?1)", &[row.blob_key.into()])?
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
    fn update_small_guarded<'a,'data:'a>(&'a self,fence:&'a narou_rs::platform::WriteFence,key:&'a ObjectKey,mutate:&'a mut SmallObjectMutator<'data>)->PlatformFuture<'a,Result<ObjectUpdateResult>>{Box::pin(async move{
        if !is_content_key(key)||backend(&self.db).await?==ContentBackend::D1{return self.legacy.update_small_guarded(fence,key,mutate).await;}
        let reference=self.reference(key).await?;let legacy_time=if reference.is_none(){self.statement("SELECT updated_at FROM legacy_content_objects WHERE object_key=?",&[key.as_ref().into()])?.first::<String>(Some("updated_at")).await.map_err(error)?.unwrap_or_default()}else{String::new()};
        let current=match &reference{Some(row)=>Some(self.read_reference(row).await?),None=>self.read_small(key).await?};let Some(next)=mutate(current.as_deref())?else{return Ok(ObjectUpdateResult::Committed);};
        let committed=self.publish_guarded(key,next.into_owned(),reference.as_ref().map(|row|row.blob_key.as_str()).unwrap_or(""),&legacy_time,true,Some(fence)).await?;Ok(if committed{ObjectUpdateResult::Committed}else{ObjectUpdateResult::Conflict})
    })}
    fn write_small_guarded<'a>(&'a self,fence:&'a narou_rs::platform::WriteFence,key:&'a ObjectKey,data:Vec<u8>)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        if !is_content_key(key)||backend(&self.db).await?==ContentBackend::D1{return self.legacy.write_small_guarded(fence,key,data).await;}
        if !self.publish_guarded(key,data,"*","",true,Some(fence)).await?{return Err(error("content publication conflict"));}Ok(())
    })}
    fn delete_guarded<'a>(&'a self,fence:&'a narou_rs::platform::WriteFence,key:&'a ObjectKey)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        if !is_content_key(key)||backend(&self.db).await?==ContentBackend::D1{return self.legacy.delete_guarded(fence,key).await;}
        self.remove_content_guarded(key,"*",Some(fence)).await?;Ok(())
    })}
    fn access_fence(&self)->Option<&dyn narou_rs::platform::NovelAccessFence>{Some(self.legacy.as_ref())}
    fn read_stream_leased<'a>(&'a self,prefix:&'a narou_rs::platform::ObjectPrefix,lease:&'a narou_rs::platform::GateLease,key:&'a ObjectKey,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<AssetStream>>>{Box::pin(async move{
        use narou_rs::platform::NovelAccessFence;
        if !prefix.matches(key){return Err(error("reader outside novel"));}self.legacy.renew_reader(prefix,lease,narou_rs::platform::READER_LEASE_SECS).await?;let stream=self.read_stream_range(key,range).await?;self.legacy.renew_reader(prefix,lease,narou_rs::platform::READER_LEASE_SECS).await?;Ok(stream)
    })}

    fn stat<'a>(&'a self,key:&'a ObjectKey) -> PlatformFuture<'a,Result<Option<ObjectMetadata>>> {
        Box::pin(async move {
            if asset_stream::is_private_or_generated(key) { return self.remote.stat(key).await; }
            if is_content_key(key) {
                let primary = self;
                if let Some(row)=primary.reference(key).await? {
                    return MetadataRow { object_key:row.object_key,size:row.size,updated_at:row.updated_at,content_type:row.content_type }.metadata().map(Some);
                }
                if backend(&primary.db).await? == ContentBackend::S3 && !key.as_ref().ends_with("/novel.txt") { return Ok(None); }
                return ObjectStore::stat(primary.legacy.as_ref(),key).await;
            }
            ObjectStore::stat(self.legacy.as_ref(),key).await
        })
    }
    fn read_small<'a>(&'a self,key:&'a ObjectKey) -> PlatformFuture<'a,Result<Option<Vec<u8>>>> {
        Box::pin(async move {
            if asset_stream::is_private_or_generated(key) { return self.remote.read_small(key).await; }
            if is_content_key(key) {
                let primary = self;
                if let Some(row)=primary.reference(key).await? { return primary.read_reference(&row).await.map(Some); }
                if backend(&primary.db).await? == ContentBackend::S3 && !key.as_ref().ends_with("/novel.txt") { return Ok(None); }
                return primary.legacy.read_small(key).await;
            }
            self.legacy.read_small(key).await
        })
    }
    fn verify_fingerprints<'a, 'input: 'a>(
        &'a self,
        fingerprints: &'input [ObjectFingerprint],
        reader: &'a mut dyn narou_rs::platform::ObjectFingerprintReader,
    ) -> PlatformFuture<'a, Result<Option<ObjectFingerprintMismatch<'input>>>> {
        Box::pin(async move {
            let mut position = 0;
            while let Some(first) = fingerprints.get(position) {
                if !is_content_key(&first.key) {
                    let store = if asset_stream::is_private_or_generated(&first.key) {
                        self.remote.as_ref()
                    } else {
                        self.legacy.as_ref()
                    };
                    if let Some(mismatch) = store.verify_fingerprints(std::slice::from_ref(first), reader).await? {
                        return Ok(Some(mismatch));
                    }
                    position += 1;
                    continue;
                }
                let count = fingerprints[position..].iter().take(FINGERPRINT_BATCH_SIZE)
                    .take_while(|fingerprint| is_content_key(&fingerprint.key)).count();
                let batch = &fingerprints[position..position + count];
                const PREFIX: &str = "SELECT object_key,sha256 FROM content_objects WHERE object_key IN (";
                const SUFFIX: &str = ") ORDER BY object_key COLLATE BINARY";
                let mut query = String::with_capacity(PREFIX.len() + count * 2 + SUFFIX.len());
                query.push_str(PREFIX);
                for index in 0..count {
                    if index != 0 { query.push(','); }
                    query.push('?');
                }
                query.push_str(SUFFIX);
                let values: Vec<JsValue> = batch.iter()
                    .map(|fingerprint| JsValue::from_str(fingerprint.key.as_ref())).collect();
                // These fresh primary rows name immutable blobs. The initial
                // read has already checked each blob's decoded SHA-256.
                let rows = self.statement(&query, &values)?.all().await.map_err(error)?
                    .results::<FingerprintRow>().map_err(error)?;
                for fingerprint in batch {
                    if let Ok(index) = rows.binary_search_by(|row| {
                        row.object_key.as_str().cmp(fingerprint.key.as_ref())
                    }) {
                        let mut expected = [0u8; 64];
                        hex::encode_to_slice(fingerprint.sha256.as_slice(), &mut expected).map_err(error)?;
                        if rows[index].sha256.as_bytes() != expected {
                            return Ok(Some(ObjectFingerprintMismatch::Changed(&fingerprint.key)));
                        }
                    } else {
                        // Missing refs can still be legacy D1 content during
                        // migration. Keep the existing backend/fallback rules.
                        let Some(bytes) = reader.read_small(&fingerprint.key).await? else {
                            return Ok(Some(ObjectFingerprintMismatch::Missing(&fingerprint.key)));
                        };
                        if <[u8; 32]>::from(Sha256::digest(&bytes)) != fingerprint.sha256 {
                            return Ok(Some(ObjectFingerprintMismatch::Changed(&fingerprint.key)));
                        }
                    }
                }
                position += count;
            }
            Ok(None)
        })
    }
    fn write_small<'a>(&'a self,key:&'a ObjectKey,data:Vec<u8>) -> PlatformFuture<'a,Result<()>> {
        Box::pin(async move {
            if asset_stream::is_private_or_generated(key) { return self.remote.write_small(key, data).await; }
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
            if asset_stream::is_private_or_generated(key) { return self.remote_assets.delete(key).await; }
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
    fn write_stream_guarded<'a>(&'a self,fence:&'a narou_rs::platform::WriteFence,key:&'a ObjectKey,stream:AssetStream)->PlatformFuture<'a,Result<()>>{
        Box::pin(self.write_content_stream_with_fence(key,stream,Some(fence)))
    }
    fn read_stream_range<'a>(&'a self,key:&'a ObjectKey,range:std::ops::Range<u64>)->PlatformFuture<'a,Result<Option<AssetStream>>>{Box::pin(self.read_content_stream_range(key,range))}

    fn stat<'a>(&'a self,key:&'a ObjectKey) -> PlatformFuture<'a,Result<Option<ObjectMetadata>>> { ObjectStore::stat(self,key) }
    fn read_stream<'a>(&'a self,key:&'a ObjectKey) -> PlatformFuture<'a,Result<Option<AssetStream>>> {
        Box::pin(self.read_content_stream(key))
    }
    fn read_range<'a>(&'a self,key:&'a ObjectKey,range:std::ops::Range<u64>) -> PlatformFuture<'a,Result<Option<Vec<u8>>>> {
        Box::pin(self.read_content_range(key,range))
    }
    fn stage_stream<'a>(&'a self,stream:AssetStream) -> PlatformFuture<'a,Result<narou_rs::platform::StagedAsset>> {
        self.remote_assets.stage_stream(stream)
    }
    fn write_stream<'a>(&'a self,key:&'a ObjectKey,stream:AssetStream) -> PlatformFuture<'a,Result<()>> {
        Box::pin(self.write_content_stream(key,stream))
    }
    fn delete<'a>(&'a self,key:&'a ObjectKey) -> PlatformFuture<'a,Result<()>> { ObjectStore::delete(self,key) }
    fn copy<'a>(&'a self,source:&'a ObjectKey,destination:&'a ObjectKey) -> PlatformFuture<'a,Result<()>> { Box::pin(self.copy_content_asset(source,destination,false)) }
    fn move_or_copy<'a>(&'a self,source:&'a ObjectKey,destination:&'a ObjectKey) -> PlatformFuture<'a,Result<()>> { Box::pin(self.copy_content_asset(source,destination,true)) }
}

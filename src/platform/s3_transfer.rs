//! S3 transfer primitives: true streamed range reads, conditional write-once
//! writes, and durable resumable staged uploads on multipart.
//!
//! Staged payload = the multipart target object under `.narou-transfer/data/`.
//! Ledger = a small conditional JSON object under `.narou-transfer/.ledger/`.
//! Part ETags persist per-part under `.narou-transfer/.parts/` so a completed
//! `UploadPart` is never lost to an isolate/process death; `Complete` rebuilds
//! the manifest from the durable part log.
//!
//! Staged upload completion stays in the private namespace. Immutable targets
//! are created only with destination preconditions; a late completion cannot
//! replace existing data, and publication still requires its original epoch.

use std::ops::Range;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::error::{NarouError, Result};
use crate::platform::http::HttpMethod;
use crate::platform::object_store::{
    AssetStream, ObjectKey, ObjectPrefix, ObjectListRequest,
};
use crate::platform::s3_store::S3Store;
use crate::platform::staging;
use crate::platform::transfer_stage::{
    self, SealedAsset, StageCursor, StageLease, StageLeaseState, StageLedgerRecord,
    WriteOnce,
};
use crate::platform::PlatformFuture;

const LEDGER_LIMIT: usize = 64 * 1024;
const PART_RECORD_LIMIT: usize = 16 * 1024;
const LEDGER_CAS_ATTEMPTS: usize = 8;
const XML_LIMIT: usize = 1024 * 1024;

fn canonical_owned_etag(mut value: String) -> String {
    value.truncate(value.trim_end_matches('"').len());
    let leading = value.len() - value.trim_start_matches('"').len();
    if leading != 0 { drop(value.drain(..leading)); }
    value
}

#[derive(Serialize, Deserialize)]
struct PartRecord {
    part: u32,
    etag: String,
    size: u64,
}

fn now() -> i64 {
    transfer_stage::now_secs()
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

impl S3Store {
    // -- ledger ------------------------------------------------------------

    async fn transfer_ledger_load(
        &self,
        stage_id: &str,
    ) -> Result<Option<(StageLedgerRecord, String)>> {
        let key = transfer_stage::stage_ledger_key(stage_id)?;
        let (response, _) = self.send(HttpMethod::Get, &self.location().object_url(&key),
            &[], None, Some(LEDGER_LIMIT)).await?;
        if response.status == 404 { return Ok(None); }
        super::streaming::require_status(&response, &[200], "read stage ledger")?;
        let etag = response.header("etag").filter(|value| !value.is_empty())
            .ok_or_else(|| NarouError::Platform("S3 ledger omitted ETag".into()))?.to_owned();
        let record: StageLedgerRecord = serde_json::from_slice(&response.body)
            .map_err(|error| NarouError::Platform(format!("invalid stage ledger: {error}")))?;
        Ok(Some((record, etag)))
    }

    async fn transfer_ledger_put(
        &self,
        stage_id: &str,
        record: &StageLedgerRecord,
        expected_etag: Option<Option<&str>>,
    ) -> Result<bool> {
        let key = transfer_stage::stage_ledger_key(stage_id)?;
        let bytes = serde_json::to_vec(record)
            .map_err(|error| NarouError::Platform(error.to_string()))?;
        if bytes.len() > LEDGER_LIMIT {
            return Err(NarouError::Platform("S3 stage ledger exceeds bound".into()));
        }
        let condition = match expected_etag {
            Some(Some(etag)) => ("if-match", etag.to_string()),
            Some(None) => ("if-match", "*".to_string()),
            None => ("if-none-match", "*".to_string()),
        };
        let (response, _) = self
            .send(
                HttpMethod::Put,
                &self.location().object_url(&key),
                &[("content-type", "application/json"), (condition.0, &condition.1)],
                Some(bytes),
                Some(XML_LIMIT),
            )
            .await?;
        if matches!(response.status, 409 | 412) {
            return Ok(false);
        }
        super::streaming::require_status(&response, &[200, 201, 204], "write stage ledger")?;
        Ok(true)
    }

    /// CAS-mutate the stage ledger against S3 ETags. `mutate` may return
    /// `Ok(None)` to abort with a caller-supplied error already raised, or
    /// `Ok(Some(record))` to attempt the write. Returns the committed record.
    async fn transfer_ledger_mutate<F>(
        &self,
        stage_id: &str,
        mut mutate: F,
    ) -> Result<StageLedgerRecord>
    where
        F: FnMut(Option<StageLedgerRecord>) -> Result<StageLedgerRecord>,
    {
        // First attempt may create (If-None-Match:*).
        for attempt in 0..LEDGER_CAS_ATTEMPTS {
            let loaded = self.transfer_ledger_load(stage_id).await?;
            let next = mutate(loaded.as_ref().map(|(record, _)| record.clone()))?;
            let expected = loaded.as_ref().map(|(_, etag)| Some(etag.as_str()));
            if self
                .transfer_ledger_put(stage_id, &next, expected)
                .await?
            {
                return Ok(next);
            }
            let _ = attempt;
        }
        Err(NarouError::Platform(
            "stage ledger CAS exhausted retries".into(),
        ))
    }

    // -- parts log ---------------------------------------------------------

    async fn put_part_record(&self, stage_id: &str, part: u32, etag: &str, size: u64) -> Result<()> {
        let key = transfer_stage::parts_key(stage_id, part)?;
        let record = PartRecord {
            part,
            etag: etag.to_string(),
            size,
        };
        let bytes = serde_json::to_vec(&record).map_err(|error| NarouError::Platform(error.to_string()))?;
        if bytes.len() > PART_RECORD_LIMIT {
            return Err(NarouError::Platform("S3 part record exceeds bound".into()));
        }
        let (response, _) = self
            .send(
                HttpMethod::Put,
                &self.location().object_url(&key),
                &[("content-type", "application/json")],
                Some(bytes),
                Some(XML_LIMIT),
            )
            .await?;
        super::streaming::require_status(&response, &[200, 201, 204], "write part record")
    }

    async fn delete_transfer_page(&self,prefix:ObjectPrefix)->Result<bool>{
        let page=self.list_objects(&ObjectListRequest::new(prefix,std::num::NonZeroUsize::new(24).expect("nonzero"))).await?;
        for row in page.objects{self.delete_object_raw(&row.key).await?;}Ok(page.next_cursor.is_none())
    }
    async fn delete_part_records(&self,id:&str)->Result<bool>{self.delete_transfer_page(ObjectPrefix::new(format!("{}{id}/",transfer_stage::PARTS_PREFIX))?).await}

    async fn completion_manifest(&self,key:&ObjectKey,upload:&str,next_part:u32)->Result<String>{
        if !(2..=10_001).contains(&next_part){return Err(NarouError::Platform("invalid multipart checkpoint".into()));}
        let mut completion=String::with_capacity((next_part as usize).saturating_mul(100).min(2*1024*1024));completion.push_str("<CompleteMultipartUpload>");let mut marker=0u32;let mut accepted=0u32;
        loop{let url=format!("{}?uploadId={}&max-parts=1000&part-number-marker={marker}",self.location().object_url(key),encode_query(upload));let(response,_)=self.send(HttpMethod::Get,&url,&[],None,Some(XML_LIMIT)).await?;super::streaming::require_status(&response,&[200],"list multipart parts")?;
            let xml=std::str::from_utf8(&response.body).map_err(|_|NarouError::Platform("invalid part listing".into()))?;let xml=super::super::s3_request::xml_result_body(xml,"ListPartsResult").ok_or_else(||NarouError::Platform("invalid part listing document".into()))?;let mut cursor=0;let mut last=marker;
            while let Some(start)=super::super::s3_request::find_tag(&xml[cursor..],"Part"){
                let start=cursor+start;let end=start+super::super::s3_request::find_close(&xml[start..],"Part").ok_or_else(||NarouError::Platform("truncated part listing".into()))?;let block=&xml[start..end];let number=super::streaming::xml_text(block.as_bytes(),"PartNumber")?.parse::<u32>().map_err(|_|NarouError::Platform("invalid part number".into()))?;
                if number<=last||number>10_000{return Err(NarouError::Platform("invalid part sequence".into()));}last=number;
                if number<next_part{if number!=accepted+1{return Err(NarouError::Platform("multipart confirmed prefix missing".into()));}let etag=super::streaming::xml_escape(&super::streaming::xml_text(block.as_bytes(),"ETag")?);if completion.len().saturating_add(etag.len()+96)>2*1024*1024{return Err(NarouError::Platform("multipart completion metadata admission exceeded".into()));}use std::fmt::Write;write!(&mut completion,"<Part><PartNumber>{number}</PartNumber><ETag>{etag}</ETag></Part>").expect("String write");accepted=number;}
                cursor=end+xml[end..].find('>').ok_or_else(||NarouError::Platform("truncated part listing".into()))?+1;
            }
            if accepted==next_part-1||super::streaming::xml_text(&response.body,"IsTruncated")?!="true"{break;}
            if last<=marker{return Err(NarouError::Platform("part listing made no progress".into()));}marker=last;
        }
        if accepted!=next_part-1{return Err(NarouError::Platform("multipart confirmed prefix missing".into()));}completion.push_str("</CompleteMultipartUpload>");Ok(completion)
    }

    // -- multipart plumbing (resumable variant) -----------------------------

    async fn initiate_upload(&self, key: &ObjectKey) -> Result<String> {
        let url = self.location().object_url(key);
        let content_type = crate::platform::object_store::content_type_for_key(key)
            .unwrap_or("application/octet-stream");
        let (response, _) = self
            .send(
                HttpMethod::Post,
                &format!("{url}?uploads="),
                &[("content-type", content_type)],
                None,
                Some(XML_LIMIT),
            )
            .await?;
        super::streaming::require_status(&response, &[200], "initiate multipart")?;
        if !std::str::from_utf8(&response.body).is_ok_and(|xml| {
            super::super::s3_request::xml_result_body(xml, "InitiateMultipartUploadResult").is_some()
        }) {
            return Err(NarouError::Platform(
                "invalid S3 multipart initiation document".into(),
            ));
        }
        super::streaming::xml_text(&response.body, "UploadId")
    }

    async fn stage_upload_part(
        &self,
        key: &ObjectKey,
        upload_id: &str,
        number: u32,
        part: Vec<u8>,
    ) -> Result<String> {
        let url = self.location().object_url(key);
        self.upload_part(&url, upload_id, number as usize, part).await
    }

    async fn complete_stage_upload(
        &self,
        key: &ObjectKey,
        upload_id: &str,
        completion:String,
        expected_size: u64,
        conditional:bool,
    ) -> Result<String> {
        let url = self.location().object_url(key);
        let mut headers=vec![("content-type","application/xml")];if conditional{headers.push(("if-none-match","*"));}
        let (response, _) = self
            .send(
                HttpMethod::Post,
                &format!("{url}?uploadId={}", encode_query(upload_id)),
                &headers,
                Some(completion.into_bytes()),
                Some(XML_LIMIT),
            )
            .await?;
        super::streaming::require_status(&response, &[200], "complete multipart")?;
        if !std::str::from_utf8(&response.body).is_ok_and(|xml| {
            super::super::s3_request::xml_result_body(xml, "CompleteMultipartUploadResult").is_some()
        }) {
            return Err(NarouError::Platform(
                "invalid S3 multipart completion document".into(),
            ));
        }
        let etag=response.header("etag").map(|value|value.trim_matches('"').to_owned())
            .or_else(||super::streaming::xml_text(&response.body,"ETag").ok().map(canonical_owned_etag)).filter(|value|!value.is_empty())
            .ok_or_else(||NarouError::Platform("multipart completion omitted ETag proof".into()))?;
        // ObjectMetadata.etag is the unquoted opaque tag; the completion proof
        // arrives in wire form, so compare canonical values.
        let metadata=self.stat_object(key).await?.ok_or_else(||NarouError::Platform("completed S3 object missing".into()))?;
        if metadata.size!=expected_size||metadata.etag.as_deref()!=Some(etag.as_str()){return Err(NarouError::Platform("completed S3 object generation/length mismatch".into()));}
        Ok(etag)
    }

    /// Sweep any multipart uploads for this key not recorded in the ledger
    /// (lost-initiation orphans) then delete data/parts/ledger objects.
    async fn purge_stage(&self,stage_id:&str,record:Option<&StageLedgerRecord>)->Result<()>{
        let data_key=transfer_stage::stage_data_key(stage_id)?;let mut uploads_done=true;
        if let Some(record)=record{let payload=record.payload_key()?;if let Some(upload)=record.upload_id.as_deref(){self.abort_upload(&payload,upload).await?;}
            // Cross-backend canonical pins live in the primary database. The
            // S3 ledger collector may abort its own upload, never delete the
            // immutable physical target. Primary-aware GC owns that decision.
            if record.direct_target.is_some()&&!payload.as_ref().starts_with("illustrations/"){uploads_done&=self.abort_untracked_uploads(&payload).await?;}
            if let Some(target)=record.publication_target.as_deref(){let target=ObjectKey::try_new(target)?;if let Some(upload)=record.publication_upload_id.as_deref(){self.abort_upload(&target,upload).await?;}
                if !target.as_ref().starts_with("illustrations/"){uploads_done&=self.abort_untracked_uploads(&target).await?;}}
        }
        uploads_done&=self.abort_untracked_uploads(&data_key).await?;let parts_done=self.delete_part_records(stage_id).await?;let checkpoints_done=self.delete_transfer_page(ObjectPrefix::new(format!(".narou-transfer/.checkpoints/{stage_id}/"))?).await?;self.delete_object_raw(&data_key).await?;
        if uploads_done&&parts_done&&checkpoints_done{self.delete_object_raw(&transfer_stage::stage_ledger_key(stage_id)?).await?;}Ok(())
    }
    /// One bounded orphan page. The retained ledger is the retry cursor;
    /// shared pool targets are never swept because another owner may upload.
    async fn abort_untracked_uploads(&self,key:&ObjectKey)->Result<bool>{
        let storage_key=self.location().storage_key(key);let object_url=self.location().object_url(key);let suffix=super::super::s3_sigv4::canonical_path(&storage_key);let bucket_url=object_url.strip_suffix(suffix.trim_start_matches('/')).ok_or_else(||NarouError::Platform("invalid multipart bucket URL".into()))?;
        let url=format!("{}?uploads=&prefix={}&max-uploads=16",bucket_url.trim_end_matches('/'),encode_query(&storage_key));let(response,_)=self.send(HttpMethod::Get,&url,&[],None,Some(XML_LIMIT)).await?;super::streaming::require_status(&response,&[200],"list multipart uploads")?;
        let xml=std::str::from_utf8(&response.body).map_err(|_|NarouError::Platform("invalid multipart listing".into()))?;let xml=super::super::s3_request::xml_result_body(xml,"ListMultipartUploadsResult").ok_or_else(||NarouError::Platform("invalid multipart listing document".into()))?;let mut cursor=0;let mut count=0;
        while let Some(start)=super::super::s3_request::find_tag(&xml[cursor..],"Upload"){
            count+=1;if count>16{return Err(NarouError::Platform("multipart listing exceeded requested page".into()));}let start=cursor+start;let end=start+super::super::s3_request::find_close(&xml[start..],"Upload").ok_or_else(||NarouError::Platform("truncated multipart listing".into()))?;let block=&xml[start..end];
            let listed=super::streaming::xml_text(block.as_bytes(),"Key")?;if listed==storage_key{let upload=super::streaming::xml_text(block.as_bytes(),"UploadId")?;self.abort_upload(key,&upload).await?;}else if listed.starts_with(&storage_key){return Ok(true);}else{return Err(NarouError::Platform("multipart prefix contains unexpected key".into()));}cursor=end+xml[end..].find('>').ok_or_else(||NarouError::Platform("truncated multipart listing".into()))?+1;
        }Ok(super::streaming::xml_text(&response.body,"IsTruncated")?!="true")
    }
    pub async fn stage_open_target(&self,id:&str,owner:&str,epoch:u64,ttl:i64,target:&ObjectKey)->Result<StageCursor>{
        if !target.as_ref().starts_with("illustrations/")&&!target.as_ref().starts_with("content/"){return Err(NarouError::Platform("direct stage target must be immutable physical content".into()));}
        let record=self.transfer_ledger_mutate(id,|current|{let mut record=match current{Some(record)=>record,None=>StageLedgerRecord::new(id,owner,epoch,ttl)?};transfer_stage::require_owner(&record,owner,epoch)?;
            if record.direct_target.as_deref()!=Some(target.as_ref())&&(record.direct_target.is_some()||record.confirmed_bytes!=0||record.upload_id.is_some()){return Err(NarouError::Platform("direct stage identity conflict".into()));}
            if record.lease.state==StageLeaseState::Aborting{return Err(NarouError::Platform("stage_aborted".into()));}record.direct_target=Some(target.to_string());Ok(record)}).await?;Ok(record.cursor())
    }
    /// Called only with a durable server-side checkpoint from an ETag-bound
    /// complete SHA scan. This is not a client-supplied adoption shortcut.
    pub async fn seal_verified_target(&self,id:&str,owner:&str,epoch:u64,checkpoint:&StageCursor,sha:&str,etag:&str)->Result<SealedAsset>{
        // Generation proofs persist in canonical (unquoted) form; wire quotes
        // from whichever response produced the proof are stripped here.
        let etag=etag.trim_matches('"');
        if etag.is_empty()||!checkpoint.prefix_sha256().is_some_and(|value|value.eq_ignore_ascii_case(sha)){return Err(NarouError::Platform("verified target SHA/ETag proof missing".into()));}
        let record=self.transfer_ledger_mutate(id,|current|{let mut record=current.ok_or_else(||NarouError::Platform("direct stage missing".into()))?;transfer_stage::require_owner(&record,owner,epoch)?;if record.direct_target.is_none()||record.lease.state==StageLeaseState::Aborting{return Err(NarouError::Platform("invalid verified target state".into()));}
            record.confirmed_bytes=checkpoint.confirmed_bytes;record.hash_state_hex=checkpoint.hash_state.as_deref().map(hex::encode);record.hash_algorithm=transfer_stage::HASH_CHECKPOINT_ALG.into();record.crc32=checkpoint.crc32;record.pending_append=None;record.final_received=true;record.sealed_etag=Some(etag.into());record.lease.state=StageLeaseState::Sealed;Ok(record)}).await?;record.verify_seal(owner,epoch,checkpoint.confirmed_bytes,sha)
    }
    pub async fn sealed_stage_etag(&self,id:&str,owner:&str,epoch:u64,size:u64,sha:&str)->Result<String>{let(record,_)=self.transfer_ledger_load(id).await?.ok_or_else(||NarouError::Platform("sealed stage proof missing".into()))?;record.verify_seal(owner,epoch,size,sha)?;if record.lease.state!=StageLeaseState::Sealed{return Err(NarouError::Platform("S3 stage is not sealed".into()));}
        // Ledgers written by earlier builds may hold the wire-quoted form;
        // readers need the canonical opaque tag, so normalize at the boundary.
        record.sealed_etag.map(canonical_owned_etag).filter(|etag|!etag.is_empty()).ok_or_else(||NarouError::Platform("sealed S3 generation proof missing".into()))}
    /// Detach control ownership after promotion without ever deleting the
    /// referenced physical object. Pool targets are always retained.
    pub async fn release_target_stage(&self,id:&str,owner:&str,epoch:u64,retain:bool)->Result<bool>{
        if self.transfer_ledger_load(id).await?.is_none(){return Ok(true);}
        if retain{self.transfer_ledger_mutate(id,|current|{let mut record=current.ok_or_else(||NarouError::Platform("direct stage missing".into()))?;transfer_stage::require_owner(&record,owner,epoch)?;if let Some(target)=record.direct_target.take(){record.publication_target=Some(target);record.publication_upload_id=record.upload_id.take();}Ok(record)}).await?;}
        <Self as transfer_stage::TransferStageStore>::stage_abort(self,id,owner,epoch).await?;Ok(self.transfer_ledger_load(id).await?.is_none())
    }
}

impl S3Store {
    pub(super) async fn conditional_small_write(&self,key:&ObjectKey,data:Vec<u8>,sha:&str)->Result<()>{
        let size=data.len() as u64;
        if !hex::encode(sha2::Sha256::digest(&data)).eq_ignore_ascii_case(sha){return Err(NarouError::Platform("hash-named pool payload SHA mismatch".into()));}
        let(response,_)=self.send(HttpMethod::Put,&self.location().object_url(key),&[("if-none-match","*"),("content-type",crate::platform::content_type_for_key(key).unwrap_or("application/octet-stream"))],Some(data),Some(XML_LIMIT)).await?;
        if matches!(response.status,409|412){return self.verify_write_once_existing(key,size,sha).await.map(|_|());}
        super::streaming::require_status(&response,&[200,201,204],"conditional pool PUT")
    }

    pub(super) async fn conditional_stream_write(&self,key:&ObjectKey,stream:AssetStream,size:u64,sha:&str)->Result<WriteOnce>{self.write_stream_once_verified(key,stream,size,sha).await.map(|(outcome,_)|outcome)}
    pub async fn write_stream_once_verified(&self,key:&ObjectKey,stream:AssetStream,size:u64,sha:&str)->Result<(WriteOnce,String)>{
        use crate::platform::TransferStageStore;
        let hash_named_pool=transfer_stage::hash_named_digest(key).is_some_and(|digest|digest.eq_ignore_ascii_case(sha));
        if !hash_named_pool{transfer_stage::check_hash_named_key(key,sha)?;}
        if !is_sha256_hex(sha){return Err(NarouError::Platform("invalid write-once SHA-256".into()));}
        // A bounded, content-addressed PUT is atomic and leaves no multipart
        // identity to reclaim. Version targets and larger streams keep journals.
        let stream:AssetStream=if hash_named_pool&&(size==u64::MAX||size<=transfer_stage::STAGE_APPEND_MAX as u64){
            let mut input=transfer_stage::bounded_chunks(stream);
            let mut buffer=Vec::new();
            loop{
                let Some(chunk)=input.next().await else{
                    let measured=buffer.len() as u64;
                    if (size!=u64::MAX&&size!=measured)||!hex::encode(sha2::Sha256::digest(&buffer)).eq_ignore_ascii_case(sha){
                        return Err(NarouError::Platform("write-once size/SHA mismatch".into()));
                    }
                    let(response,_)=self.send(HttpMethod::Put,&self.location().object_url(key),&[("if-none-match","*"),("content-type",crate::platform::content_type_for_key(key).unwrap_or("application/octet-stream"))],Some(buffer),Some(XML_LIMIT)).await?;
                    if matches!(response.status,409|412){return self.verify_immutable_object(key,measured,sha).await.map(|etag|(WriteOnce::Reused,etag));}
                    super::streaming::require_status(&response,&[200,201,204],"conditional PUT")?;
                    let etag=response.header("etag").map(|value|value.trim_matches('"').to_owned()).filter(|etag|!etag.is_empty())
                        .ok_or_else(||NarouError::Platform("conditional PUT omitted ETag proof".into()))?;
                    return Ok((WriteOnce::Written,etag));
                };
                let bytes=chunk?;
                if bytes.len()>transfer_stage::STAGE_APPEND_MAX-buffer.len(){
                    let prefix=futures::stream::iter([Ok(buffer),Ok(bytes)]);
                    break Box::pin(prefix.chain(input)) as AssetStream;
                }
                if buffer.is_empty(){buffer=bytes;}else{buffer.extend_from_slice(&bytes);}
            }
        }else{stream};
        let random=staging::new_staging_key()?;let id=random.as_ref().strip_prefix(staging::STAGING_PREFIX).expect("stage prefix");let owner=format!("conditional/{id}");
        let stage=transfer_stage::stage_asset_stream(self,id,&owner,1,24*60*60,stream).await?;
        let size=if size==u64::MAX{stage.size}else{size};
        if stage.size!=size||!stage.sha256.eq_ignore_ascii_case(sha){let _=self.stage_abort(id,&owner,1).await;return Err(NarouError::Platform("write-once size/SHA mismatch".into()));}
        // The conditional destination write arbitrates reuse. Its durable
        // upload journal belongs to this invocation, not to a shared target.
        let lock_id=format!("conditional-{id}");
        self.stage_open(&lock_id,&owner,1,24*60*60).await?;
        self.transfer_ledger_mutate(&lock_id,|current|{let mut record=current.ok_or_else(||NarouError::Platform("conditional reservation missing".into()))?;transfer_stage::require_owner(&record,&owner,1)?;record.publication_target=Some(key.to_string());Ok(record)}).await?;
        let result=async {
            if size<=transfer_stage::STAGE_APPEND_MAX as u64{
                let bytes=self.read_object(&stage.key,Some(transfer_stage::STAGE_APPEND_MAX)).await?.ok_or_else(||NarouError::Platform("conditional source missing".into()))?;
                let(response,_)=self.send(HttpMethod::Put,&self.location().object_url(key),&[("if-none-match","*"),("content-type",crate::platform::content_type_for_key(key).unwrap_or("application/octet-stream"))],Some(bytes),Some(XML_LIMIT)).await?;
                if matches!(response.status,409|412){return self.verify_immutable_object(key,size,sha).await.map(|etag|(WriteOnce::Reused,etag));}
                super::streaming::require_status(&response,&[200,201,204],"conditional PUT")?;let etag=response.header("etag").map(|value|value.trim_matches('"').to_owned()).filter(|etag|!etag.is_empty()).ok_or_else(||NarouError::Platform("conditional PUT omitted ETag proof".into()))?;return Ok((WriteOnce::Written,etag));
            }
            let upload=self.initiate_upload(key).await?;
            self.transfer_ledger_mutate(&lock_id,|current|{let mut record=current.ok_or_else(||NarouError::Platform("conditional reservation missing".into()))?;transfer_stage::require_owner(&record,&owner,1)?;record.publication_upload_id=Some(upload.clone());Ok(record)}).await?;
            // Every copied part is pinned to the sealed staged generation, so
            // a torn source can never publish bytes we did not verify.
            let source_etag=self.sealed_stage_etag(id,&owner,1,size,sha).await?;
            let source_match=format!("\"{source_etag}\"");
            let copy_source=self.location().copy_source(&stage.key);let mut parts=Vec::new();let mut offset=0u64;
            while offset<size{
                if parts.len()==10_000{return Err(NarouError::Platform("conditional multipart part limit".into()));}
                let end=(offset+transfer_stage::STAGE_APPEND_MAX as u64).min(size);let range=format!("bytes={offset}-{}",end-1);let number=parts.len()+1;
                let url=format!("{}?uploadId={}&partNumber={number}",self.location().object_url(key),encode_query(&upload));
                let(response,_)=self.send(HttpMethod::Put,&url,&[("x-amz-copy-source",&copy_source),("x-amz-copy-source-range",&range),("x-amz-copy-source-if-match",source_match.as_str())],None,Some(XML_LIMIT)).await?;
                super::streaming::require_status(&response,&[200],"conditional copy part")?;parts.push((number as u32,super::streaming::xml_text(&response.body,"ETag")?));offset=end;
            }
            let mut xml=String::from("<CompleteMultipartUpload>");
            for(number,etag)in &parts{use std::fmt::Write;write!(xml,"<Part><PartNumber>{number}</PartNumber><ETag>{}</ETag></Part>",super::streaming::xml_escape(etag)).expect("String write");}xml.push_str("</CompleteMultipartUpload>");
            let(response,_)=self.send(HttpMethod::Post,&format!("{}?uploadId={}",self.location().object_url(key),encode_query(&upload)),&[("content-type","application/xml"),("if-none-match","*")],Some(xml.into_bytes()),Some(XML_LIMIT)).await?;
            if matches!(response.status,409|412){return self.verify_immutable_object(key,size,sha).await.map(|etag|(WriteOnce::Reused,etag));}
            super::streaming::require_status(&response,&[200],"conditional multipart complete")?;
            if !std::str::from_utf8(&response.body).is_ok_and(|xml|super::super::s3_request::xml_result_body(xml,"CompleteMultipartUploadResult").is_some()){return Err(NarouError::Platform("invalid conditional completion".into()));}
            let etag=response.header("etag").map(|value|value.trim_matches('"').to_owned())
                .or_else(||super::streaming::xml_text(&response.body,"ETag").ok().map(canonical_owned_etag)).filter(|etag|!etag.is_empty())
                .ok_or_else(||NarouError::Platform("conditional completion omitted ETag proof".into()))?;Ok((WriteOnce::Written,etag))
        }.await;
        // On failure the owner ledger remains for resumable GC, including the
        // destination multipart identity. Pool objects are never rolled back.
        if result.is_ok(){self.stage_abort(&lock_id,&owner,1).await?;self.stage_abort(id,&owner,1).await?;}result
    }
    async fn verify_write_once_existing(&self,key:&ObjectKey,size:u64,sha:&str)->Result<WriteOnce>{self.verify_immutable_object(key,size,sha).await.map(|_|WriteOnce::Reused)}
    pub async fn verify_immutable_object(&self,key:&ObjectKey,size:u64,sha:&str)->Result<String>{
        let metadata=self.stat_object(key).await?.ok_or_else(||NarouError::Platform("conditional collision object disappeared".into()))?;if metadata.size!=size{return Err(NarouError::Platform("write-once existing size mismatch".into()));}
        let etag=metadata.etag.filter(|etag|!etag.is_empty()).ok_or_else(||NarouError::Platform("existing immutable object omitted ETag".into()))?;
        let mut stream=self.read_stream_range_if_match(key,0..size,&etag).await?.ok_or_else(||NarouError::Platform("conditional collision object disappeared".into()))?;let mut hash=sha2::Sha256::new();let mut seen=0u64;
        while let Some(chunk)=stream.next().await{let bytes=chunk?;seen=seen.checked_add(bytes.len() as u64).filter(|value|*value<=size).ok_or_else(||NarouError::Platform("write-once existing size mismatch".into()))?;hash.update(bytes);}
        if seen!=size||!hex::encode(hash.finalize()).eq_ignore_ascii_case(sha){return Err(NarouError::Platform("write-once existing bytes differ".into()));}Ok(etag)
    }
}

fn encode_query(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

// ---------------------------------------------------------------------------
// AssetStore: true streamed range + write-once
// ---------------------------------------------------------------------------

impl S3Store {
    /// Single `Range` GET whose body is re-chunked to ≤ 64 KiB items. This is
    /// the only correct way to serve the transfer read path on S3 — the old
    /// `read_range` contract fetches at most 64 KiB per call, so an 8 MiB
    /// export batch through it would be 128 GETs.
    pub(super) async fn stream_range(
        &self,
        key: &ObjectKey,
        range: Range<u64>,
    ) -> Result<Option<AssetStream>> {self.stream_range_if_match(key,range,None).await}
    pub async fn read_stream_range_if_match(&self,key:&ObjectKey,range:Range<u64>,etag:&str)->Result<Option<AssetStream>>{self.stream_range_if_match(key,range,Some(etag)).await}
    async fn stream_range_if_match(&self,key:&ObjectKey,range:Range<u64>,etag:Option<&str>)->Result<Option<AssetStream>>{
        // Callers pass the canonical unquoted proof; the header needs the wire
        // form. The proof is an exact opaque tag — quoting is unconditional.
        let etag=etag.map(|value|value.trim_matches('"'));
        if range.start>range.end{return Err(NarouError::Platform("invalid asset range".into()));}
        if range.start == range.end {
            return match self.stat_object(key).await? {
                Some(meta) if range.end>meta.size=>Err(NarouError::Platform("asset range exceeds object".into())),
                Some(meta) if etag.is_some_and(|expected|meta.etag.as_deref()!=Some(expected))=>Err(NarouError::Platform("S3 target generation changed".into())),
                Some(_) => Ok(Some(Box::pin(futures::stream::empty()))),
                None => Ok(None),
            };
        }
        self.renew_stage_progress(key).await?;
        let header = if range.end == u64::MAX {
            format!("bytes={}-", range.start)
        } else {
            format!("bytes={}-{}", range.start, range.end - 1)
        };
        let if_match=etag.map(|value|format!("\"{value}\""));
        let mut headers=vec![("range",header.as_str())];if let Some(value)=if_match.as_deref(){headers.push(("if-match",value));}
        let response=self.stream_get_object(key,&headers).await?;
        if etag.is_some_and(|expected|response.status==206&&response.header("etag").map(|value|value.trim_matches('"'))!=Some(expected)){return Err(NarouError::Platform("S3 target generation changed".into()));}
        match response.status {
            404 => Ok(None),
            206 => {
                let (delivered, total) = super::streaming::content_range(&response)?;
                if range.end==u64::MAX&&delivered.end!=total{return Err(NarouError::Platform("S3 open-ended range was truncated".into()));}
                if delivered.start != range.start {
                    return Err(NarouError::Platform(
                        "S3 range response offset mismatch".into(),
                    ));
                }
                if range.end != u64::MAX && delivered.end != range.end {
                    return Err(NarouError::Platform(
                        "S3 range response length mismatch".into(),
                    ));
                }
                let expected = delivered.end - delivered.start;
                Ok(Some(transfer_stage::bounded_chunks(
                    super::streaming::exact_stream(response.body, Some(expected)),
                )))
            }
            200 => Err(NarouError::Platform("S3 ignored Range; refusing a linear prefix scan".into())),
            // `bytes */N` (N>0) は存在するオブジェクトへの範囲超過。
            // `bytes */0`・Content-Range 不明・先頭からの読み取りは
            // 「読むもの無し」として欠落に畳む。
            416 if range.start==0||super::streaming::range_416_total(&response).is_none_or(|total|total==0)=>Ok(None),
            416 => Err(NarouError::Platform("asset range exceeds object".into())),
            status => Err(NarouError::Platform(format!(
                "S3 range GET failed with status {status}"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// TransferStageStore on multipart
// ---------------------------------------------------------------------------

impl transfer_stage::TransferStageStore for S3Store {
    fn stage_append_min(&self) -> usize {
        transfer_stage::S3_STAGE_PART_MIN
    }

    fn stage_open<'a>(
        &'a self,
        stage_id: &'a str,
        owner: &'a str,
        epoch: u64,
        lease_secs: i64,
    ) -> PlatformFuture<'a, Result<StageCursor>> {
        Box::pin(async move {
            let now = now();
            let owner = owner.to_string();
            let record = self
                .transfer_ledger_mutate(stage_id, |existing| {
                    match existing {
                        Some(record) => {
                            transfer_stage::require_owner(&record, &owner, epoch)?;
                            Ok(record)
                        }
                        None => Ok(StageLedgerRecord {
                            lease: StageLease {
                                stage_id: stage_id.to_string(),
                                owner: owner.clone(),
                                epoch,
                                expires_at: now + lease_secs.max(1),
                                state: StageLeaseState::Writing,
                                created_at: now,
                            },
                            confirmed_bytes: 0,
                            hash_state_hex: Some(hex::encode(
                                transfer_stage::encode_sha_state(&sha2::Sha256::new()),
                            )),
                            hash_algorithm:transfer_stage::HASH_CHECKPOINT_ALG.into(),
                            next_part: 1,
                            upload_id: None,
                            crc32: 0,
                            final_received: false,
                            pending_append: None,
                            publication_target: None,
                            publication_upload_id: None,
                            direct_target:None,
                            sealed_etag:None,
                        }),
                    }
                })
                .await?;
            Ok(record.cursor())
        })
    }

    fn stage_cursor<'a>(
        &'a self,
        stage_id: &'a str,
    ) -> PlatformFuture<'a, Result<Option<StageCursor>>> {
        Box::pin(async move {
            Ok(self
                .transfer_ledger_load(stage_id)
                .await?
                .map(|(record, _)| record.cursor()))
        })
    }

    fn stage_append<'a>(&'a self,stage_id:&'a str,owner:&'a str,epoch:u64,offset:u64,payload:&'a [u8],final_chunk:bool)->PlatformFuture<'a,Result<StageCursor>>{
        Box::pin(async move{
            if payload.len()>transfer_stage::STAGE_APPEND_MAX || (!final_chunk && payload.len()<transfer_stage::S3_STAGE_PART_MIN){return Err(NarouError::Platform("invalid S3 stage part length".into()));}
            let claim=transfer_stage::PendingAppend{offset,size:payload.len() as u64,sha256:hex::encode(sha2::Sha256::digest(payload)),final_chunk};
            let record=self.transfer_ledger_mutate(stage_id,|current|{
                let mut record=current.ok_or_else(||NarouError::Platform("stage not open".into()))?;transfer_stage::require_writable(&record,owner,epoch)?;
                if record.confirmed_bytes!=offset||record.final_received{return Err(NarouError::Platform("stage_offset_conflict".into()));}
                if record.pending_append.as_ref().is_some_and(|pending|pending!=&claim){return Err(NarouError::Platform("stage_chunk_conflict".into()));}
                record.pending_append=Some(claim.clone());Ok(record)
            }).await?;
            let key=record.payload_key()?;
            if record.upload_id.is_none()&&final_chunk&&offset==0{
                if record.direct_target.is_some(){let(response,_)=self.send(HttpMethod::Put,&self.location().object_url(&key),&[("if-none-match","*"),("content-type",crate::platform::content_type_for_key(&key).unwrap_or("application/octet-stream"))],Some(payload.to_vec()),Some(64*1024)).await?;super::streaming::require_status(&response,&[200,201,204],"conditional direct stage PUT")?;}else{self.write_object(&key,payload.to_vec()).await?;}
            }else{
                let record=if record.upload_id.is_none(){
                    let upload=self.initiate_upload(&key).await?;
                    let selected=self.transfer_ledger_mutate(stage_id,|current|{let mut record=current.ok_or_else(||NarouError::Platform("stage disappeared".into()))?;transfer_stage::require_writable(&record,owner,epoch)?;
                        if record.pending_append.as_ref()!=Some(&claim){return Err(NarouError::Platform("stage append claim lost".into()));}
                        if record.upload_id.is_none(){record.upload_id=Some(upload.clone());}Ok(record)}).await?;
                    if selected.upload_id.as_deref()!=Some(upload.as_str()){self.abort_upload(&key,&upload).await?;}selected
                }else{record.clone()};
                if record.next_part>10_000{return Err(NarouError::Platform("S3 multipart part-count limit exceeded".into()));}
                let upload=record.upload_id.as_deref().ok_or_else(||NarouError::Platform("stage upload id missing".into()))?;
                let etag=self.stage_upload_part(&key,upload,record.next_part,payload.to_vec()).await?;
                self.put_part_record(stage_id,record.next_part,&etag,payload.len() as u64).await?;
            }
            let committed=self.transfer_ledger_mutate(stage_id,|current|{let mut record=current.ok_or_else(||NarouError::Platform("stage disappeared".into()))?;
                if record.pending_append.as_ref()!=Some(&claim){return Err(NarouError::Platform("stage append claim lost".into()));}
                record.append(owner,epoch,offset,payload,final_chunk)?;if record.upload_id.is_some(){record.next_part+=1;}Ok(record)
            }).await?;
            self.write_object(&transfer_stage::stage_checkpoint_key(stage_id,committed.confirmed_bytes)?,serde_json::to_vec(&committed.cursor()).map_err(|error|NarouError::Platform(error.to_string()))?).await?;
            Ok(committed.cursor())
        })
    }

    fn stage_rewind<'a>(&'a self,id:&'a str,owner:&'a str,epoch:u64,checkpoint:&'a StageCursor)->PlatformFuture<'a,Result<StageCursor>>{
        Box::pin(async move{
            let trusted=if checkpoint.confirmed_bytes==0{StageLedgerRecord::new(id,owner,epoch,1)?.cursor()}else{
                let bytes=self.read_object(&transfer_stage::stage_checkpoint_key(id,checkpoint.confirmed_bytes)?,Some(64*1024)).await?.ok_or_else(||NarouError::Platform("rewind checkpoint missing".into()))?;
                let stored:StageCursor=serde_json::from_slice(&bytes).map_err(|error|NarouError::Platform(error.to_string()))?;if stored.prefix_sha256()!=checkpoint.prefix_sha256(){return Err(NarouError::Platform("rewind SHA conflict".into()));}stored};
            let checkpoint=&trusted;
            let record=self.transfer_ledger_mutate(id,|current|{let mut record=current.ok_or_else(||NarouError::Platform("stage not open".into()))?;
                if checkpoint.storage_position==0||checkpoint.storage_position>record.next_part{return Err(NarouError::Platform("invalid multipart checkpoint".into()));}
                record.rewind(owner,epoch,checkpoint)?;record.next_part=checkpoint.storage_position;Ok(record)
            }).await?;
            // Uploaded tail parts are deliberately omitted from Complete. They
            // may be overwritten at the same part number during resumed append.
            Ok(record.cursor())
        })
    }

    fn stage_read<'a>(
        &'a self,
        stage_id: &'a str,
        range: Range<u64>,
    ) -> PlatformFuture<'a, Result<Option<AssetStream>>> {
        Box::pin(async move {let Some((record,_))=self.transfer_ledger_load(stage_id).await?else{return Ok(None);};if record.lease.state!=StageLeaseState::Sealed{return Ok(None);}let etag=record.sealed_etag.as_deref().filter(|etag|!etag.is_empty()).ok_or_else(||NarouError::Platform("sealed S3 generation proof missing".into()))?;if range.start>range.end||range.end>record.confirmed_bytes{return Err(NarouError::Platform("sealed S3 range exceeds proof".into()));}self.read_stream_range_if_match(&record.payload_key()?,range,etag).await})
    }

    fn stage_touch<'a>(
        &'a self,
        stage_id: &'a str,
        owner: &'a str,
        epoch: u64,
        lease_secs: i64,
    ) -> PlatformFuture<'a, Result<()>> {
        Box::pin(async move {
            let now = now();
            self.transfer_ledger_mutate(stage_id, |existing| {
                let mut record = existing
                    .ok_or_else(|| NarouError::Platform(format!("stage not open: {stage_id}")))?;
                transfer_stage::require_owner(&record, owner, epoch)?;
                if record.lease.state==StageLeaseState::Aborting{return Err(NarouError::Platform("stage_aborted".into()));}
                record.lease.expires_at = now.saturating_add(lease_secs.max(1));
                Ok(record)
            })
            .await?;
            Ok(())
        })
    }

    fn stage_seal<'a>(&'a self,id:&'a str,owner:&'a str,epoch:u64,size:u64,sha:&'a str)->PlatformFuture<'a,Result<SealedAsset>>{Box::pin(async move{
        let Some((record,_))=self.transfer_ledger_load(id).await?else{return Err(NarouError::Platform("stage not open".into()));};let sealed=record.verify_seal(owner,epoch,size,sha)?;
        if record.lease.state==StageLeaseState::Sealed{if record.sealed_etag.as_deref().is_none_or(str::is_empty){return Err(NarouError::Platform("sealed S3 generation proof missing".into()));}return Ok(sealed);}
        let record=self.transfer_ledger_mutate(id,|current|{let mut current=current.ok_or_else(||NarouError::Platform("stage disappeared".into()))?;
            current.verify_seal(owner,epoch,size,sha)?;current.lease.state=StageLeaseState::Sealing;Ok(current)}).await?;
        let etag=if let Some(upload)=record.upload_id.as_deref(){
            match self.completion_manifest(&sealed.key,upload,record.next_part).await{
                Ok(completion)=>self.complete_stage_upload(&sealed.key,upload,completion,size,record.direct_target.is_some()).await?,
                Err(error)=>{if record.direct_target.is_some(){return Err(error);}let metadata=self.stat_object(&sealed.key).await?.filter(|meta|meta.size==size).ok_or(error)?;metadata.etag.filter(|etag|!etag.is_empty()).ok_or_else(||NarouError::Platform("owned stage completion omitted ETag".into()))?}
            }
        }else{let metadata=self.stat_object(&sealed.key).await?.filter(|meta|meta.size==size).ok_or_else(||NarouError::Platform("stage bytes missing".into()))?;metadata.etag.filter(|etag|!etag.is_empty()).ok_or_else(||NarouError::Platform("owned stage omitted ETag".into()))?};
        self.transfer_ledger_mutate(id,|current|{let mut current=current.ok_or_else(||NarouError::Platform("stage disappeared".into()))?;
            current.verify_seal(owner,epoch,size,sha)?;if current.hash_state_hex!=record.hash_state_hex||current.next_part!=record.next_part{return Err(NarouError::Platform("stage seal checkpoint changed".into()));}
            current.sealed_etag=Some(etag.clone());current.lease.state=StageLeaseState::Sealed;Ok(current)}).await?;Ok(sealed)
    })}

    fn stage_abort<'a>(&'a self,id:&'a str,owner:&'a str,epoch:u64)->PlatformFuture<'a,Result<()>>{Box::pin(async move{
        if self.transfer_ledger_load(id).await?.is_none(){return Ok(());}
        let record=self.transfer_ledger_mutate(id,|current|{let mut record=current.ok_or_else(||NarouError::Platform("stage disappeared".into()))?;transfer_stage::require_owner(&record,owner,epoch)?;record.lease.state=StageLeaseState::Aborting;Ok(record)}).await?;
        self.purge_stage(id,Some(&record)).await
    })}

    fn stage_lease_page<'a>(
        &'a self,
        cursor: Option<&'a str>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<(Vec<StageLease>, Option<String>)>> {
        Box::pin(async move {
            let prefix = ObjectPrefix::new(transfer_stage::LEDGER_PREFIX)?;
            let mut request =
                ObjectListRequest::new(prefix, std::num::NonZeroUsize::new(limit.clamp(1,100)).expect("nonzero"));
            if let Some(cursor) = cursor {
                request = request.after(cursor.to_string());
            }
            let page = self.list_objects(&request).await?;
            let next = page.next_cursor.clone();
            let mut leases = Vec::with_capacity(page.objects.len());
            for object in page.objects {
                let Some(bytes) = self.read_object(&object.key, Some(LEDGER_LIMIT)).await? else {
                    continue;
                };
                match serde_json::from_slice::<StageLedgerRecord>(&bytes) {
                    Ok(record) => leases.push(record.lease()),
                    Err(_) => continue,
                }
            }
            Ok((leases, next))
        })
    }

    fn stage_reclaim<'a>(&'a self,id:&'a str,owner:&'a str,epoch:u64)->PlatformFuture<'a,Result<bool>>{Box::pin(async move{
        let Some((record,etag))=self.transfer_ledger_load(id).await?else{return Ok(true);};
        if record.lease.owner!=owner||record.lease.epoch!=epoch||(record.lease.state!=StageLeaseState::Aborting&&record.lease.expires_at>now()){return Ok(false);}
        let mut claimed=record;claimed.lease.state=StageLeaseState::Aborting;
        if !self.transfer_ledger_put(id,&claimed,Some(Some(&etag))).await?{return Ok(false);}
        self.purge_stage(id,Some(&claimed)).await?;Ok(self.transfer_ledger_load(id).await?.is_none())
    })}
}

//! Primary-only Worker library/staging adapter for the shared transfer engine.
use std::{collections::{BTreeMap,HashSet},num::NonZeroUsize,ops::Range,sync::Arc};
use chrono::Utc;
use futures::StreamExt;
use narou_rs::{application::transfer::*,db::NovelRecord,platform::{AssetStore,AssetStream,NovelAccessFence,NovelId,NovelObjectKeys,NovelRepository,ObjectKey,ObjectListRequest,ObjectPrefix,ObjectStore,PlatformFuture,TransferStageStore},transfer::{ArchiveNovel,VIRTUAL_INPUT_FILES},virtual_novel};
use serde::{Deserialize,Serialize};
use worker::{D1PreparedStatement,wasm_bindgen::JsValue};
use crate::{db_handle::DbHandle,d1_object_store::D1ObjectStore,d1_repository::D1NovelRepository};
use super::{journal::D1TransferJournal,publication,ticket};

pub(super) fn error(e:impl std::fmt::Display)->TransferError{TransferError::Platform{message:e.to_string()}}
pub(super) fn invalid(code:&'static str,message:impl Into<String>)->TransferError{TransferError::InvalidRequest{code,message:message.into()}}
pub(super) fn metadata_error(e:impl std::fmt::Display)->TransferError{let message=e.to_string();if message.contains("transfer_metadata_budget:"){TransferError::ExceedsLimit{code:"metadata_budget",message}}else{invalid("invalid_illustration_index",message)}}
pub(super) fn statement(db:&DbHandle,sql:&str,values:&[JsValue])->Result<D1PreparedStatement,TransferError>{db.prepare(sql).bind(values).map_err(error)}
pub(super) async fn run(db:&DbHandle,sql:&str,values:&[JsValue])->Result<(),TransferError>{let result=statement(db,sql,values)?.run().await.map_err(error)?;if !result.success(){return Err(error(result.error().unwrap_or_default()));}Ok(())}
pub(super) async fn collect(mut stream:AssetStream,expected:u64)->Result<Vec<u8>,TransferError>{
    if expected>MAX_BINARY_BODY_BYTES as u64{return Err(invalid("invalid_range","read exceeds binary budget"));}
    let mut bytes=Vec::with_capacity(expected as usize);while let Some(chunk)=stream.next().await{let chunk=chunk.map_err(error)?;
        if chunk.len()>64*1024||bytes.len() as u64+chunk.len() as u64>expected{return Err(invalid("invalid_range","source returned an oversized range"));}bytes.extend_from_slice(&chunk);}
    if bytes.len() as u64!=expected{return Err(invalid("truncated_source","source range ended early"));}Ok(bytes)
}
pub(super) async fn read_metadata(lease:&mut narou_rs::application::novel_access::NovelReadLease,key:&ObjectKey,size:u64)->Result<Option<Vec<u8>>,TransferError>{
    if size>16*1024*1024{return Err(invalid("metadata_budget","metadata initializer exceeds 16 MiB"));}
    let Some(mut stream)=lease.open_all(key).await.map_err(error)?else{return Ok(None)};
    let mut bytes=Vec::with_capacity(size as usize);
    while let Some(chunk)=stream.next().await{lease.refresh().await.map_err(error)?;let chunk=chunk.map_err(error)?;
        if chunk.len()>64*1024||bytes.len()as u64+chunk.len()as u64>size{return Err(invalid("source_size_mismatch","metadata source changed size or exceeded codec output bound"));}bytes.extend_from_slice(&chunk);
    }
    if bytes.len()as u64!=size{return Err(invalid("source_size_mismatch","metadata source ended before its declared size"));}Ok(Some(bytes))
}
pub(super) fn keys(record:&NovelRecord)->Result<NovelObjectKeys,TransferError>{NovelObjectKeys::new(&record.sitename,&record.file_title,record.use_subdirectory).map_err(error)}
pub(super) fn key_for(record:&NovelRecord,path:&str)->Result<ObjectKey,TransferError>{ObjectKey::try_new(format!("{}/{}",keys(record)?.prefix(),path)).map_err(error)}

#[derive(Deserialize)]
pub(super) struct StageOwner{pub stage_id:String,pub session_id:String,pub expected_size:u64,pub epoch:u64,pub backend:String,pub active:i64,pub sealed_key:Option<String>,pub sha256:Option<String>,pub initialized:i64,pub purpose:String}
#[derive(Default,Serialize,Deserialize)]
struct InventoryCursor{ordinal:u32,phase:u8,after:Option<String>,index_stage:Option<String>,index_offset:u64,#[serde(default)]metadata_emitted:bool}

#[derive(Deserialize)]
struct StagePart{size:u64,sha256:String}
#[derive(Deserialize)]
struct RefRow{blob_key:String,size:u64,sha256:String,backend:String}
#[derive(Deserialize)]
struct PinRow{stage:String,size:u64,sha256:String,backend:String}

pub struct WorkerTransferBackend{
    pub(super) db:DbHandle,pub(super) journal:Arc<D1TransferJournal>,
    pub(super) objects:Arc<dyn ObjectStore>,pub(super) assets:Arc<dyn AssetStore>,
    pub(super) stage:Arc<dyn TransferStageStore>,pub(super) d1:Arc<D1ObjectStore>,
    pub(super) repository:D1NovelRepository,pub(super) s3:bool,pub(super) signing_key:Vec<u8>,
    pub(super) sites:Arc<Vec<narou_rs::downloader::site_setting::SiteSetting>>,
}
impl WorkerTransferBackend{
    pub fn new(db:DbHandle,journal:Arc<D1TransferJournal>,objects:Arc<dyn ObjectStore>,assets:Arc<dyn AssetStore>,stage:Arc<dyn TransferStageStore>,d1:Arc<D1ObjectStore>,s3:bool,signing_key:Vec<u8>,sites:Arc<Vec<narou_rs::downloader::site_setting::SiteSetting>>)->Self{
        let db=db.primary_view();let repository=D1NovelRepository::new(db.clone()).with_sites(sites.clone()).with_objects(objects.clone());Self{db,journal,objects,assets,stage,d1,s3,signing_key,sites,repository}
    }
    pub(super) async fn stage_owner(&self,handle:&str)->Result<StageOwner,TransferError>{
        statement(&self.db,"SELECT stage_id,session_id,expected_size,epoch,backend,active,sealed_key,sha256,initialized,purpose FROM transfer_stage_owners WHERE stage_id=?",&[handle.into()])?.first(None).await.map_err(error)?.ok_or_else(||invalid("stage_expired","stage is not retained"))
    }
    fn stage_store(&self,backend:&str)->Result<&dyn TransferStageStore,TransferError>{match backend{"d1"=>Ok(self.d1.as_ref()),"s3" if self.s3=>Ok(self.stage.as_ref()),_=>Err(TransferError::Unsupported{code:"stage_backend_unavailable",reason:"the retained stage backend is unavailable; no storage fallback is used".into()})}}
    pub(super) async fn stage_stream(&self,handle:&str,range:Range<u64>)->Result<Option<AssetStream>,TransferError>{
        if range.start>range.end{return Err(invalid("invalid_range","unordered range"));}
        let session=if handle.starts_with("pin_"){statement(&self.db,"SELECT session_id FROM transfer_pins WHERE pin=?",&[handle.into()])?.first::<String>(Some("session_id")).await.map_err(error)?.ok_or_else(||invalid("snapshot_missing","pin owner is missing"))?}else{self.stage_owner(handle).await?.session_id};
        let session=SessionId(session);self.journal.ensure_execution(&session).await?;
        let live=statement(&self.db,"SELECT 1 live FROM transfer_sessions s WHERE s.session_id=? AND (s.state NOT IN ('completed','completed_with_errors','released','cancelled','failed','expired') OR EXISTS(SELECT 1 FROM transfer_publications p WHERE p.session_id=s.session_id AND (p.state NOT IN ('pending','committed','rolled_back','skipped') OR (p.state='committed' AND COALESCE(p.phase_cursor,'')<>'finished'))))",&[session.0.as_str().into()])?.first::<i64>(Some("live")).await.map_err(error)?.is_some();
        if !live{return Err(TransferError::SessionExpired{resumable:false});}
        let stream=if handle.starts_with("pin_"){
            let pin=statement(&self.db,"SELECT stage,size,sha256,backend FROM transfer_pins WHERE pin=?",&[handle.into()])?.first::<PinRow>(None).await.map_err(error)?.ok_or_else(||invalid("snapshot_missing","pin is missing"))?;
            if range.end>pin.size{return Err(invalid("invalid_range","pin read exceeds size"));}
            self.stage_store(&pin.backend)?.read_stream_range(&ObjectKey::try_new(pin.stage).map_err(error)?,range).await.map_err(error)?
        }else{let owner=self.stage_owner(handle).await?;if range.end>owner.expected_size{return Err(invalid("invalid_range","stage read exceeds size"));}self.stage_store(&owner.backend)?.stage_read(handle,range).await.map_err(error)?};
        let Some(stream)=stream else{return Ok(None)};let journal=self.journal.clone();
        let stream=futures::stream::try_unfold((stream,journal,session,Utc::now().timestamp()),|(mut stream,journal,session,mut renewed)|async move{
            let part=stream.next().await;
            if Utc::now().timestamp()-renewed>=10{journal.ensure_execution(&session).await.map_err(|e|narou_rs::error::NarouError::Platform(e.to_string()))?;renewed=Utc::now().timestamp();}
            match part{Some(part)=>part.map(|bytes|Some((bytes,(stream,journal,session,renewed)))),None=>Ok(None)}
        });Ok(Some(Box::pin(stream)))
    }
    pub(super) async fn metadata_bytes(&self,handle:&str,size:u64)->Result<Vec<u8>,TransferError>{
        if size>16*1024*1024{return Err(invalid("metadata_budget","metadata initializer exceeds 16 MiB"));}
        let mut stream=self.stage_stream(handle,0..size).await?.ok_or_else(||invalid("stage_missing","metadata stage is missing"))?;
        let mut bytes=Vec::with_capacity(size as usize);
        while let Some(part)=stream.next().await{let part=part.map_err(error)?;if part.len()>64*1024||bytes.len()as u64+part.len()as u64>size{return Err(invalid("source_size_mismatch","metadata stream exceeded its declared size"));}bytes.extend_from_slice(&part);}
        if bytes.len()as u64!=size{return Err(invalid("truncated_source","metadata stream ended early"));}Ok(bytes)
    }
    pub(super) async fn abort_owned_stage(&self,session:&SessionId,handle:&str)->Result<bool,TransferError>{
        let owner=statement(&self.db,"SELECT * FROM transfer_stage_owners WHERE stage_id=?",&[handle.into()])?.first::<StageOwner>(None).await.map_err(error)?;let Some(owner)=owner else{return Ok(true)};
        if owner.session_id!=session.0{return Err(TransferError::SecretMismatch);}
        if let Some(key)=owner.sealed_key.as_deref(){let live=statement(&self.db,"SELECT 1 live WHERE EXISTS(SELECT 1 FROM transfer_object_refs WHERE blob_key=?) OR EXISTS(SELECT 1 FROM transfer_pins WHERE stage=?)",&[key.into(),key.into()])?.first::<i64>(Some("live")).await.map_err(error)?.is_some();if live{return Ok(true);}}
        run(&self.db,"UPDATE transfer_stage_owners SET active=0 WHERE stage_id=? AND session_id=?",&[handle.into(),session.0.as_str().into()]).await?;
        let store=self.stage_store(&owner.backend)?;store.stage_abort(handle,&owner.session_id,owner.epoch).await.map_err(error)?;
        let gone=store.stage_reclaim(handle,&owner.session_id,owner.epoch).await.map_err(error)?;
        if gone{run(&self.db,"DELETE FROM transfer_stage_owners WHERE stage_id=? AND session_id=? AND active=0",&[handle.into(),session.0.as_str().into()]).await?;}Ok(gone)
    }
    async fn live_stage(&self,handle:&str)->Result<StageOwner,TransferError>{
        let owner=self.stage_owner(handle).await?;
        if owner.active==0{return Err(TransferError::ChunkConflict{code:"stage_replaced",message:"stage handle was retired; fetch confirmed file state".into()});}
        let live=statement(&self.db,"SELECT 1 AS live FROM transfer_sessions WHERE session_id=? AND state NOT IN ('completed','completed_with_errors','released','cancelled','failed','expired')",&[owner.session_id.as_str().into()])?.first::<i64>(Some("live")).await.map_err(error)?;
        if live.is_none(){return Err(TransferError::SessionExpired{resumable:false});}Ok(owner)
    }
    pub(super) async fn revision(&self,id:i64)->Result<(String,String),TransferError>{
        let record=self.repository.get(NovelId(id)).await.map_err(error)?.ok_or_else(||invalid("source_missing","novel was deleted"))?;
        let prefix=keys(&record)?.prefix().as_ref().to_string();
        self.repository.bind_record_origin(&record).await.map_err(error)?;drop(record);
        self.bound_revision(id,&prefix).await
    }
    async fn bound_revision(&self,id:i64,prefix:&str)->Result<(String,String),TransferError>{
        #[derive(Deserialize)]struct Revisions{record_revision:u64,content_revision:String}
        let row=statement(&self.db,"SELECT COALESCE(r.record_revision,0) record_revision,CAST(COALESCE(r.content_revision,0) AS TEXT)||':'||CAST(COALESCE(g.content_revision,0) AS TEXT) content_revision FROM novels n JOIN novel_gate_bindings b ON b.novel_id=n.id AND b.prefix=? LEFT JOIN novel_revisions r ON r.novel_id=n.id LEFT JOIN novel_access_gates g ON g.prefix=b.prefix WHERE n.id=?",&[prefix.into(),(id as f64).into()])?.first::<Revisions>(None).await.map_err(error)?.ok_or_else(||invalid("source_missing","novel binding changed or was deleted"))?;
        Ok((row.record_revision.to_string(),row.content_revision))
    }
    pub(super) async fn membership(&self,id:i64)->Result<String,TransferError>{
        let ids=self.member_ids(id).await?;
        let members=ids.iter().map(i64::to_string).collect::<Vec<_>>();
        Ok(narou_rs::platform::membership_digest(members.iter().map(String::as_str)))
    }
    pub(super) async fn portable_url(&self,record:&NovelRecord)->Result<String,TransferError>{
        if virtual_novel::is_portable_toc_url(&record.toc_url){return Ok(record.toc_url.clone());}
        let candidate=virtual_novel::new_toc_url().map_err(error)?;
        let candidate_fold=narou_rs::db::novel_codec::fold(&candidate);let previous_fold=narou_rs::db::novel_codec::fold(&record.toc_url);
        self.repository.bind_record_origin(record).await.map_err(error)?;
        let prefix=keys(record)?.prefix().clone();let fence=self.d1.capture_writer(&ObjectPrefix::from(prefix)).await.map_err(error)?;let token=self.opaque_id("portable_url");
        let results=self.db.batch(vec![self.d1.open_guard(&fence,&token).map_err(error)?,statement(&self.db,"UPDATE novels SET toc_url=?,toc_url_fold=? WHERE id=? AND toc_url=? AND sitename=? AND file_title=? AND use_subdirectory=?",&[candidate.as_str().into(),candidate_fold.as_str().into(),(record.id as f64).into(),record.toc_url.as_str().into(),record.sitename.as_str().into(),record.file_title.as_str().into(),(record.use_subdirectory as i32).into()])?,self.db.prepare("INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE changes()=0"),statement(&self.db,"UPDATE novel_access_gates SET origin_url_fold=?1 WHERE prefix=?2 AND origin_url_fold IN ('',?3)",&[candidate_fold.into(),fence.prefix.as_ref().into(),previous_fold.into()])?,self.db.prepare("INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE changes()=0"),self.d1.finish_publication_guard(&token).map_err(error)?]).await.map_err(error)?;
        crate::d1_repository::ensure_batch_success(&results).map_err(error)?;
        self.repository.get(NovelId(record.id)).await.map_err(error)?.map(|r|r.toc_url).ok_or(TransferError::SessionNotFound)
    }
    pub(super) async fn inventory_metadata(&self,session:&SessionId,id:i64,ordinal:u32)->Result<(ExportNovelEntry,ArchiveNovel),TransferError>{
        let initial=self.repository.get(NovelId(id)).await.map_err(error)?.ok_or_else(||invalid("source_missing",format!("novel {id} is missing")))?;
        if virtual_novel::is_virtual_record(&initial){self.portable_url(&initial).await?;}
        let(record_revision,content_revision)=self.revision(id).await?;
        let record=self.repository.get(NovelId(id)).await.map_err(error)?.ok_or_else(||invalid("source_missing",format!("novel {id} is missing")))?;
        let parent=virtual_novel::is_virtual_record(&record);let toc_url=record.toc_url.clone();
        let latest=self.revision(id).await?;if latest.0!=record_revision||latest.1!=content_revision{return Err(TransferError::PlanStale{message:"source metadata changed while cataloguing".into()});}
        let title=record.title.chars().take(200).collect();let attribute_tag=record.attribute_tag().map(str::to_owned);let mut tags=record.tags;tags.retain(|tag|!virtual_novel::is_internal_tag(tag));
        let metadata=ArchiveNovel{toc_url:record.toc_url,title:record.title,author:record.author,sitename:record.sitename,domain:record.domain,tags,attribute_tag,novel_type:record.novel_type,files:vec![],ncode:if parent{None}else{record.ncode},subtitle:None,general_lastup:record.general_lastup.map(|v|v.to_rfc3339()),last_update:Some(record.last_update.to_rfc3339()),virtual_members:None};
        Ok((ExportNovelEntry{session_id:session.clone(),novel_ordinal:ordinal,id:NovelId(id),toc_url,title,kind:if parent{NovelKind::VirtualParent}else{NovelKind::Normal},membership_digest:if parent{Some(self.membership(id).await?)}else{None},record_revision,content_revision,metadata:None,files:vec![],files_complete:false},metadata))
    }
    async fn source_entry(&self,record:&NovelRecord,path:String,size:u64)->Result<SourceFileEntry,TransferError>{
        narou_rs::transfer::validate_relative_path(&path).map_err(|e|invalid("invalid_path",e.to_string()))?;
        let reference=self.d1.transfer_reference(&key_for(record,&path)?).await.map_err(error)?;
        let source=if reference.is_some_and(|reference|matches!(reference.backend.as_str(),"d1"|"s3")&&reference.size==size){SourceRef::Pinnable}else{SourceRef::Readable};
        Ok(SourceFileEntry{logical_path:path,size,known_role:None,source})
    }
    async fn source_already_catalogued(&self,session:&SessionId,ordinal:u32,path:&str)->Result<bool,TransferError>{
        let key=format!("xsource:{ordinal}:{}",sha256_hex(path.as_bytes()));
        Ok(statement(&self.db,"SELECT 1 seen FROM transfer_journal_entries WHERE session_id=? AND publication_id=4294967295 AND key=?",&[session.0.as_str().into(),key.into()])?.first::<i64>(Some("seen")).await.map_err(error)?.is_some())
    }
    async fn catalog_index(&self,session:&SessionId,entry:&ExportNovelEntry,record:NovelRecord)->Result<Option<String>,TransferError>{
        let source_keys=keys(&record)?;drop(record);
        let previous=statement(&self.db,"SELECT stage_id FROM transfer_catalog_indexes WHERE session_id=? AND novel_ordinal=?",&[session.0.as_str().into(),(entry.novel_ordinal as f64).into()])?.first::<String>(Some("stage_id")).await.map_err(error)?;
        if let Some(id)=previous {let owner=self.stage_owner(&id).await?;if owner.session_id!=session.0||owner.purpose!="illustration_index"{return Err(invalid("source_pin_conflict","catalog index owner differs"));}let store=self.stage_store(&owner.backend)?;
            if let Some(cursor)=store.stage_cursor(&id).await.map_err(error)?{if owner.active!=0&&owner.initialized!=0&&cursor.confirmed_bytes==owner.expected_size{if let Some(sha)=cursor.prefix_sha256(){let sealed=store.stage_seal(&id,&session.0,owner.epoch,owner.expected_size,&sha).await.map_err(error)?;run(&self.db,"UPDATE transfer_stage_owners SET sealed_key=?,sha256=? WHERE stage_id=?",&[sealed.key.as_ref().into(),sealed.sha256.into(),id.as_str().into()]).await?;return Ok(Some(id));}}}
            self.abort_owned_stage(session,&id).await?;
        }
        let key=source_keys.illustration_cache();let Some(meta)=self.objects.stat(&key).await.map_err(error)?else{return Ok(None)};
        if meta.size>16*1024*1024{return Err(TransferError::ExceedsLimit{code:"metadata_budget",message:"illustration metadata exceeds 16 MiB initializer budget".into()});}
        let id=self.opaque_id("index");
        self.journal.ensure_execution(session).await?;
        let results=self.db.batch(vec![self.journal.execution_guard(session)?,statement(&self.db,"INSERT INTO transfer_stage_owners(stage_id,session_id,file_id,expected_size,epoch,backend,purpose) VALUES(?,?,NULL,16777216,1,?,'illustration_index')",&[id.as_str().into(),session.0.as_str().into(),(if self.s3{"s3"}else{"d1"}).into()])?,statement(&self.db,"INSERT INTO transfer_catalog_indexes(session_id,novel_ordinal,stage_id) VALUES(?,?,?) ON CONFLICT(session_id,novel_ordinal) DO UPDATE SET stage_id=excluded.stage_id",&[session.0.as_str().into(),(entry.novel_ordinal as f64).into(),id.as_str().into()])?]).await.map_err(error)?;
        crate::d1_repository::ensure_batch_success(&results).map_err(error)?;
        let store=self.stage_store(if self.s3{"s3"}else{"d1"})?;
        store.stage_open(&id,&session.0,1,SESSION_IDLE_TTL_SECS).await.map_err(error)?;
        let mut scope=narou_rs::application::novel_access::NovelReadScope::new(&self.objects,"transfer-index-initializer");
        let result=async{
            let prefix=ObjectPrefix::from(source_keys.prefix().clone());let mut lease=scope.lease_for(&prefix).await.map_err(error)?;
            let bytes=read_metadata(&mut lease,&key,meta.size).await?.ok_or_else(||invalid("source_missing","illustration metadata disappeared during initialization"))?;
            if bytes.len()>16*1024*1024{return Err(invalid("metadata_budget","illustration metadata exceeds 16 MiB"));}
            let yaml=std::str::from_utf8(&bytes).map_err(metadata_error)?;
            const INDEX_PART_BYTES:usize=5*1024*1024;
            let mut offset=0u64;let mut pending=Vec::with_capacity(INDEX_PART_BYTES);let mut count=0usize;let mut expanded=0u64;
            for name in narou_rs::illustration_store::IllustrationFilenameReader::new(yaml){let name=name.map_err(metadata_error)?;
                count=count.checked_add(1).filter(|n|*n<=50_000).ok_or_else(||invalid("metadata_budget","expanded illustration filename count exceeds 50000"))?;
                expanded=expanded.checked_add(name.len()as u64+1).filter(|n|*n<=16*1024*1024).ok_or_else(||invalid("metadata_budget","expanded serialized filename spool exceeds 16 MiB"))?;
                narou_rs::transfer::validate_relative_path(&format!("挿絵/{name}")).map_err(metadata_error)?;
                for part in [name.as_bytes(),b"\n".as_slice()]{let mut rest=part;while !rest.is_empty(){
                    if pending.len()==INDEX_PART_BYTES{scope.refresh_all().await.map_err(error)?;self.journal.ensure_execution(session).await?;store.stage_append(&id,&session.0,1,offset,&pending,false).await.map_err(error)?;offset+=pending.len()as u64;pending.clear();}
                    let n=(INDEX_PART_BYTES-pending.len()).min(rest.len());pending.extend_from_slice(&rest[..n]);rest=&rest[n..];
                }}
            }
            let latest=self.bound_revision(entry.id.0,prefix.as_ref()).await?;if latest.0!=entry.record_revision||latest.1!=entry.content_revision{return Err(TransferError::PlanStale{message:"source changed after catalog selection".into()});}scope.refresh_all().await.map_err(error)?;self.journal.ensure_execution(session).await?;
            let size=offset+pending.len()as u64;store.stage_append(&id,&session.0,1,offset,&pending,true).await.map_err(error)?;
            let result=self.db.batch(vec![self.journal.execution_guard(session)?,statement(&self.db,"UPDATE transfer_stage_owners SET expected_size=?,initialized=1 WHERE stage_id=? AND active=1",&[(size as f64).into(),id.as_str().into()])?]).await.map_err(error)?;crate::d1_repository::ensure_batch_success(&result).map_err(error)?;
            let cursor=store.stage_cursor(&id).await.map_err(error)?.ok_or_else(||error("catalog index cursor missing"))?;let sha=cursor.prefix_sha256().ok_or_else(||error("catalog index hash missing"))?;let sealed=store.stage_seal(&id,&session.0,1,size,&sha).await.map_err(error)?;
            let result=self.db.batch(vec![self.journal.execution_guard(session)?,statement(&self.db,"UPDATE transfer_stage_owners SET expected_size=?,sealed_key=?,sha256=? WHERE stage_id=?",&[(size as f64).into(),sealed.key.as_ref().into(),sealed.sha256.into(),id.as_str().into()])?]).await.map_err(error)?;crate::d1_repository::ensure_batch_success(&result).map_err(error)?;Ok(Some(id.clone()))
        }.await;
        let released=scope.release_all().await.map_err(error);
        match result{Ok(value)=>{released?;Ok(value)},Err(error)=>{let _=released;if matches!(error,TransferError::InvalidRequest{..}|TransferError::ExceedsLimit{..}){self.abort_owned_stage(session,&id).await?;}Err(error)}}
    }
    async fn snapshot_admit(&self,session:&SessionId)->Result<bool,TransferError>{
        self.journal.ensure_execution(session).await?;
        run(&self.db,"INSERT OR IGNORE INTO transfer_snapshot_state(session_id,admitted) VALUES(?,0)",&[session.0.as_str().into()]).await?;
        #[derive(Serialize,Deserialize)]struct State{admission_cursor:Option<String>,admitted:i64}
        let state=statement(&self.db,"SELECT admission_cursor,admitted FROM transfer_snapshot_state WHERE session_id=?",&[session.0.as_str().into()])?.first::<State>(None).await.map_err(error)?.ok_or_else(||error("snapshot state missing"))?;
        let input=serde_json::to_string(&state).map_err(error)?;let step=self.begin_backend_step(session,"snapshot_admit",&input).await?;
        if let Some(result)=step.result_json{return serde_json::from_str(&result).map_err(error);}
        if step.input_json!=input{return Err(TransferError::RecoveryRequired{message:"snapshot admission cursor advanced without its operation receipt".into()});}
        if state.admitted!=0{return Ok(true);}
        #[derive(Deserialize)]struct Entry{sort_key:String,entry_json:String,prefix:String}
        let rows=statement(&self.db,"SELECT b.prefix||char(1)||printf('%08d',m.novel_ordinal) sort_key,m.entry_json,b.prefix FROM transfer_inventory_metadata m JOIN novel_gate_bindings b ON b.novel_id=CAST(json_extract(m.entry_json,'$.id') AS INTEGER) WHERE m.session_id=? AND b.prefix||char(1)||printf('%08d',m.novel_ordinal)>? ORDER BY sort_key LIMIT 17",&[session.0.as_str().into(),state.admission_cursor.as_deref().unwrap_or("").into()])?.all().await.map_err(error)?.results::<Entry>().map_err(error)?;
        let more=rows.len()>16;let mut last=state.admission_cursor;let mut worked=false;
        for row in rows.into_iter().take(16){let entry:ExportNovelEntry=serde_json::from_str(&row.entry_json).map_err(error)?;let prefix=ObjectPrefix::new(&row.prefix).map_err(error)?;let owner=format!("{}:snapshot:group",session.0);
            self.check_source_namespace(entry.id.0).await?;
            self.journal.ensure_execution(session).await?;
            run(&self.db,"INSERT OR IGNORE INTO transfer_snapshot_gates(session_id,novel_ordinal,prefix,owner,epoch) VALUES(?,?,?,?,0)",&[session.0.as_str().into(),(entry.novel_ordinal as f64).into(),row.prefix.as_str().into(),owner.as_str().into()]).await?;
            let epoch=self.d1.close_gate(&prefix,&owner).await.map_err(error)?;
            run(&self.db,"UPDATE transfer_snapshot_gates SET epoch=? WHERE session_id=? AND novel_ordinal=?",&[(epoch as f64).into(),session.0.as_str().into(),(entry.novel_ordinal as f64).into()]).await?;
            self.check_snapshot(&entry).await?;last=Some(row.sort_key);worked=true;
        }
        if !more{let missing=statement(&self.db,"SELECT 1 missing FROM transfer_inventory_metadata m WHERE m.session_id=? AND NOT EXISTS(SELECT 1 FROM transfer_snapshot_gates s JOIN novel_access_gates g ON g.prefix=s.prefix AND g.owner=s.owner AND g.epoch=s.epoch AND g.closed=1 WHERE s.session_id=m.session_id AND s.novel_ordinal=m.novel_ordinal) LIMIT 1",&[session.0.as_str().into()])?.first::<i64>(Some("missing")).await.map_err(error)?.is_some();if missing{return Err(TransferError::PlanStale{message:"a selected source lost its canonical binding before snapshot admission".into()});}}
        let readers=statement(&self.db,"SELECT COUNT(*) readers FROM novel_reader_leases r WHERE r.expires_at>unixepoch() AND EXISTS(SELECT 1 FROM transfer_snapshot_gates g WHERE g.session_id=? AND g.prefix=r.prefix)",&[session.0.as_str().into()])?.first::<u64>(Some("readers")).await.map_err(error)?.unwrap_or(0);
        let admitted=!more&&readers==0;let done=admitted&&!worked;
        self.journal.ensure_execution(session).await?;
        let result=self.db.batch(vec![self.journal.execution_guard(session)?,statement(&self.db,"UPDATE transfer_snapshot_state SET admission_cursor=?,admitted=? WHERE session_id=?",&[last.as_deref().map(JsValue::from).unwrap_or(JsValue::NULL),(admitted as i32).into(),session.0.as_str().into()])?,self.finish_backend_step(session,"snapshot_admit",if done{"true"}else{"false"})?]).await.map_err(error)?;
        crate::d1_repository::ensure_batch_success(&result).map_err(error)?;
        Ok(done)
    }
    async fn release_snapshot_gate(&self,session:&SessionId,ordinal:u32)->Result<(),TransferError>{
        #[derive(Deserialize)]struct Gate{prefix:String,owner:String,epoch:u64}
        let gate=statement(&self.db,"SELECT prefix,owner,epoch FROM transfer_snapshot_gates WHERE session_id=? AND novel_ordinal=?",&[session.0.as_str().into(),(ordinal as f64).into()])?.first::<Gate>(None).await.map_err(error)?;let Some(gate)=gate else{return Ok(())};
        let others=statement(&self.db,"SELECT 1 held FROM transfer_snapshot_gates WHERE session_id=? AND prefix=? AND novel_ordinal<>? LIMIT 1",&[session.0.as_str().into(),gate.prefix.as_str().into(),(ordinal as f64).into()])?.first::<i64>(Some("held")).await.map_err(error)?.is_some();
        if !others{let prefix=ObjectPrefix::new(&gate.prefix).map_err(error)?;let current=self.d1.gate_info(&prefix).await.map_err(error)?;if current.owner.as_deref()==Some(&gate.owner){self.d1.open_gate(&prefix,&gate.owner,gate.epoch).await.map_err(error)?;}}
        run(&self.db,"DELETE FROM transfer_snapshot_gates WHERE session_id=? AND novel_ordinal=?",&[session.0.as_str().into(),(ordinal as f64).into()]).await
    }
    pub(super) async fn check_source_namespace(&self,id:i64)->Result<(),TransferError>{
        let prefix=statement(&self.db,"SELECT b.prefix FROM novel_gate_bindings b JOIN novels n ON n.id=b.novel_id WHERE b.novel_id=?",&[(id as f64).into()])?.first::<String>(Some("prefix")).await.map_err(error)?.ok_or_else(||invalid("source_missing","selected source has no current namespace binding"))?;
        let [ancestors,lower,upper]=super::publication::namespace_bounds(&prefix)?;
        let values=[prefix.as_str().into(),(id as f64).into(),ancestors,lower,upper];
        let overlap=statement(&self.db,"SELECT 1 overlap WHERE EXISTS(SELECT 1 FROM novel_gate_bindings b JOIN novels n ON n.id=b.novel_id WHERE (b.prefix IN(SELECT value FROM json_each(?3)) OR (b.prefix>=?4 AND b.prefix<?5)) AND NOT(b.prefix=?1 AND b.novel_id=?2)) OR EXISTS(SELECT 1 FROM novel_namespace_claims c WHERE (c.prefix IN(SELECT value FROM json_each(?3)) OR (c.prefix>=?4 AND c.prefix<?5)) AND NOT(c.prefix=?1 AND c.novel_id=?2)) OR EXISTS(SELECT 1 FROM novel_access_gates g WHERE (g.prefix IN(SELECT value FROM json_each(?3)) OR (g.prefix>=?4 AND g.prefix<?5)) AND g.prefix!=?1 AND (g.origin_url_fold!='' OR g.closed!=0))",&values)?.first::<i64>(Some("overlap")).await.map_err(error)?.is_some();
        if overlap{return Err(invalid("source_namespace_overlap","source namespace overlaps another current or retained novel namespace"));}Ok(())
    }
    async fn source_guard(&self,entry:&ExportNovelEntry)->Result<NovelRecord,TransferError>{
        self.check_source_namespace(entry.id.0).await?;
        let record=self.check_snapshot(entry).await?;let prefix=keys(&record)?.prefix().as_ref().to_string();
        let held=statement(&self.db,"SELECT 1 held FROM transfer_snapshot_gates s JOIN novel_access_gates g ON g.prefix=s.prefix AND g.owner=s.owner AND g.epoch=s.epoch AND g.closed=1 JOIN transfer_execution_leases l USING(session_id) WHERE s.session_id=? AND s.prefix=? AND s.novel_ordinal=? AND l.owner=? AND l.expires_at>unixepoch()",&[entry.session_id.0.as_str().into(),prefix.into(),(entry.novel_ordinal as f64).into(),self.journal.execution_owner().into()])?.first::<i64>(Some("held")).await.map_err(error)?.is_some();
        if !held{return Err(TransferError::Busy{code:"snapshot_gate_lost",message:"source read requires the owned closed snapshot gate".into()});}Ok(record)
    }
    async fn initialized_source(&self,entry:&ExportNovelEntry,file:&SourceFileEntry,logical:&ObjectKey)->Result<Option<String>,TransferError>{
        let raw=statement(&self.db,"SELECT 1 raw FROM transfer_object_refs WHERE object_key=?",&[logical.as_ref().into()])?.first::<i64>(Some("raw")).await.map_err(error)?.is_some();
        if raw{return Ok(None);}
        let encoding=statement(&self.db,"SELECT encoding FROM content_objects WHERE object_key=? UNION ALL SELECT encoding FROM objects WHERE object_key=? LIMIT 1",&[logical.as_ref().into(),logical.as_ref().into()])?.first::<String>(Some("encoding")).await.map_err(error)?;
        if encoding.as_deref().is_none_or(|value|value=="none"){return Ok(None);}
        if file.size>16*1024*1024{return Err(TransferError::ExceedsLimit{code:"legacy_metadata_limit",message:"legacy compressed object exceeds its existing decoded storage limit".into()});}
        let prefix={let record=self.check_snapshot(entry).await?;keys(&record)?.prefix().as_ref().to_string()};
        #[derive(Deserialize)]struct Snapshot{session_id:String}
        let snapshot=statement(&self.db,"SELECT s.session_id FROM transfer_snapshot_gates s JOIN novel_access_gates g ON g.prefix=s.prefix AND g.owner=s.owner AND g.epoch=s.epoch AND g.closed=1 WHERE s.session_id=? AND s.prefix=? AND s.novel_ordinal=?",&[entry.session_id.0.as_str().into(),prefix.as_str().into(),(entry.novel_ordinal as f64).into()])?.first::<Snapshot>(None).await.map_err(error)?.ok_or_else(||invalid("snapshot_missing","source initialization requires its mutation gate"))?;
        #[derive(Deserialize)]struct SourcePointer{stage_id:String,source_fingerprint:String}
        let fingerprint=format!("{}:{}",entry.record_revision,entry.content_revision);
        let previous=statement(&self.db,"SELECT stage_id,source_fingerprint FROM transfer_source_materializations WHERE session_id=? AND novel_ordinal=? AND logical_path=?",&[snapshot.session_id.as_str().into(),(entry.novel_ordinal as f64).into(),file.logical_path.as_str().into()])?.first::<SourcePointer>(None).await.map_err(error)?;
        if let Some(pointer)=previous{if pointer.source_fingerprint!=fingerprint{return Err(invalid("snapshot_changed","initialized source revision differs"));}let id=pointer.stage_id;let owner=self.stage_owner(&id).await?;if owner.session_id!=snapshot.session_id||owner.expected_size!=file.size||owner.purpose!="legacy_source"{return Err(invalid("source_pin_conflict","initialized source owner differs"));}let store=self.stage_store(&owner.backend)?;
            if let Some(cursor)=store.stage_cursor(&id).await.map_err(error)?{if owner.active!=0&&owner.initialized!=0&&cursor.confirmed_bytes==file.size{if let Some(sha)=cursor.prefix_sha256(){let sealed=store.stage_seal(&id,&snapshot.session_id,owner.epoch,file.size,&sha).await.map_err(error)?;run(&self.db,"UPDATE transfer_stage_owners SET sealed_key=?,sha256=? WHERE stage_id=?",&[sealed.key.as_ref().into(),sealed.sha256.into(),id.as_str().into()]).await?;return Ok(Some(id));}}}
            self.abort_owned_stage(&SessionId(snapshot.session_id.clone()),&id).await?;
        }
        let id=self.opaque_id("source");
        let results=self.db.batch(vec![self.journal.execution_guard(&entry.session_id)?,statement(&self.db,"INSERT INTO transfer_stage_owners(stage_id,session_id,file_id,expected_size,epoch,backend,purpose) VALUES(?,?,NULL,?,1,?,'legacy_source')",&[id.as_str().into(),snapshot.session_id.as_str().into(),(file.size as f64).into(),(if self.s3{"s3"}else{"d1"}).into()])?,statement(&self.db,"INSERT INTO transfer_source_materializations(session_id,novel_ordinal,logical_path,stage_id,source_fingerprint) VALUES(?,?,?,?,?) ON CONFLICT(session_id,novel_ordinal,logical_path) DO UPDATE SET stage_id=excluded.stage_id,source_fingerprint=excluded.source_fingerprint",&[snapshot.session_id.as_str().into(),(entry.novel_ordinal as f64).into(),file.logical_path.as_str().into(),id.as_str().into(),fingerprint.into()])?]).await.map_err(error)?;
        crate::d1_repository::ensure_batch_success(&results).map_err(error)?;
        self.stage.stage_open(&id,&snapshot.session_id,1,SESSION_IDLE_TTL_SECS).await.map_err(error)?;
        let mut input=self.assets.read_stream(logical).await.map_err(error)?.ok_or_else(||invalid("source_missing","legacy source disappeared"))?;
        const INITIALIZER_PART_BYTES:usize=5*1024*1024;
        let mut pending=Vec::with_capacity(INITIALIZER_PART_BYTES);let mut written=0u64;
        while let Some(chunk)=input.next().await{let chunk=chunk.map_err(error)?;if chunk.len()>64*1024{return Err(invalid("codec_io_limit","legacy decoder exceeded 64 KiB output"));}
            let mut offset=0;while offset<chunk.len(){
                if pending.len()==INITIALIZER_PART_BYTES{self.journal.ensure_execution(&entry.session_id).await?;self.stage.stage_append(&id,&snapshot.session_id,1,written,&pending,false).await.map_err(error)?;written+=pending.len()as u64;pending.clear();}
                let take=(INITIALIZER_PART_BYTES-pending.len()).min(chunk.len()-offset);pending.extend_from_slice(&chunk[offset..offset+take]);offset+=take;
                if written+pending.len() as u64>file.size{return Err(invalid("source_size_mismatch","legacy source exceeds catalog size"));}
            }
        }
        if written+pending.len() as u64!=file.size{return Err(invalid("source_short_read","legacy source ended before catalog size"));}
        // The decoder's final EOF (including CRC validation) precedes the final
        // append. A failed decoder can never leave a reusable complete stage.
        self.stage.stage_append(&id,&snapshot.session_id,1,written,&pending,true).await.map_err(error)?;
        let result=self.db.batch(vec![self.journal.execution_guard(&entry.session_id)?,statement(&self.db,"UPDATE transfer_stage_owners SET initialized=1 WHERE stage_id=? AND active=1",&[id.as_str().into()])?]).await.map_err(error)?;crate::d1_repository::ensure_batch_success(&result).map_err(error)?;
        let cursor=self.stage.stage_cursor(&id).await.map_err(error)?.ok_or_else(||error("initializer cursor missing"))?;let sha=cursor.prefix_sha256().ok_or_else(||error("initializer hash checkpoint missing"))?;
        let sealed=self.stage.stage_seal(&id,&snapshot.session_id,1,file.size,&sha).await.map_err(error)?;
        run(&self.db,"UPDATE transfer_stage_owners SET sealed_key=?,sha256=? WHERE stage_id=?",&[sealed.key.as_ref().into(),sealed.sha256.into(),id.into()]).await?;
        // The legacy initializer is one explicitly bounded <=16MiB unit;
        // defer the first <=8MiB range copy to the next invocation.
        Err(TransferError::Busy{code:"snapshot_preparing",message:"legacy source initialized; retry the same advance identity".into()})
    }
    pub(super) async fn resolve_record(&self,entry:&ExportNovelEntry)->Result<NovelRecord,TransferError>{self.repository.get(entry.id).await.map_err(error)?.ok_or_else(||invalid("source_missing","source novel was deleted"))}
    pub(super) async fn check_snapshot(&self,entry:&ExportNovelEntry)->Result<NovelRecord,TransferError>{let record=self.resolve_record(entry).await?;if self.revision(record.id).await?!=(entry.record_revision.clone(),entry.content_revision.clone()){return Err(TransferError::PlanStale{message:"source changed after catalog selection".into()});}if let Some(expected)=entry.membership_digest.as_deref(){if self.membership(record.id).await?!=expected{return Err(TransferError::PlanStale{message:"source parent membership changed after catalog selection".into()});}}Ok(record)}
}
impl TransferBackend for WorkerTransferBackend{
    fn opaque_id(&self,prefix:&str)->String{format!("{prefix}_{}",crate::content_store::nonce().expect("Worker Web Crypto is required"))}
    fn storage_capabilities<'a>(&'a self,_scope:&'a AuthScope)->PlatformFuture<'a,Result<StorageCapabilities,TransferError>>{Box::pin(async move{
        let available=option_env!("NAROU_TRANSFER_BUILD_ID").is_some_and(|id|id.len()==64&&id.bytes().all(|b|b.is_ascii_hexdigit()))&&option_env!("NAROU_TRANSFER_ABI_VERSION")==Some("2");
        let gate=CapabilityGate{allowed:available,reason:(!available).then(||"this build has no matching transfer WASM artifacts".into())};
        let max_file=if self.s3{CHUNK_BYTES*10_000}else{10u64*1024*1024*1024};
        let backend_limits=BTreeMap::from([
            ("max_file_bytes".into(),max_file.to_string()),
            ("max_stage_bytes".into(),(if self.s3{u64::MAX}else{10u64*1024*1024*1024}).to_string()),
            ("max_multipart_parts".into(),(if self.s3{10_000}else{0}).to_string()),
            ("max_indexed_url_bytes".into(),"1800".into()),
            ("max_record_bytes".into(),"1900000".into()),
            ("max_control_file_bytes".into(),"16777216".into()),
        ]);
        Ok(StorageCapabilities{policy_version:1,methods:if self.s3{vec!["d1".into(),"s3".into()]}else{vec!["d1".into()]},import:gate.clone(),export:gate,unpersistable_roles:vec![FileRole::Raw,FileRole::Internal],supports_reuse:true,available_bytes:None,backend_limits})
    })}
    fn issue_ticket<'a>(&'a self,scope:&'a AuthScope,_valid_for_secs:i64)->PlatformFuture<'a,Result<String,TransferError>>{Box::pin(async move{Ok(ticket::issue(&self.signing_key,scope,&self.opaque_id("ticket"),Utc::now().timestamp()))})}
    fn verify_ticket<'a>(&'a self,scope:&'a AuthScope,value:&'a str)->PlatformFuture<'a,Result<bool,TransferError>>{Box::pin(async move{ticket::verify(&self.signing_key,scope,value,Utc::now().timestamp(),CREATION_TICKET_TTL_SECS).map(|_|true)})}
    fn library_index<'a>(&'a self,_scope:&'a AuthScope,urls:&'a[String])->PlatformFuture<'a,Result<Vec<LibraryMatch>,TransferError>>{Box::pin(async move{
        #[derive(Deserialize)]struct Row{id:i64,title:String,ncode:Option<String>,record_revision:u64,content_revision:u64}
        let mut out=Vec::new();for page in urls.chunks(100){let folded=page.iter().map(|s|narou_rs::db::novel_codec::fold(s)).collect::<Vec<_>>();
            let rows=statement(&self.db,"SELECT n.id,substr(n.title,1,200) title,CASE WHEN length(n.ncode)<=24 THEN n.ncode ELSE NULL END ncode,COALESCE(r.record_revision,0) record_revision,COALESCE(r.content_revision,0) content_revision FROM json_each(?) j JOIN novels n ON n.toc_url_fold=j.value LEFT JOIN novel_revisions r ON r.novel_id=n.id ORDER BY n.id",&[serde_json::to_string(&folded).map_err(error)?.into()])?.all().await.map_err(error)?.results::<Row>().map_err(error)?;
            for row in rows{
                let parent=row.ncode.as_deref().is_some_and(virtual_novel::is_virtual_ncode);let(record_revision,content_revision)=self.revision(row.id).await?;
                if record_revision!=row.record_revision.to_string()||content_revision.split(':').next()!=Some(row.content_revision.to_string().as_str()){return Err(TransferError::PlanStale{message:"destination metadata changed during library lookup".into()});}
                let membership_digest=if parent{Some(self.membership(row.id).await?)}else{None};
                if self.revision(row.id).await?!=(record_revision.clone(),content_revision.clone()){return Err(TransferError::PlanStale{message:"destination metadata changed while capturing membership".into()});}
                out.push(LibraryMatch{id:NovelId(row.id),title:row.title,kind:if parent{NovelKind::VirtualParent}else{NovelKind::Normal},record_revision,content_revision,membership_digest});
            }
        }Ok(out)
    })}
    fn find_reusable<'a>(&'a self,_scope:&'a AuthScope,session:&'a SessionRow,file:&'a FileRow)->PlatformFuture<'a,Result<Option<PinRef>,TransferError>>{Box::pin(async move{
        let target=statement(&self.db,"SELECT existing_id FROM transfer_plan_entries WHERE session_id=? AND novel_ordinal=?",&[session.session_id.0.as_str().into(),(file.novel_ordinal as f64).into()])?.first::<i64>(Some("existing_id")).await.map_err(error)?;
        let Some(id)=target else{return Ok(None)};let Some(record)=self.repository.get(NovelId(id)).await.map_err(error)?else{return Ok(None)};
        let key=key_for(&record,&file.logical_path)?;let row=statement(&self.db,"SELECT blob_key,size,sha256,backend FROM transfer_object_refs WHERE object_key=? AND size=? AND sha256=?",&[key.as_ref().into(),(file.size as f64).into(),file.sha256.as_str().into()])?.first::<RefRow>(None).await.map_err(error)?;
        let Some(row)=row else{return Ok(None)};let pin=format!("pin_{}",sha256_hex(format!("{}:{}",session.session_id.0,file.file_id).as_bytes()));
        run(&self.db,"INSERT OR IGNORE INTO transfer_pins(pin,session_id,stage,size,sha256,backend) SELECT ?,?,blob_key,size,sha256,backend FROM transfer_object_refs WHERE blob_key=? AND size=? AND sha256=? AND backend=? LIMIT 1",&[pin.as_str().into(),session.session_id.0.as_str().into(),row.blob_key.as_str().into(),(row.size as f64).into(),row.sha256.as_str().into(),row.backend.as_str().into()]).await?;
        let adopted=statement(&self.db,"SELECT stage,size,sha256,backend FROM transfer_pins WHERE pin=? AND session_id=?",&[pin.as_str().into(),session.session_id.0.as_str().into()])?.first::<PinRow>(None).await.map_err(error)?;
        let Some(adopted)=adopted else{return Ok(None)};
        if adopted.size!=file.size||!adopted.sha256.eq_ignore_ascii_case(&file.sha256){return Err(TransferError::ChunkConflict{code:"reuse_pin_conflict",message:"retained reuse pin has another content identity".into()});}
        Ok(Some(PinRef{pin,size:adopted.size,sha256:adopted.sha256}))
    })}
    fn release_pin<'a>(&'a self,session:&'a SessionId,pin:&'a str)->PlatformFuture<'a,Result<(),TransferError>>{Box::pin(async move{run(&self.db,"DELETE FROM transfer_pins WHERE session_id=? AND pin=?",&[session.0.as_str().into(),pin.into()]).await})}
    fn stage_begin<'a>(&'a self,session:&'a SessionId,file_id:u32,expected_size:u64)->PlatformFuture<'a,Result<String,TransferError>>{Box::pin(async move{
        if expected_size>9_007_199_254_740_991||self.s3&&expected_size>CHUNK_BYTES*10_000||!self.s3&&expected_size>10*1024*1024*1024{return Err(TransferError::ExceedsLimit{code:"backend_file_limit",message:"file exceeds backend integer/multipart capacity".into()});}
        self.journal.ensure_execution(session).await?;
        let id=self.opaque_id("file");
        let file=self.journal.file(session,file_id).await?;
        if let Some(file)=&file{if matches!(file.role,FileRole::Raw|FileRole::Internal)&&file_id!=MANIFEST_FILE_ID{return Err(invalid("file_policy","Worker never stages raw or internal files"));}}
        let backend=if self.s3&&file_id!=MANIFEST_FILE_ID&&!file.as_ref().is_some_and(|file|crate::content_store::metadata_in_d1(file.role)){"s3"}else{"d1"};
        run(&self.db,"INSERT OR IGNORE INTO transfer_stage_owners(stage_id,session_id,file_id,expected_size,epoch,backend) SELECT ?,?,?,?,1,? WHERE EXISTS(SELECT 1 FROM transfer_sessions WHERE session_id=? AND state NOT IN ('completed','completed_with_errors','released','cancelled','failed','expired')) AND (?='s3' OR COALESCE((SELECT SUM(o.expected_size) FROM transfer_stage_owners o JOIN transfer_sessions s USING(session_id) WHERE o.backend='d1' AND o.active=1 AND o.stage_id<>? AND s.state NOT IN ('completed','completed_with_errors','released','cancelled','failed','expired')),0)+?<=10737418240)",&[id.as_str().into(),session.0.as_str().into(),(file_id as f64).into(),(expected_size as f64).into(),backend.into(),session.0.as_str().into(),backend.into(),id.as_str().into(),(expected_size as f64).into()]).await?;
        let owner=statement(&self.db,"SELECT * FROM transfer_stage_owners WHERE session_id=? AND file_id=? AND active=1",&[session.0.as_str().into(),(file_id as f64).into()])?.first::<StageOwner>(None).await.map_err(error)?.ok_or_else(||TransferError::InsufficientStorage{message:"outstanding transfer reservations exceed the D1 storage profile".into()})?;
        let id=owner.stage_id.clone();
        let owner=self.live_stage(&id).await?;if owner.expected_size!=expected_size{return Err(invalid("stage_conflict","stage expected size changed"));}
        self.stage_store(&owner.backend)?.stage_open(&id,&session.0,owner.epoch,SESSION_IDLE_TTL_SECS).await.map_err(error)?;Ok(id)
    })}
    fn stage_append<'a>(&'a self,handle:&'a str,offset:u64,data:&'a[u8])->PlatformFuture<'a,Result<(),TransferError>>{Box::pin(async move{
        let owner=self.live_stage(handle).await?;let end=offset.checked_add(data.len() as u64).ok_or_else(||invalid("invalid_offset","offset overflow"))?;
        if end>owner.expected_size{return Err(invalid("invalid_offset","stage exceeds declared size"));}
        let digest=sha256_hex(data);
        run(&self.db,"INSERT OR IGNORE INTO transfer_stage_parts(stage_id,offset,size,sha256) VALUES(?,?,?,?)",&[handle.into(),(offset as f64).into(),(data.len() as f64).into(),digest.as_str().into()]).await?;
        let part=statement(&self.db,"SELECT size,sha256 FROM transfer_stage_parts WHERE stage_id=? AND offset=?",&[handle.into(),(offset as f64).into()])?.first::<StagePart>(None).await.map_err(error)?.ok_or_else(||error("missing reserved stage part"))?;
        if part.size!=data.len() as u64||part.sha256!=digest{return Err(TransferError::ChunkConflict{code:"chunk_conflict",message:"stage part already has different bytes".into()});}
        let cursor=self.stage_store(&owner.backend)?.stage_cursor(handle).await.map_err(error)?.ok_or_else(||invalid("stage_expired","stage cursor missing"))?;
        if cursor.confirmed_bytes==end&&(data.len()>0||cursor.sealed){return Ok(());}
        if cursor.confirmed_bytes!=offset{return Err(TransferError::ChunkConflict{code:"unexpected_offset",message:"stage cursor differs from confirmed offset".into()});}
        self.stage_store(&owner.backend)?.stage_append(handle,&owner.session_id,owner.epoch,offset,data,end==owner.expected_size).await.map_err(error)?;Ok(())
    })}
    fn stage_seal<'a>(&'a self,handle:&'a str,size:u64,sha:&'a str)->PlatformFuture<'a,Result<String,TransferError>>{Box::pin(async move{let owner=self.live_stage(handle).await?;
        if size!=owner.expected_size{return Err(invalid("size_mismatch","seal size differs from stage reservation"));}
        if owner.sha256.as_deref().is_some_and(|expected|!expected.eq_ignore_ascii_case(sha)){return Err(TransferError::ChunkConflict{code:"stage_identity_mismatch",message:"stage finalization has another expected digest".into()});}
        let session=SessionId(owner.session_id.clone());self.journal.ensure_execution(&session).await?;
        let result=self.db.batch(vec![self.journal.execution_guard(&session)?,statement(&self.db,"UPDATE transfer_stage_owners SET sha256=? WHERE stage_id=? AND epoch=? AND active=1",&[sha.into(),handle.into(),(owner.epoch as f64).into()])?]).await.map_err(error)?;crate::d1_repository::ensure_batch_success(&result).map_err(error)?;
        let sealed=self.stage_store(&owner.backend)?.stage_seal(handle,&owner.session_id,owner.epoch,size,sha).await.map_err(error)?;
        let result=self.db.batch(vec![self.journal.execution_guard(&session)?,statement(&self.db,"UPDATE transfer_stage_owners SET sealed_key=?,sha256=? WHERE stage_id=? AND epoch=? AND active=1",&[sealed.key.as_ref().into(),sealed.sha256.as_str().into(),handle.into(),(owner.epoch as f64).into()])?]).await.map_err(error)?;crate::d1_repository::ensure_batch_success(&result).map_err(error)?;Ok(handle.to_string())
    })}
    fn stage_needs_finalize(&self)->bool{self.s3}
    fn stage_finalize<'a>(&'a self,handle:&'a str,_cursor:Option<&'a str>)->PlatformFuture<'a,Result<PublicationStep,TransferError>>{Box::pin(async move{let owner=self.stage_owner(handle).await?;let sha=owner.sha256.as_deref().ok_or_else(||invalid("stage_unsealed","stage seal is not durable"))?;self.stage_store(&owner.backend)?.stage_seal(handle,&owner.session_id,owner.epoch,owner.expected_size,sha).await.map_err(error)?;Ok(PublicationStep{done:true,..Default::default()})})}
    fn stage_read_range<'a>(&'a self,handle:&'a str,range:Range<u64>)->PlatformFuture<'a,Result<Vec<u8>,TransferError>>{Box::pin(async move{
        let expected=range.end.checked_sub(range.start).ok_or_else(||invalid("invalid_range","unordered range"))?;
        if expected>MAX_BINARY_BODY_BYTES as u64{return Err(invalid("invalid_range","range exceeds binary budget"));}
        let stream=self.stage_stream(handle,range).await?;
        collect(stream.ok_or_else(||invalid("snapshot_missing","stage bytes are missing"))?,expected).await
    })}
    fn stage_abort<'a>(&'a self,session:&'a SessionId,handle:&'a str)->PlatformFuture<'a,Result<(),TransferError>>{Box::pin(async move{if handle.starts_with("pin_"){return self.release_pin(session,handle).await;}self.abort_owned_stage(session,handle).await.map(|_|())})}
    fn export_metadata_page<'a>(&'a self,session:&'a SessionId,novel_ordinal:u32,cursor:&'a narou_rs::application::transfer::MetadataCursor,limit:usize)->PlatformFuture<'a,Result<ExportMetadataPage,TransferError>>{Box::pin(self.frozen_metadata_page(session,novel_ordinal,cursor,limit))}
    fn export_inventory<'a>(&'a self,_scope:&'a AuthScope,session:&'a SessionId,ids:&'a[i64],cursor:Option<&'a str>,_limit:usize)->PlatformFuture<'a,Result<Page<ExportNovelEntry>,TransferError>>{Box::pin(async move{
        self.journal.ensure_execution(session).await?;
        let mut position:InventoryCursor=cursor.map(serde_json::from_str).transpose().map_err(|_|invalid("invalid_cursor","invalid export inventory cursor"))?.unwrap_or_default();
        if !super::publication::namespace_index_ready(self,session).await?{return Ok(Page{items:vec![],next_cursor:Some(serde_json::to_string(&position).map_err(error)?)});}
        if !self.freeze_selection(&session,ids).await?{return Ok(Page{items:vec![],next_cursor:Some(serde_json::to_string(&position).map_err(error)?)});}
        let Some(source)=self.frozen_source(&session,position.ordinal).await?else{return Ok(Page{items:vec![],next_cursor:None})};
        let Some(mut entry)=self.frozen_inventory(&session,&source,position.ordinal,!position.metadata_emitted).await?else{return Ok(Page{items:vec![],next_cursor:Some(serde_json::to_string(&position).map_err(error)?)});};
        let record=self.check_snapshot(&entry).await?;
        if !position.metadata_emitted {
            if entry.metadata.is_none(){return Err(invalid("metadata_missing","frozen source metadata is incomplete"));}
            position.metadata_emitted=true;entry.files.clear();entry.files_complete=false;
            return Ok(Page{items:vec![entry],next_cursor:Some(serde_json::to_string(&position).map_err(error)?)});
        }
        entry.metadata=None;
        if entry.kind==NovelKind::VirtualParent{
            if source.member_count==0{return Err(invalid("missing_member","virtual parent has no members"));}
            for path in VIRTUAL_INPUT_FILES{let key=key_for(&record,path)?;if let Some(meta)=self.objects.stat(&key).await.map_err(error)?{entry.files.push(self.source_entry(&record,path.to_string(),meta.size).await?);}}entry.files_complete=true;position.ordinal+=1;position.metadata_emitted=false;position.phase=0;position.after=None;
        }else{
            if source.member_count!=0{return Err(invalid("invalid_member","ordinary work has a parent membership entry"));}
            match position.phase{
                0|1=>{
                    let prefix=if position.phase==0{keys(&record)?.prefix().as_ref().to_string()}else{format!("{}/挿絵",keys(&record)?.prefix())};
                    let mut request=ObjectListRequest::new(ObjectPrefix::new(prefix).map_err(error)?,NonZeroUsize::new(100).unwrap());request.cursor=position.after.clone();
                    let page=self.objects.list_page(&request).await.map_err(error)?;
                    for meta in page.objects{let root=format!("{}/",keys(&record)?.prefix());let Some(path)=meta.key.as_ref().strip_prefix(&root)else{return Err(error("storage listing escaped requested novel prefix"));};
                        if position.phase==0&&path.starts_with("挿絵/"){continue;}if narou_rs::transfer::projection::classify(path,false,&ClassifyContext::default())==FileRole::Internal{continue;}
                        if !self.source_already_catalogued(&session,position.ordinal,path).await?{entry.files.push(self.source_entry(&record,path.to_string(),meta.size).await?);}
                    }
                    match page.next_cursor{Some(next)=>{if Some(&next)==position.after.as_ref(){return Err(error("storage returned non-advancing cursor"));}position.after=Some(next);},None=>{position.phase+=1;position.after=None;}}
                },
                2=>{
                    if position.index_stage.is_none(){position.index_stage=self.catalog_index(&session,&entry,record).await?;position.index_offset=0;
                        if position.index_stage.is_none(){entry.files_complete=true;position.ordinal+=1;position.metadata_emitted=false;position.phase=0;}
                    }else{
                        let stage=position.index_stage.as_ref().ok_or_else(||error("missing index stage"))?;let owner=self.stage_owner(stage).await?;let end=(position.index_offset+256*1024).min(owner.expected_size);
                        let bytes=collect(self.stage_store(&owner.backend)?.stage_read(stage,position.index_offset..end).await.map_err(error)?.ok_or_else(||invalid("snapshot_missing","catalog index spool missing"))?,end-position.index_offset).await?;let mut consumed=0usize;let mut records=0;let mut page_names=HashSet::new();
                        for line in bytes.split_inclusive(|b|*b==b'\n'){if records==100{break;}if line.last()!=Some(&b'\n'){if end==owner.expected_size{return Err(invalid("metadata_truncated","index filename did not terminate"));}break;}
                            let name=std::str::from_utf8(&line[..line.len()-1]).map_err(error)?;let path=format!("挿絵/{name}");
                            if page_names.insert(path.clone())&&!self.source_already_catalogued(&session,position.ordinal,&path).await?{let key=keys(&record)?.illustration(name).map_err(error)?;let meta=self.objects.stat(&key).await.map_err(error)?.ok_or_else(||invalid("source_missing","indexed illustration is missing"))?;entry.files.push(self.source_entry(&record,path,meta.size).await?);}
                            consumed+=line.len();records+=1;
                        }
                        position.index_offset+=consumed as u64;if position.index_offset==owner.expected_size{entry.files_complete=true;position.ordinal+=1;position.metadata_emitted=false;position.phase=0;position.after=None;position.index_stage=None;position.index_offset=0;}
                    }
                },_=>return Err(invalid("invalid_cursor","invalid catalog phase")),
            }
        }
        let next_cursor=if self.frozen_source(&session,position.ordinal).await?.is_some(){Some(serde_json::to_string(&position).map_err(error)?)}else{None};
        Ok(Page{items:vec![entry],next_cursor})
    })}
    fn snapshot_gate<'a>(&'a self,session:&'a SessionId,entry:&'a ExportNovelEntry,release:bool)->PlatformFuture<'a,Result<PublicationStep,TransferError>>{Box::pin(async move{
        if release{return Ok(PublicationStep{done:true,..Default::default()});}
        if !self.snapshot_admit(session).await?{return Ok(PublicationStep{done:false,..Default::default()});}
        let held=statement(&self.db,"SELECT 1 held FROM transfer_snapshot_gates WHERE session_id=? AND novel_ordinal=?",&[session.0.as_str().into(),(entry.novel_ordinal as f64).into()])?.first::<i64>(Some("held")).await.map_err(error)?.is_some();
        if held{self.check_snapshot(entry).await?;}
        Ok(PublicationStep{done:true,..Default::default()})
    })}
    fn snapshot_finish<'a>(&'a self,session:&'a SessionId)->PlatformFuture<'a,Result<PublicationStep,TransferError>>{Box::pin(async move{
        if !self.snapshot_admit(session).await?{return Ok(PublicationStep{done:false,..Default::default()});}
        #[derive(Serialize,Deserialize)]struct Remaining{novel_ordinal:u32,entry_json:String}
        let rows=statement(&self.db,"SELECT g.novel_ordinal,m.entry_json FROM transfer_snapshot_gates g JOIN transfer_inventory_metadata m USING(session_id,novel_ordinal) WHERE g.session_id=? ORDER BY g.prefix,g.novel_ordinal LIMIT 16",&[session.0.as_str().into()])?.all().await.map_err(error)?.results::<Remaining>().map_err(error)?;
        let step=self.begin_backend_step(session,"snapshot_finish",&serde_json::to_string(&rows).map_err(error)?).await?;
        if let Some(result)=step.result_json{return Ok(PublicationStep{done:serde_json::from_str(&result).map_err(error)?,..Default::default()});}
        let rows:Vec<Remaining>=serde_json::from_str(&step.input_json).map_err(error)?;
        let incomplete=statement(&self.db,"SELECT 1 pending FROM transfer_catalog_entries c WHERE c.session_id=? AND c.selected=1 AND NOT EXISTS(SELECT 1 FROM transfer_files f WHERE f.session_id=c.session_id AND f.novel_ordinal=c.novel_ordinal AND f.logical_path=c.logical_path AND f.state IN ('staged','pinned') AND f.confirmed_bytes=f.size AND f.sha256<>'' AND (f.stage IS NOT NULL OR f.pin IS NOT NULL)) LIMIT 1",&[session.0.as_str().into()])?.first::<i64>(Some("pending")).await.map_err(error)?.is_some();
        if incomplete{return Err(invalid("snapshot_incomplete","all selected files must be pinned before any snapshot gate reopens"));}
        let done=rows.is_empty();
        for row in rows{let held=statement(&self.db,"SELECT 1 held FROM transfer_snapshot_gates WHERE session_id=? AND novel_ordinal=?",&[session.0.as_str().into(),(row.novel_ordinal as f64).into()])?.first::<i64>(Some("held")).await.map_err(error)?.is_some();if held{let entry:ExportNovelEntry=serde_json::from_str(&row.entry_json).map_err(error)?;self.check_snapshot(&entry).await?;self.release_snapshot_gate(session,row.novel_ordinal).await?;}}
        let result=self.db.batch(vec![self.journal.execution_guard(session)?,self.finish_backend_step(session,"snapshot_finish",if done{"true"}else{"false"})?]).await.map_err(error)?;crate::d1_repository::ensure_batch_success(&result).map_err(error)?;
        Ok(PublicationStep{done,..Default::default()})
    })}
    fn pin_source<'a>(&'a self,session:&'a SessionId,entry:&'a ExportNovelEntry,file:&'a SourceFileEntry)->PlatformFuture<'a,Result<PinRef,TransferError>>{Box::pin(async move{
        if session!=&entry.session_id{return Err(TransferError::SecretMismatch);}
        let record=self.source_guard(entry).await?;let key=key_for(&record,&file.logical_path)?;let row=statement(&self.db,"SELECT blob_key,size,sha256,backend FROM transfer_object_refs WHERE object_key=?",&[key.as_ref().into()])?.first::<RefRow>(None).await.map_err(error)?.ok_or_else(||invalid("snapshot_unverified","source is not a verified immutable reference"))?;
        let pin=format!("pin_{}",sha256_hex(format!("source:{}:{}:{}",session.0,entry.novel_ordinal,file.logical_path).as_bytes()));
        run(&self.db,"INSERT OR IGNORE INTO transfer_pins(pin,session_id,stage,size,sha256,backend) SELECT ?,?,blob_key,size,sha256,backend FROM transfer_object_refs WHERE object_key=? AND blob_key=? AND size=? AND sha256=? AND backend=?",&[pin.as_str().into(),session.0.as_str().into(),key.as_ref().into(),row.blob_key.as_str().into(),(row.size as f64).into(),row.sha256.as_str().into(),row.backend.as_str().into()]).await?;
        let adopted=statement(&self.db,"SELECT stage,size,sha256,backend FROM transfer_pins WHERE pin=? AND session_id=?",&[pin.as_str().into(),session.0.as_str().into()])?.first::<PinRow>(None).await.map_err(error)?.ok_or_else(||invalid("snapshot_missing","source pin was not retained"))?;
        if adopted.stage!=row.blob_key||adopted.backend!=row.backend||adopted.size!=row.size||adopted.size!=file.size||!adopted.sha256.eq_ignore_ascii_case(&row.sha256){return Err(TransferError::ChunkConflict{code:"snapshot_pin_conflict",message:"source pin identity changed".into()});}
        Ok(PinRef{pin,size:adopted.size,sha256:adopted.sha256})
    })}
    fn source_read_range<'a>(&'a self,entry:&'a ExportNovelEntry,file:&'a SourceFileEntry,range:Range<u64>)->PlatformFuture<'a,Result<Vec<u8>,TransferError>>{Box::pin(async move{let record=self.source_guard(entry).await?;let expected=range.end.checked_sub(range.start).ok_or_else(||invalid("invalid_range","unordered range"))?;let key=key_for(&record,&file.logical_path)?;drop(record);
        if let Some(stage)=self.initialized_source(entry,file,&key).await?{return self.stage_read_range(&stage,range).await;}let stream=self.assets.read_stream_range(&key,range).await.map_err(error)?.ok_or_else(||invalid("source_missing","selected source file is missing"))?;collect(stream,expected).await})}
    fn source_fingerprint<'a>(&'a self,entry:&'a ExportNovelEntry,file:&'a SourceFileEntry)->PlatformFuture<'a,Result<Option<String>,TransferError>>{Box::pin(async move{let record=self.check_snapshot(entry).await?;let meta=self.objects.stat(&key_for(&record,&file.logical_path)?).await.map_err(error)?;Ok(meta.map(|m|format!("{}:{}:{}:{}",entry.record_revision,entry.content_revision,m.size,m.etag.unwrap_or_default())))})}
    fn publication_gate_close<'a>(&'a self,spec:&'a PublicationSpec,cursor:Option<&'a str>)->PlatformFuture<'a,Result<PublicationStep,TransferError>>{Box::pin(publication::gate_close(self,spec,cursor))}
    fn publication_drain<'a>(&'a self,spec:&'a PublicationSpec,cursor:Option<&'a str>)->PlatformFuture<'a,Result<PublicationStep,TransferError>>{Box::pin(publication::drain(self,spec,cursor))}
    fn publication_prepare<'a>(&'a self,spec:&'a PublicationSpec,cursor:Option<&'a str>)->PlatformFuture<'a,Result<PublicationStep,TransferError>>{Box::pin(publication::prepare(self,spec,cursor))}
    fn publication_install<'a>(&'a self,spec:&'a PublicationSpec,cursor:Option<&'a str>)->PlatformFuture<'a,Result<PublicationStep,TransferError>>{Box::pin(publication::install(self,spec,cursor))}
    fn publication_commit<'a>(&'a self,spec:&'a PublicationSpec,cursor:Option<&'a str>)->PlatformFuture<'a,Result<PublicationStep,TransferError>>{Box::pin(publication::commit(self,spec,cursor))}
    fn publication_rollback<'a>(&'a self,spec:&'a PublicationSpec,cursor:Option<&'a str>)->PlatformFuture<'a,Result<PublicationStep,TransferError>>{Box::pin(publication::rollback(self,spec,cursor))}
    fn publication_finish<'a>(&'a self,spec:&'a PublicationSpec,cursor:Option<&'a str>)->PlatformFuture<'a,Result<PublicationStep,TransferError>>{Box::pin(publication::finish(self,spec,cursor))}
    fn publication_gate_open<'a>(&'a self,spec:&'a PublicationSpec,cursor:Option<&'a str>)->PlatformFuture<'a,Result<PublicationStep,TransferError>>{Box::pin(publication::gate_open(self,spec,cursor))}
    fn release_session_resources<'a>(&'a self,session:&'a SessionRow,cursor:Option<&'a str>)->PlatformFuture<'a,Result<PublicationStep,TransferError>>{Box::pin(publication::release(self,session,cursor))}
}

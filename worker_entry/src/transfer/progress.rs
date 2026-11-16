//! Immutable candidates, per-novel gates, and one primary D1 commit point.
//! No canonical bytes change before commit, so pre-commit recovery discards
//! only this publication's candidates; old refs/metadata remain untouched.
use std::num::NonZeroUsize;
use chrono::Utc;
use narou_rs::{application::transfer::*,db::NovelRecord,downloader::{persistence::TocMetadata,util::{load_length_limit,sanitize_filename_with_limit}},platform::{NovelAccessFence,NovelId,NovelRepository,ObjectListRequest,ObjectPrefix},transfer::projection,virtual_novel};
use serde::{Deserialize,Serialize};
use worker::wasm_bindgen::JsValue;
use super::backend::{WorkerTransferBackend,error,invalid,statement,run,keys,key_for};

#[derive(Deserialize)]
struct Target{assigned_id:Option<i64>,reservation_generation:Option<i64>,prefix:Option<String>,epoch:u64,record_json:Option<String>,summary_json:Option<String>,committed:i64,rolled_back:i64,receipt:Option<String>}
#[derive(Default,Serialize,Deserialize)]
struct ParentSummary{tags:Vec<String>,latest:Option<chrono::DateTime<Utc>>,count:i64,novel:bool,manga:bool,next:usize}
fn owner(spec:&PublicationSpec)->String{format!("{}:publish:{}:{}",spec.session_id.0,spec.publication_id,spec.plan_revision)}
fn binds(spec:&PublicationSpec)->[JsValue;2]{[spec.session_id.0.as_str().into(),(spec.publication_id as f64).into()]}
async fn optional_target(backend:&WorkerTransferBackend,spec:&PublicationSpec)->Result<Option<Target>,TransferError>{statement(&backend.db,"SELECT * FROM transfer_targets WHERE session_id=? AND publication_id=?",&binds(spec))?.first(None).await.map_err(error)}
async fn target(backend:&WorkerTransferBackend,spec:&PublicationSpec)->Result<Target,TransferError>{statement(&backend.db,"SELECT * FROM transfer_targets WHERE session_id=? AND publication_id=?",&binds(spec))?.first(None).await.map_err(error)?.ok_or_else(||error("publication target is not reserved"))}
fn record(row:&Target)->Result<NovelRecord,TransferError>{serde_json::from_str(row.record_json.as_deref().ok_or_else(||error("publication metadata is not prepared"))?).map_err(error)}
fn prefix(row:&Target)->Result<ObjectPrefix,TransferError>{ObjectPrefix::new(row.prefix.as_deref().ok_or_else(||error("publication prefix is not reserved"))?).map_err(error)}
fn precommit_guard(backend:&WorkerTransferBackend,spec:&PublicationSpec)->Result<worker::D1PreparedStatement,TransferError>{statement(&backend.db,"INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM transfer_sessions WHERE session_id=? AND plan_revision=? AND state IN ('publishing','paused'))",&[spec.session_id.0.as_str().into(),spec.plan_revision.as_str().into()])}
pub(super) fn namespace_bounds(prefix:&str)->Result<[JsValue;3],TransferError>{
    let canonical=prefix.trim_end_matches('/');let mut ancestors=Vec::new();let mut bytes=0usize;
    for end in canonical.match_indices('/').map(|(offset,_)|offset).chain(std::iter::once(canonical.len())){let ancestor=&canonical[..end];bytes=bytes.checked_add(ancestor.len().saturating_add(3)).filter(|n|*n<=1_900_000).ok_or_else(||invalid("backend_metadata_limit","namespace ancestry exceeds D1 control budget"))?;ancestors.push(ancestor);}
    let ancestors=serde_json::to_string(&ancestors).map_err(error)?;if ancestors.len()>1_900_000{return Err(invalid("backend_metadata_limit","namespace ancestry exceeds D1 control budget"));}
    // '/' is immediately before '0': this exact separator-only upper bound
    // includes even a descendant beginning with the highest Unicode scalar.
    Ok([ancestors.into(),format!("{canonical}/").into(),format!("{canonical}0").into()])
}
fn namespace_values(spec:&PublicationSpec,row:&Target,prefix:&str)->Result<[JsValue;8],TransferError>{
    let id=row.assigned_id.ok_or_else(||error("namespace reservation has no id"))?;let [ancestors,lower,upper]=namespace_bounds(prefix)?;
    Ok([prefix.into(),(id as f64).into(),row.reservation_generation.map(|v|(v as f64).into()).unwrap_or(JsValue::NULL),narou_rs::db::novel_codec::fold(&spec.toc_url).into(),owner(spec).into(),ancestors,lower,upper])
}
fn namespace_guard(backend:&WorkerTransferBackend,values:&[JsValue;8])->Result<worker::D1PreparedStatement,TransferError>{
    statement(&backend.db,"INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM novel_namespace_claims c JOIN novel_access_gates g ON g.prefix=c.prefix WHERE c.prefix=?1 AND c.novel_id=?2 AND c.reservation_generation IS ?3 AND c.toc_url_fold=?4 AND c.owner=?5 AND c.job_id IS NULL AND (g.origin_url_fold=c.toc_url_fold OR (g.origin_url_fold='' AND c.empty_owned=1)) AND (?3 IS NULL OR EXISTS(SELECT 1 FROM novel_id_reservations i WHERE i.id=?2 AND i.generation=?3 AND i.available=0 AND i.job_id IS NULL))) OR EXISTS(SELECT 1 FROM novel_namespace_claims o WHERE (o.prefix IN(SELECT value FROM json_each(?6)) OR (o.prefix>=?7 AND o.prefix<?8)) AND NOT(o.prefix=?1 AND o.novel_id=?2 AND o.reservation_generation IS ?3 AND o.toc_url_fold=?4 AND o.owner=?5 AND o.job_id IS NULL)) OR EXISTS(SELECT 1 FROM novel_gate_bindings b JOIN novels n ON n.id=b.novel_id WHERE (b.prefix IN(SELECT value FROM json_each(?6)) OR (b.prefix>=?7 AND b.prefix<?8)) AND NOT(b.prefix=?1 AND b.novel_id=?2 AND n.toc_url_fold=?4)) OR EXISTS(SELECT 1 FROM novel_access_gates o WHERE (o.prefix IN(SELECT value FROM json_each(?6)) OR (o.prefix>=?7 AND o.prefix<?8)) AND o.prefix!=?1 AND (o.closed!=0 OR o.origin_url_fold!=''))",values)
}
fn release_namespace(backend:&WorkerTransferBackend,spec:&PublicationSpec,row:&Target)->Result<worker::D1PreparedStatement,TransferError>{
    statement(&backend.db,"DELETE FROM novel_namespace_claims WHERE owner=?1 AND novel_id=?2 AND reservation_generation IS ?3 AND prefix IS ?4 AND EXISTS(SELECT 1 FROM transfer_targets t WHERE t.session_id=?5 AND t.publication_id=?6 AND t.assigned_id=novel_namespace_claims.novel_id AND t.reservation_generation IS novel_namespace_claims.reservation_generation AND t.prefix=novel_namespace_claims.prefix AND (t.committed=1 OR t.rolled_back=1))",&[owner(spec).into(),row.assigned_id.map(|v|(v as f64).into()).unwrap_or(JsValue::NULL),row.reservation_generation.map(|v|(v as f64).into()).unwrap_or(JsValue::NULL),row.prefix.as_deref().map(JsValue::from).unwrap_or(JsValue::NULL),spec.session_id.0.as_str().into(),(spec.publication_id as f64).into()])
}
fn numeric_guard(backend:&WorkerTransferBackend,row:&Target)->Result<worker::D1PreparedStatement,TransferError>{
    statement(&backend.db,"INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE EXISTS(SELECT 1 FROM worker_jobs j WHERE json_valid(j.checkpoint_json) AND CAST(json_extract(j.checkpoint_json,'$.creation_reservation[0]') AS INTEGER)=?1 AND CAST(json_extract(j.checkpoint_json,'$.creation_reservation[1]') AS INTEGER)=?2 AND json_extract(j.checkpoint_json,'$.job_id')=j.job_id) OR EXISTS(SELECT 1 FROM novel_id_reservations i WHERE i.id=?1 AND i.generation=?2 AND i.job_id IS NOT NULL)",&[row.assigned_id.map(|v|(v as f64).into()).unwrap_or(JsValue::NULL),row.reservation_generation.map(|v|(v as f64).into()).unwrap_or(JsValue::NULL)])
}
fn done(id:Option<i64>)->PublicationStep{PublicationStep{done:true,assigned_id:id,..Default::default()}}
fn stale(message:impl Into<String>)->TransferError{TransferError::PlanStale{message:message.into()}}
#[derive(Deserialize)]
struct NamespaceOrigin{epoch:u64,content_revision:u64,closed:i64,origin_url_fold:String}
async fn probe_namespace_origin(backend:&WorkerTransferBackend,spec:&PublicationSpec,prefix:&str,folded:&str)->Result<NamespaceOrigin,TransferError>{
    let results=backend.db.batch(vec![backend.journal.execution_guard(&spec.session_id)?,precommit_guard(backend,spec)?,statement(&backend.db,"INSERT OR IGNORE INTO novel_access_gates(prefix,updated_at) VALUES(?,unixepoch())",&[prefix.into()])?]).await.map_err(error)?;crate::d1_repository::ensure_batch_success(&results).map_err(error)?;
    let origin=statement(&backend.db,"SELECT epoch,content_revision,closed,origin_url_fold FROM novel_access_gates WHERE prefix=?",&[prefix.into()])?.first::<NamespaceOrigin>(None).await.map_err(error)?.ok_or_else(||error("namespace gate disappeared"))?;
    if origin.closed!=0{return Err(TransferError::Busy{code:"namespace_busy",message:"destination namespace is recovering".into()});}
    if !origin.origin_url_fold.is_empty()&&origin.origin_url_fold!=folded{return Err(stale("retained namespace belongs to another URL; full removal is required"));}
    let mut occupied=false;
    for logical in [prefix.to_string(),format!("{prefix}/挿絵")]{let page=backend.objects.list_page(&ObjectListRequest::new(ObjectPrefix::new(logical).map_err(error)?,NonZeroUsize::new(1).unwrap())).await.map_err(error)?;occupied|=!page.objects.is_empty();}
    if origin.origin_url_fold.is_empty()&&spec.target.action==PlanAction::Create&&occupied{return Err(stale("unowned destination contains retained data"));}
    Ok(origin)
}
fn origin_admission(backend:&WorkerTransferBackend,prefix:&str,origin:&NamespaceOrigin)->Result<worker::D1PreparedStatement,TransferError>{
    statement(&backend.db,"INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM novel_access_gates g WHERE g.prefix=? AND g.epoch=? AND g.content_revision=? AND g.origin_url_fold=? AND g.closed=0)",&[prefix.into(),(origin.epoch as f64).into(),(origin.content_revision as f64).into(),origin.origin_url_fold.as_str().into()])
}

async fn check_plan(backend:&WorkerTransferBackend,spec:&PublicationSpec,row:&Target)->Result<(),TransferError>{
    let current=backend.repository.find_by_toc_url(&spec.toc_url).await.map_err(error)?;
    match(spec.target.action,current){
        (PlanAction::Create,None)=>{},
        (PlanAction::Overwrite,Some(live)) if Some(live.id)==spec.target.existing_id=>{
            let(rev,content)=backend.revision(live.id).await?;
            if spec.target.record_revision.as_deref()!=Some(&rev)||spec.target.content_revision.as_deref()!=Some(&content){return Err(stale("destination record or content changed after planning"));}
            if virtual_novel::is_virtual_record(&live)!=(spec.kind==NovelKind::VirtualParent){return Err(stale("destination kind changed"));}
            if let Some(expected)=spec.target.membership_digest.as_deref(){if backend.membership(live.id).await?!=expected{return Err(stale("parent membership changed after planning"));}}
        },
        _=>return Err(stale("destination identity changed after planning")),
    }
    if row.rolled_back!=0{return Err(stale("publication has already rolled back"));}Ok(())
}

async fn publication_file(backend:&WorkerTransferBackend,spec:&PublicationSpec,index:u32)->Result<PublicationFile,TransferError>{
    if index>=spec.file_count{return Err(invalid("invalid_file","publication file index is out of range"));}
    let id=spec.file_id_start.checked_add(index).ok_or_else(||invalid("invalid_file","publication file ordinal overflow"))?;
    let file=backend.journal.file(&spec.session_id,id).await?.ok_or_else(||invalid("stage_missing","publication file row is missing"))?;
    if file.novel_ordinal!=spec.publication_id||!matches!(file.state,FileState::Verified|FileState::Reused){return Err(invalid("files_incomplete","publication file is not independently verified"));}
    Ok(PublicationFile{logical_path:file.logical_path,size:file.size,sha256:file.sha256,stage:file.stage,pin:file.pin})
}

async fn incoming_toc(backend:&WorkerTransferBackend,spec:&PublicationSpec)->Result<Option<TocMetadata>,TransferError>{
    let file_id=statement(&backend.db,"SELECT file_id FROM transfer_files WHERE session_id=? AND novel_ordinal=? AND logical_path='toc.yaml'",&binds(spec))?.first::<u32>(Some("file_id")).await.map_err(error)?;
    let Some(file_id)=file_id else{return Ok(None)};
    let index=file_id.checked_sub(spec.file_id_start).ok_or_else(||invalid("invalid_file","toc ordinal escaped publication"))?;
    let file=publication_file(backend,spec,index).await?;
    if file.size>16*1024*1024{return Err(TransferError::ExceedsLimit{code:"metadata_budget",message:"toc.yaml exceeds the storage metadata budget".into()});}
    let handle=file.stage.as_ref().or(file.pin.as_ref()).ok_or_else(||invalid("stage_missing","toc has no retained bytes"))?;let bytes=backend.metadata_bytes(handle,file.size).await?;
    match narou_rs::downloader::persistence::deserialize_toc_metadata(&bytes){Ok(metadata)=>Ok(Some(metadata)),Err(error) if error.to_string().contains("transfer_metadata_budget:")=>Err(TransferError::ExceedsLimit{code:"metadata_budget",message:error.to_string()}),Err(_)=>Ok(None)}
}
fn host(url:&str)->Option<String>{worker::Url::parse(url).ok().and_then(|u|u.host_str().map(str::to_string))}
fn extract_ncode(url:&str)->Option<String>{url.split(|c:char|!c.is_ascii_alphanumeric()).find_map(|part|{let b=part.as_bytes();if b.first().is_none_or(|v|!v.eq_ignore_ascii_case(&b'n')){return None;}let n=b[1..].iter().take_while(|v|v.is_ascii_digit()).count();if n==0||n+1==b.len()||!b[n+1..].iter().all(u8::is_ascii_alphabetic){None}else{Some(part.to_ascii_lowercase())}})}
async fn build_record(backend:&WorkerTransferBackend,spec:&PublicationSpec,id:i64)->Result<NovelRecord,TransferError>{
    let metadata=&spec.metadata;let parent=spec.kind==NovelKind::VirtualParent;
    let toc=if parent{None}else{incoming_toc(backend,spec).await?};
    let live=if spec.target.action==PlanAction::Overwrite{backend.repository.get(NovelId(id)).await.map_err(error)?}else{None};
    let setting=backend.sites.iter().find(|s|s.matches_url(&spec.toc_url));
    let title=if metadata.title.trim().is_empty(){toc.as_ref().and_then(|v|v.title.clone()).unwrap_or_default()}else{metadata.title.clone()};
    if title.trim().is_empty(){return Err(invalid("invalid_metadata","novel title is empty"));}
    let author=if metadata.author.trim().is_empty(){toc.as_ref().and_then(|v|v.author.clone()).unwrap_or_default()}else{metadata.author.clone()};
    let ncode=if parent{Some(virtual_novel::ncode(id))}else{metadata.ncode.clone().or_else(||extract_ncode(&spec.toc_url))};
    let file_title=if parent{virtual_novel::ncode(id)}else{let limit=load_length_limit("folder-length-limit",Some(50));match ncode.as_deref(){Some(n)if !setting.is_some_and(|s|s.append_title_to_folder_name)=>n.to_string(),Some(n)=>sanitize_filename_with_limit(&format!("{n} {title}"),limit),None=>sanitize_filename_with_limit(&title,limit)}};
    let local=statement(&backend.db,"SELECT value_json FROM app_state WHERE scope='local' AND key='download.use-subdirectory'",&[])?.first::<String>(Some("value_json")).await.map_err(error)?;
    let use_subdirectory=local.and_then(|v|serde_json::from_str::<bool>(&v).ok()).unwrap_or(false);
    let now=Utc::now();let lastup=metadata.general_lastup.as_deref().and_then(|s|chrono::DateTime::parse_from_rfc3339(s).ok()).map(|v|v.with_timezone(&Utc));
    let site=if metadata.sitename.trim().is_empty(){setting.map(|s|s.sitename.clone()).or_else(||host(&spec.toc_url)).unwrap_or_else(||"narou.rs".into())}else{metadata.sitename.clone()};
    let mut new=live.unwrap_or_else(||NovelRecord{id,author:author.clone(),title:title.clone(),file_title,toc_url:spec.toc_url.clone(),sitename:site,novel_type:metadata.novel_type,end:parent,last_update:now,new_arrivals_date:None,use_subdirectory,general_firstup:None,novelupdated_at:None,general_lastup:lastup,last_mail_date:None,tags:vec![],ncode:ncode.clone(),domain:metadata.domain.clone().or_else(||host(&spec.toc_url)),general_all_no:toc.as_ref().map(|t|t.chapter_count as i64).filter(|n|*n>0),length:None,suspend:false,is_narou:setting.is_some_and(|s|s.is_narou),last_check_date:None,convert_failure:false,requires_login:false,login_session:None,extra_fields:Default::default()});
    new.title=title;new.author=author;if new.ncode.is_none(){new.ncode=ncode;}if new.domain.is_none(){new.domain=metadata.domain.clone();}if new.general_lastup.is_none(){new.general_lastup=lastup;}
    new.tags.extend(metadata.tags.iter().filter(|t|!virtual_novel::is_internal_tag(t)).cloned());narou_rs::db::novel_codec::dedup_tags(&mut new.tags);
    if let Some(attribute)=metadata.attribute_tag.as_deref(){new.set_attribute_tag(attribute);}
    narou_rs::application::attribute_tags::initialize_record(&mut new,&backend.sites);
    Ok(new)
}
async fn ensure_target(backend:&WorkerTransferBackend,spec:&PublicationSpec)->Result<Target,TransferError>{
    let results=backend.db.batch(vec![backend.journal.execution_guard(&spec.session_id)?,precommit_guard(backend,spec)?,statement(&backend.db,"INSERT OR IGNORE INTO transfer_targets(session_id,publication_id,assigned_id) VALUES(?,?,?)",&[spec.session_id.0.as_str().into(),(spec.publication_id as f64).into(),spec.target.existing_id.map(|id|(id as f64).into()).unwrap_or(JsValue::NULL)])?]).await.map_err(error)?;crate::d1_repository::ensure_batch_success(&results).map_err(error)?;
    let mut row=target(backend,spec).await?;
    if row.rolled_back!=0 {
        let results=backend.db.batch(vec![backend.journal.execution_guard(&spec.session_id)?,precommit_guard(backend,spec)?,statement(&backend.db,"UPDATE transfer_targets SET assigned_id=?,reservation_generation=NULL,prefix=NULL,epoch=0,record_json=NULL,summary_json=NULL,rolled_back=0,receipt=NULL WHERE session_id=? AND publication_id=? AND committed=0 AND rolled_back=1",&[spec.target.existing_id.map(|id|(id as f64).into()).unwrap_or(JsValue::NULL),spec.session_id.0.as_str().into(),(spec.publication_id as f64).into()])?]).await.map_err(error)?;crate::d1_repository::ensure_batch_success(&results).map_err(error)?;
        row=target(backend,spec).await?;
    }
    if row.assigned_id.is_none(){
        const OWNER_FILTER:&str="AND NOT EXISTS(SELECT 1 FROM transfer_targets WHERE session_id=?1 AND publication_id=?2 AND assigned_id IS NOT NULL) ";
        let mut sql=String::with_capacity(narou_rs::db::novel_query::ALLOCATE_STEP_SQL.len()+OWNER_FILTER.len()+16);sql.push_str(narou_rs::db::novel_query::ALLOCATE_STEP_SQL);
        // The numeric reservation is adopted atomically; reuse drops its OLD
        // job association, never the new generation's authoritative owner.
        let insert=sql.find("ON CONFLICT").expect("shared allocator has ON CONFLICT");sql.insert_str(insert,OWNER_FILTER);
        let assign=sql.rfind("available = 0").expect("shared allocator assigns available");sql.insert_str(assign+"available = 0".len(),", job_id = NULL");
        let allocate=statement(&backend.db,&sql,&binds(spec))?;
        let adopt=statement(&backend.db,"UPDATE transfer_targets SET assigned_id=(SELECT id FROM novel_id_reservations ORDER BY generation DESC LIMIT 1),reservation_generation=(SELECT generation FROM novel_id_reservations ORDER BY generation DESC LIMIT 1) WHERE session_id=? AND publication_id=? AND assigned_id IS NULL",&binds(spec))?;
        let results=backend.db.batch(vec![backend.journal.execution_guard(&spec.session_id)?,precommit_guard(backend,spec)?,allocate,backend.db.prepare(narou_rs::db::novel_query::ALLOCATE_BUMP_SQL),adopt]).await.map_err(error)?;crate::d1_repository::ensure_batch_success(&results).map_err(error)?;row=target(backend,spec).await?;
    }
    if row.record_json.is_none(){let id=row.assigned_id.ok_or_else(||error("target id allocation failed"))?;let record=build_record(backend,spec,id).await?;let serialized=serde_json::to_string(&record).map_err(error)?;
        if serialized.len()>1_900_000{return Err(TransferError::ExceedsLimit{code:"backend_metadata_limit",message:"novel record exceeds the D1 row budget".into()});}
        let key=keys(&record)?;let prefix=key.prefix().as_ref();let folded=narou_rs::db::novel_codec::fold(&spec.toc_url);
        let claim_values=namespace_values(spec,&row,prefix)?;
        let collision=statement(&backend.db,"SELECT 1 collision FROM novel_namespace_claims WHERE (prefix IN(SELECT value FROM json_each(?6)) OR (prefix>=?7 AND prefix<?8) OR novel_id=?2 OR toc_url_fold=?4) AND NOT(prefix=?1 AND novel_id=?2 AND reservation_generation IS ?3 AND toc_url_fold=?4 AND owner=?5 AND job_id IS NULL) LIMIT 1",&claim_values)?.first::<i64>(Some("collision")).await.map_err(error)?.is_some();
        if collision{return Err(stale("destination namespace is reserved by another mutation"));}
        let origin=probe_namespace_origin(backend,spec,prefix,&folded).await?;
        let mut statements=vec![backend.journal.execution_guard(&spec.session_id)?,precommit_guard(backend,spec)?,numeric_guard(backend,&row)?,origin_admission(backend,prefix,&origin)?];
        if spec.target.action==PlanAction::Overwrite{statements.push(statement(&backend.db,"UPDATE novel_access_gates SET origin_url_fold=?4 WHERE prefix=?1 AND origin_url_fold='' AND ?3 IS NULL AND EXISTS(SELECT 1 FROM novels n JOIN novel_gate_bindings b ON b.novel_id=n.id WHERE n.id=?2 AND n.toc_url_fold=?4 AND b.prefix=?1)",&claim_values[..4])?);}
        statements.push(statement(&backend.db,"INSERT OR IGNORE INTO novel_namespace_claims(prefix,novel_id,reservation_generation,toc_url_fold,owner,epoch,empty_owned) SELECT ?1,?2,?3,?4,?5,g.epoch,CASE WHEN g.origin_url_fold='' AND ?3 IS NOT NULL THEN 1 ELSE 0 END FROM novel_access_gates g WHERE g.prefix=?1 AND g.closed=0 AND g.origin_url_fold IN ('',?4)",&claim_values[..5])?);
        statements.push(namespace_guard(backend,&claim_values)?);
        statements.push(statement(&backend.db,"UPDATE transfer_targets SET prefix=?,record_json=? WHERE session_id=? AND publication_id=? AND assigned_id=? AND reservation_generation IS ? AND record_json IS NULL AND committed=0 AND rolled_back=0",&[prefix.into(),serialized.into(),spec.session_id.0.as_str().into(),(spec.publication_id as f64).into(),(id as f64).into(),row.reservation_generation.map(|v|(v as f64).into()).unwrap_or(JsValue::NULL)])?);
        statements.push(backend.db.prepare("INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE changes()=0"));let results=backend.db.batch(statements).await.map_err(error)?;
        crate::d1_repository::ensure_batch_success(&results).map_err(error)?;row=target(backend,spec).await?;
    }Ok(row)
}
pub(super) async fn namespace_index_ready(backend:&WorkerTransferBackend,session:&SessionId)->Result<bool,TransferError>{
    let input=serde_json::to_string(backend.journal.execution_owner()).map_err(error)?;
    let step=backend.begin_backend_step(session,"namespace_index",&input).await?;
    if let Some(result)=step.result_json{return serde_json::from_str(&result).map_err(error);}
    let complete=statement(&backend.db,"SELECT 1 ready FROM transfer_journal_entries WHERE session_id=? AND publication_id=4294967295 AND key='namespace:index:ready' AND value='AQ=='",&[session.0.as_str().into()])?.first::<i64>(Some("ready")).await.map_err(error)?.is_some();
    // Initialization and target admission are distinct public units. Even its
    // final page records pending; an old opcode cannot perform NEXT work.
    let mut initialized=false;
    let admit=if step.input_json!=input{false}else if complete{true}else{initialized=backend.repository.bootstrap_record_origins().await.map_err(error)?;false};
    let mut statements=vec![backend.journal.execution_guard(session)?];
    if initialized{statements.push(statement(&backend.db,"INSERT OR IGNORE INTO transfer_journal_entries(session_id,publication_id,key,value) VALUES(?,4294967295,'namespace:index:ready','AQ==')",&[session.0.as_str().into()])?);statements.push(statement(&backend.db,"INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM transfer_journal_entries WHERE session_id=? AND publication_id=4294967295 AND key='namespace:index:ready' AND value='AQ==')",&[session.0.as_str().into()])?);}
    statements.push(backend.finish_backend_step(session,"namespace_index",if admit{"true"}else{"false"})?);
    let results=backend.db.batch(statements).await.map_err(error)?;crate::d1_repository::ensure_batch_success(&results).map_err(error)?;Ok(admit)
}
pub(super) async fn gate_close(backend:&WorkerTransferBackend,spec:&PublicationSpec,_cursor:Option<&str>)->Result<PublicationStep,TransferError>{
    if !namespace_index_ready(backend,&spec.session_id).await?{return Ok(PublicationStep{done:false,..Default::default()});}
    let row=ensure_target(backend,spec).await?;if row.committed!=0{return Ok(done(row.assigned_id));}let prefix=prefix(&row)?;let owner=owner(spec);
    let namespace=namespace_values(spec,&row,prefix.as_ref())?;
    let results=backend.db.batch(vec![backend.journal.execution_guard(&spec.session_id)?,precommit_guard(backend,spec)?,numeric_guard(backend,&row)?,namespace_guard(backend,&namespace)?]).await.map_err(error)?;crate::d1_repository::ensure_batch_success(&results).map_err(error)?;
    check_plan(backend,spec,&row).await?;
    let id=row.assigned_id.ok_or_else(||error("target id missing"))?;
    if spec.target.action==PlanAction::Overwrite{
        let live=backend.repository.get(NovelId(id)).await.map_err(error)?.ok_or_else(||stale("overwrite target disappeared"))?;
        if keys(&live)?.prefix().as_ref()!=prefix.as_ref(){return Err(stale("overwrite target namespace changed"));}
        backend.repository.bind_record_origin(&live).await.map_err(error)?;
    }else{
        #[derive(Deserialize)]struct PreviousBinding{prefix:String,epoch:u64,closed:i64}
        let previous=statement(&backend.db,"SELECT b.prefix,COALESCE(g.epoch,0) epoch,COALESCE(g.closed,0) closed FROM novel_gate_bindings b LEFT JOIN novel_access_gates g ON g.prefix=b.prefix WHERE b.novel_id=?",&[(id as f64).into()])?.first::<PreviousBinding>(None).await.map_err(error)?;
        let mut statements=vec![backend.journal.execution_guard(&spec.session_id)?,precommit_guard(backend,spec)?,numeric_guard(backend,&row)?,namespace_guard(backend,&namespace)?];
        if let Some(previous)=previous{if previous.prefix!=prefix.as_ref(){if previous.closed!=0{return Err(TransferError::Busy{code:"namespace_busy",message:"previous numeric generation namespace is recovering".into()});}
            statements.push(statement(&backend.db,"UPDATE novel_gate_bindings SET prefix=?1 WHERE novel_id=?2 AND prefix=?3 AND NOT EXISTS(SELECT 1 FROM novels WHERE id=?2) AND NOT EXISTS(SELECT 1 FROM novel_access_gates g WHERE g.prefix=?3 AND (g.epoch!=?4 OR g.closed!=0)) AND EXISTS(SELECT 1 FROM novel_id_reservations i WHERE i.id=?2 AND i.generation=?5 AND i.available=0 AND i.job_id IS NULL)",&[prefix.as_ref().into(),(id as f64).into(),previous.prefix.as_str().into(),(previous.epoch as f64).into(),row.reservation_generation.map(|v|(v as f64).into()).unwrap_or(JsValue::NULL)])?);
        }}
        statements.push(statement(&backend.db,"INSERT OR IGNORE INTO novel_gate_bindings(novel_id,prefix) SELECT ?,? WHERE NOT EXISTS(SELECT 1 FROM novels WHERE id=?)",&[(id as f64).into(),prefix.as_ref().into(),(id as f64).into()])?);
        statements.push(statement(&backend.db,"INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM novel_gate_bindings WHERE novel_id=? AND prefix=?)",&[(id as f64).into(),prefix.as_ref().into()])?);
        let results=backend.db.batch(statements).await.map_err(error)?;crate::d1_repository::ensure_batch_success(&results).map_err(error)?;
    }
    let epoch=backend.d1.close_gate(&prefix,&owner).await.map_err(error)?;
    run(&backend.db,"UPDATE transfer_targets SET epoch=? WHERE session_id=? AND publication_id=?",&[(epoch as f64).into(),spec.session_id.0.as_str().into(),(spec.publication_id as f64).into()]).await?;Ok(done(row.assigned_id))
}
pub(super) async fn drain(backend:&WorkerTransferBackend,spec:&PublicationSpec,_cursor:Option<&str>)->Result<PublicationStep,TransferError>{let row=target(backend,spec).await?;if row.committed!=0{return Ok(done(row.assigned_id));}let prefix=prefix(&row)?;backend.d1.renew_gate(&prefix,&owner(spec),row.epoch).await.map_err(error)?;let gate=backend.d1.gate_info(&prefix).await.map_err(error)?;Ok(PublicationStep{done:gate.pending_readers==0,assigned_id:row.assigned_id,..Default::default()})}
pub(super) async fn prepare(backend:&WorkerTransferBackend,spec:&PublicationSpec,_cursor:Option<&str>)->Result<PublicationStep,TransferError>{
    let row=target(backend,spec).await?;if row.committed!=0{return Ok(done(row.assigned_id));}check_plan(backend,spec,&row).await?;
    if spec.kind==NovelKind::VirtualParent{
        if spec.member_ids.is_empty()||spec.member_ids.len()!=spec.member_count as usize{return Err(stale("parent members have not all been resolved"));}
        let kind=format!("prepare:{}",spec.publication_id);
        let input=row.summary_json.as_deref().unwrap_or("null");
        let step=backend.begin_backend_step(&spec.session_id,&kind,input).await?;
        if let Some(result)=step.result_json{let next:usize=serde_json::from_str(&result).map_err(error)?;return Ok(PublicationStep{done:next>=spec.member_ids.len(),cursor:(next<spec.member_ids.len()).then(||next.to_string()),assigned_id:row.assigned_id,..Default::default()});}
        if step.input_json!=input{return Err(TransferError::RecoveryRequired{message:"parent summary advanced without operation receipt".into()});}
        let mut summary:ParentSummary=row.summary_json.as_deref().map(serde_json::from_str).transpose().map_err(error)?.unwrap_or_default();
        #[derive(Deserialize)]struct Member{position:usize,id:Option<i64>,tags_json:Option<String>,attribute_tag:Option<String>,general_all_no:Option<i64>,general_lastup:Option<String>,ncode:Option<String>}
        let ids=serde_json::to_string(&spec.member_ids).map_err(error)?;
        let members=statement(&backend.db,"SELECT CAST(j.key AS INTEGER) position,n.id,n.tags_json,n.attribute_tag,n.general_all_no,n.general_lastup,n.ncode FROM json_each(?) j LEFT JOIN novels n ON n.id=j.value WHERE CAST(j.key AS INTEGER)>=? ORDER BY CAST(j.key AS INTEGER) LIMIT 100",&[ids.into(),(summary.next as f64).into()])?.all().await.map_err(error)?.results::<Member>().map_err(error)?;
        for member in members{if member.position!=summary.next||member.id!=spec.member_ids.get(summary.next).copied()||member.ncode.as_deref().is_some_and(virtual_novel::is_virtual_ncode){return Err(stale("parent member is missing or has wrong kind"));}
            let tags:Vec<String>=serde_json::from_str(member.tags_json.as_deref().ok_or_else(||stale("parent member tags are missing"))?).map_err(error)?;summary.tags.extend(tags.into_iter().filter(|t|!virtual_novel::is_internal_tag(t)&&!narou_rs::application::attribute_tags::is_attribute_tag(t)));narou_rs::db::novel_codec::dedup_tags(&mut summary.tags);
            summary.count=summary.count.checked_add(member.general_all_no.unwrap_or(0)).ok_or_else(||invalid("metadata_overflow","member episode count overflow"))?;
            if member.attribute_tag.as_deref()==Some("漫画"){summary.manga=true;}else{summary.novel=true;}
            if let Some(date)=member.general_lastup.as_deref().and_then(|v|chrono::DateTime::parse_from_rfc3339(v).ok()).map(|v|v.with_timezone(&Utc)){if summary.latest.is_none_or(|old|date>old){summary.latest=Some(date);}}summary.next+=1;
        }
        let encoded=serde_json::to_string(&summary).map_err(error)?;if encoded.len()>1_900_000{return Err(TransferError::ExceedsLimit{code:"backend_metadata_limit",message:"parent metadata exceeds D1 budget".into()});}
        let mut statements=vec![backend.journal.execution_guard(&spec.session_id)?,statement(&backend.db,"UPDATE transfer_targets SET summary_json=? WHERE session_id=? AND publication_id=?",&[encoded.into(),spec.session_id.0.as_str().into(),(spec.publication_id as f64).into()])?];
        if summary.next>=spec.member_ids.len(){
            let mut record=record(&row)?;record.tags.extend(summary.tags);record.tags.retain(|t|!virtual_novel::is_internal_tag(t));record.tags.push(virtual_novel::tag(record.id));narou_rs::db::novel_codec::dedup_tags(&mut record.tags);record.general_all_no=(summary.count>0).then_some(summary.count);if summary.latest.is_some(){record.general_lastup=summary.latest;}
            if summary.manga&&!summary.novel{record.set_attribute_tag("漫画");}else if summary.novel&&!summary.manga{record.set_attribute_tag("小説");}
            let encoded=serde_json::to_string(&record).map_err(error)?;if encoded.len()>1_900_000{return Err(invalid("backend_metadata_limit","prepared parent record exceeds D1 row budget"));}
            statements.push(statement(&backend.db,"UPDATE transfer_targets SET record_json=? WHERE session_id=? AND publication_id=?",&[encoded.into(),spec.session_id.0.as_str().into(),(spec.publication_id as f64).into()])?);
        }
        statements.push(backend.finish_backend_step(&spec.session_id,&kind,&summary.next.to_string())?);
        let result=backend.db.batch(statements).await.map_err(error)?;crate::d1_repository::ensure_batch_success(&result).map_err(error)?;
        if summary.next<spec.member_ids.len(){return Ok(PublicationStep{done:false,cursor:Some(summary.next.to_string()),assigned_id:row.assigned_id,..Default::default()});}
    }Ok(done(row.assigned_id))
}
pub(super) async fn install(backend:&WorkerTransferBackend,spec:&PublicationSpec,cursor:Option<&str>)->Result<PublicationStep,TransferError>{
    let row=target(backend,spec).await?;if row.committed!=0{return Ok(done(row.assigned_id));}let record=record(&row)?;let prefix=prefix(&row)?;backend.d1.renew_gate(&prefix,&owner(spec),row.epoch).await.map_err(error)?;
    let start=cursor.map(|v|v.parse::<usize>()).transpose().map_err(error)?.unwrap_or(0);let end=(start+100).min(spec.file_count as usize);let mut statements=Vec::new();
    for index in start..end{
        let file=publication_file(backend,spec,index as u32).await?;
        let role=projection::classify(&file.logical_path,spec.kind==NovelKind::VirtualParent,&ClassifyContext::default());
        if matches!(role,FileRole::Raw|FileRole::Internal){return Err(invalid("file_policy","Worker does not persist raw or internal files"));}
        if spec.kind==NovelKind::VirtualParent&&!narou_rs::transfer::is_virtual_input_file(&file.logical_path){return Err(invalid("invalid_parent_file","virtual parent contains a body file"));}
        let handle=file.stage.as_ref().or(file.pin.as_ref()).ok_or_else(||invalid("stage_missing","publication file is not retained"))?;
        #[derive(Deserialize)]struct Asset{blob_key:String,size:u64,sha256:String,backend:String}
        let asset=if handle.starts_with("pin_"){statement(&backend.db,"SELECT stage blob_key,size,sha256,backend FROM transfer_pins WHERE pin=? AND session_id=?",&[handle.as_str().into(),spec.session_id.0.as_str().into()])?.first::<Asset>(None).await.map_err(error)?}
        else{statement(&backend.db,"SELECT sealed_key blob_key,expected_size size,sha256,backend FROM transfer_stage_owners WHERE stage_id=? AND session_id=? AND sealed_key IS NOT NULL",&[handle.as_str().into(),spec.session_id.0.as_str().into()])?.first::<Asset>(None).await.map_err(error)?}.ok_or_else(||invalid("stage_missing","verified stage no longer exists"))?;
        if asset.size!=file.size||asset.sha256!=file.sha256{return Err(invalid("stage_identity_mismatch","publication identity differs from sealed bytes"));}
        if role==FileRole::Illustration{narou_rs::platform::transfer_stage::check_hash_named_key(&key_for(&record,&file.logical_path)?,&file.sha256).map_err(error)?;}
        let key=key_for(&record,&file.logical_path)?;statements.push(statement(&backend.db,"INSERT OR IGNORE INTO transfer_publication_files(session_id,publication_id,file_index,object_key,blob_key,size,sha256,backend) VALUES(?,?,?,?,?,?,?,?)",&[spec.session_id.0.as_str().into(),(spec.publication_id as f64).into(),(index as f64).into(),key.as_ref().into(),asset.blob_key.into(),(asset.size as f64).into(),asset.sha256.into(),asset.backend.into()])?);
    }
    if !statements.is_empty(){let results=backend.db.batch(statements).await.map_err(error)?;crate::d1_repository::ensure_batch_success(&results).map_err(error)?;}
    Ok(PublicationStep{done:end==spec.file_count as usize,cursor:(end<spec.file_count as usize).then(||end.to_string()),assigned_id:row.assigned_id,..Default::default()})
}
pub(super) async fn commit(backend:&WorkerTransferBackend,spec:&PublicationSpec,_cursor:Option<&str>)->Result<PublicationStep,TransferError>{
    let row=target(backend,spec).await?;if row.committed!=0{return Ok(PublicationStep{done:true,committed:true,assigned_id:row.assigned_id,receipt:row.receipt.map(String::into_bytes),..Default::default()});}
    check_plan(backend,spec,&row).await?;let record=record(&row)?;let prefix=prefix(&row)?;
    let receipt=PublicationReceipt{novel_ordinal:spec.publication_id,action:spec.target.action,outcome:if spec.target.action==PlanAction::Create{"created"}else{"overwritten"}.into(),assigned_id:Some(record.id),files_installed:spec.file_count as usize as u32,error:None};let receipt=serde_json::to_string(&receipt).map_err(error)?;
    let namespace=namespace_values(spec,&row,prefix.as_ref())?;
    for _ in 0..8{
        let token=backend.opaque_id("guard");let mut statements=vec![backend.journal.execution_guard(&spec.session_id)?];statements.extend(backend.d1.publication_guard(&prefix,&owner(spec),row.epoch,&token).map_err(error)?);
        statements.push(statement(&backend.db,"INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM transfer_sessions WHERE session_id=? AND plan_revision=? AND state IN ('publishing','paused','cancelling')) OR NOT EXISTS(SELECT 1 FROM novel_gate_bindings WHERE novel_id=? AND prefix=?)",&[spec.session_id.0.as_str().into(),spec.plan_revision.as_str().into(),(record.id as f64).into(),prefix.as_ref().into()])?);
        statements.push(namespace_guard(backend,&namespace)?);
        statements.push(numeric_guard(backend,&row)?);
        statements.push(statement(&backend.db,"UPDATE novel_access_gates SET origin_url_fold=? WHERE prefix=? AND origin_url_fold IN ('',?1)",&[narou_rs::db::novel_codec::fold(&spec.toc_url).into(),prefix.as_ref().into()])?);
        statements.push(backend.db.prepare("INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE changes()=0"));
        // Recheck the same record and content revisions in the commit batch.
        if spec.target.action==PlanAction::Create{statements.push(statement(&backend.db,"INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE EXISTS(SELECT 1 FROM novels WHERE id=? OR toc_url_fold=?)",&[(record.id as f64).into(),narou_rs::db::novel_codec::fold(&spec.toc_url).into()])?);}
        else{statements.push(statement(&backend.db,"INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM novels n LEFT JOIN novel_revisions r ON r.novel_id=n.id JOIN novel_access_gates g ON g.prefix=? WHERE n.id=? AND n.toc_url_fold=? AND CAST(COALESCE(r.record_revision,0) AS TEXT)=? AND CAST(COALESCE(r.content_revision,0) AS TEXT)||':'||CAST(g.content_revision AS TEXT)=?)",&[prefix.as_ref().into(),(record.id as f64).into(),narou_rs::db::novel_codec::fold(&spec.toc_url).into(),spec.target.record_revision.as_deref().unwrap_or("").into(),spec.target.content_revision.as_deref().unwrap_or("").into()])?);}
        statements.push(statement(&backend.db,"INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE (SELECT COUNT(*) FROM transfer_publication_files WHERE session_id=? AND publication_id=?)!=?",&[spec.session_id.0.as_str().into(),(spec.publication_id as f64).into(),(spec.file_count as f64).into()])?);
        let membership_raw=if spec.kind==NovelKind::VirtualParent{statement(&backend.db,"SELECT value_json FROM app_state WHERE scope='inv' AND key='virtual_novel_members'",&[])?.first::<String>(Some("value_json")).await.map_err(error)?}else{None};
        let membership_new=if spec.kind==NovelKind::VirtualParent{
            let mut map:virtual_novel::VirtualNovelMembers=membership_raw.as_deref().map(serde_json::from_str).transpose().map_err(error)?.unwrap_or_default();
            let before=map.get(record.id).unwrap_or_default().iter().map(i64::to_string).collect::<Vec<_>>();
            if let Some(expected)=spec.target.membership_digest.as_deref(){if narou_rs::platform::membership_digest(before.iter().map(String::as_str))!=expected{return Err(stale("target membership changed"));}}
            map.remove(record.id);map.insert(record.id,spec.member_ids.clone()).map_err(stale)?;Some(map)
        }else{None};
        if let Some(map)=membership_new.as_ref(){
            let members=serde_json::to_string(&spec.member_ids).map_err(error)?;statements.push(statement(&backend.db,"INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE EXISTS(SELECT 1 FROM json_each(?) m LEFT JOIN novels n ON n.id=m.value WHERE n.id IS NULL OR (n.ncode GLOB 'vs_[0-9]*' AND substr(n.ncode,4) NOT GLOB '*[^0-9]*'))",&[members.into()])?);
            statements.push(statement(&backend.db,"INSERT INTO app_state(scope,key,value_json,value_yaml) SELECT 'inv','virtual_novel_members',?,? WHERE ? IS NULL OR EXISTS(SELECT 1 FROM app_state WHERE scope='inv' AND key='virtual_novel_members' AND value_json=?) ON CONFLICT(scope,key) DO UPDATE SET value_json=excluded.value_json,value_yaml=excluded.value_yaml WHERE app_state.value_json=?",&[serde_json::to_string(map).map_err(error)?.into(),serde_yaml::to_string(map).map_err(error)?.into(),membership_raw.as_deref().map(JsValue::from).unwrap_or(JsValue::NULL),membership_raw.as_deref().map(JsValue::from).unwrap_or(JsValue::NULL),membership_raw.as_deref().map(JsValue::from).unwrap_or(JsValue::NULL)])?);
            statements.push(backend.db.prepare("INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE changes()=0"));
        }
        let snapshot=crate::tag_sequence::read_snapshot(&backend.db).await.map_err(error)?;let library=if snapshot.sequence.is_initialized(){None}else{Some(crate::tag_sequence::library_tags(&backend.db).await.map_err(error)?)};
        let written=record.tags.iter().filter(|t|!virtual_novel::is_internal_tag(t)).map(String::as_str).collect::<Vec<_>>();let next=crate::tag_sequence::advance(&snapshot,library.as_ref(),&written);
        statements.push(statement(&backend.db,"INSERT INTO transfer_object_refs(object_key,blob_key,size,sha256,backend,updated_at,blob_generation) SELECT f.object_key,f.blob_key,f.size,f.sha256,f.backend,?,CASE WHEN f.backend='d1' THEN (SELECT v.generation FROM transfer_verified_objects v JOIN objects o ON o.object_key=v.blob_key AND o.generation=v.generation WHERE v.blob_key=f.blob_key AND v.size=f.size AND v.sha256=f.sha256) ELSE '' END FROM transfer_publication_files f WHERE f.session_id=? AND f.publication_id=? AND EXISTS(SELECT 1 FROM transfer_storage_guards WHERE token=?) ON CONFLICT(object_key) DO UPDATE SET blob_key=excluded.blob_key,size=excluded.size,sha256=excluded.sha256,backend=excluded.backend,updated_at=excluded.updated_at,blob_generation=excluded.blob_generation",&[Utc::now().to_rfc3339().into(),spec.session_id.0.as_str().into(),(spec.publication_id as f64).into(),token.as_str().into()])?);
        crate::d1_repository::push_upsert_statements(&backend.db,&record,&mut statements).map_err(error)?;
        statements.push(crate::tag_sequence::upsert_statement(&backend.db,snapshot.raw_json.as_deref(),&next).map_err(error)?);statements.push(crate::tag_sequence::guard_statement(&backend.db,&next).map_err(error)?);
        statements.push(statement(&backend.db,"UPDATE transfer_targets SET committed=1,receipt=? WHERE session_id=? AND publication_id=? AND committed=0",&[receipt.as_str().into(),spec.session_id.0.as_str().into(),(spec.publication_id as f64).into()])?);
        statements.push(backend.db.prepare("INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE changes()=0"));
        use base64::Engine;let encoded=base64::engine::general_purpose::STANDARD.encode(receipt.as_bytes());
        statements.push(statement(&backend.db,"UPDATE transfer_publications SET state='committed',assigned_id=?,receipt=? WHERE session_id=? AND publication_id=?",&[(record.id as f64).into(),encoded.into(),spec.session_id.0.as_str().into(),(spec.publication_id as f64).into()])?);
        statements.push(release_namespace(backend,spec,&row)?);
        statements.push(backend.d1.finish_publication_guard(&token).map_err(error)?);
        match backend.db.batch(statements).await{Ok(results)=>{crate::d1_repository::ensure_batch_success(&results).map_err(error)?;return Ok(PublicationStep{done:true,committed:true,assigned_id:Some(record.id),receipt:Some(receipt.into_bytes()),..Default::default()});},Err(_)=>{
            let current=target(backend,spec).await?;if current.committed!=0{return Ok(PublicationStep{done:true,committed:true,assigned_id:current.assigned_id,receipt:current.receipt.map(String::into_bytes),..Default::default()});}check_plan(backend,spec,&current).await?;
        }}
    }Err(TransferError::Busy{code:"metadata_busy",message:"concurrent membership or tag sequence updates; retry the same operation".into()})
}
pub(super) async fn rollback(backend:&WorkerTransferBackend,spec:&PublicationSpec,_cursor:Option<&str>)->Result<PublicationStep,TransferError>{
    let Some(row)=optional_target(backend,spec).await?else{return Ok(done(None))};if row.committed!=0{return Ok(PublicationStep{done:true,committed:true,assigned_id:row.assigned_id,receipt:row.receipt.map(String::into_bytes),..Default::default()});}
    run(&backend.db,"DELETE FROM transfer_publication_files WHERE session_id=? AND publication_id=? AND file_index IN(SELECT file_index FROM transfer_publication_files WHERE session_id=? AND publication_id=? LIMIT 100)",&[spec.session_id.0.as_str().into(),(spec.publication_id as f64).into(),spec.session_id.0.as_str().into(),(spec.publication_id as f64).into()]).await?;
    let more=statement(&backend.db,"SELECT 1 AS pending FROM transfer_publication_files WHERE session_id=? AND publication_id=? LIMIT 1",&binds(spec))?.first::<i64>(Some("pending")).await.map_err(error)?.is_some();if more{return Ok(PublicationStep{done:false,..Default::default()});}
    let mut statements=vec![backend.journal.execution_guard(&spec.session_id)?,numeric_guard(backend,&row)?];
    if let(Some(id),Some(generation))=(row.assigned_id,row.reservation_generation){statements.push(statement(&backend.db,"INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE EXISTS(SELECT 1 FROM novel_namespace_claims WHERE novel_id=? AND reservation_generation=? AND owner!=?)",&[(id as f64).into(),(generation as f64).into(),owner(spec).into()])?);}
    statements.push(statement(&backend.db,"UPDATE transfer_targets SET rolled_back=1 WHERE session_id=? AND publication_id=? AND committed=0",&binds(spec))?);statements.push(release_namespace(backend,spec,&row)?);
    if let(Some(id),Some(generation))=(row.assigned_id,row.reservation_generation){statements.push(statement(&backend.db,narou_rs::db::novel_query::RELEASE_ID_SQL,&[(id as f64).into(),(generation as f64).into()])?);}
    let results=backend.db.batch(statements).await.map_err(error)?;crate::d1_repository::ensure_batch_success(&results).map_err(error)?;Ok(done(row.assigned_id))
}
pub(super) async fn finish(backend:&WorkerTransferBackend,spec:&PublicationSpec,_cursor:Option<&str>)->Result<PublicationStep,TransferError>{let row=target(backend,spec).await?;if row.committed==0&&row.rolled_back==0{return Err(TransferError::RecoveryRequired{message:"publication has no durable terminal mark".into()});}
    run(&backend.db,"DELETE FROM transfer_publication_files WHERE session_id=? AND publication_id=? AND file_index IN(SELECT file_index FROM transfer_publication_files WHERE session_id=? AND publication_id=? LIMIT 100)",&[spec.session_id.0.as_str().into(),(spec.publication_id as f64).into(),spec.session_id.0.as_str().into(),(spec.publication_id as f64).into()]).await?;
    let pending=statement(&backend.db,"SELECT 1 pending FROM transfer_publication_files WHERE session_id=? AND publication_id=? LIMIT 1",&binds(spec))?.first::<i64>(Some("pending")).await.map_err(error)?.is_some();Ok(PublicationStep{done:!pending,committed:row.committed!=0,assigned_id:row.assigned_id,receipt:row.receipt.map(String::into_bytes),..Default::default()})}

pub(super) async fn gate_open(backend:&WorkerTransferBackend,spec:&PublicationSpec,_cursor:Option<&str>)->Result<PublicationStep,TransferError>{let Some(row)=optional_target(backend,spec).await?else{return Ok(done(None))};if row.committed==0&&row.rolled_back==0{return Err(TransferError::RecoveryRequired{message:"cannot reopen an unresolved publication".into()});}if row.prefix.is_none(){return Ok(done(row.assigned_id));}let prefix=prefix(&row)?;let gate=backend.d1.gate_info(&prefix).await.map_err(error)?;if gate.owner.as_deref()==Some(owner(spec).as_str()){backend.d1.open_gate(&prefix,&owner(spec),gate.epoch).await.map_err(error)?;}Ok(done(row.assigned_id))}

#[derive(Serialize,Deserialize)]
struct CleanupGate{prefix:String,owner:String,epoch:u64}
#[derive(Serialize,Deserialize)]
enum ReleaseWork{Gates(Vec<CleanupGate>),Pins(Vec<String>),Stages(Vec<String>),Rows{table:String,key:String,keys:Vec<String>},Done}

async fn release_work(backend:&WorkerTransferBackend,session:&SessionId)->Result<ReleaseWork,TransferError>{
    let gates=statement(&backend.db,"SELECT DISTINCT prefix,owner,epoch FROM transfer_snapshot_gates WHERE session_id=? ORDER BY prefix LIMIT 16",&[session.0.as_str().into()])?.all().await.map_err(error)?.results::<CleanupGate>().map_err(error)?;
    if !gates.is_empty(){return Ok(ReleaseWork::Gates(gates));}
    #[derive(Deserialize)]struct Key{key:String}
    let pins=statement(&backend.db,"SELECT pin key FROM transfer_pins WHERE session_id=? ORDER BY pin LIMIT 100",&[session.0.as_str().into()])?.all().await.map_err(error)?.results::<Key>().map_err(error)?;
    if !pins.is_empty(){return Ok(ReleaseWork::Pins(pins.into_iter().map(|row|row.key).collect()));}
    let stages=statement(&backend.db,"SELECT stage_id key FROM transfer_stage_owners o WHERE session_id=? AND NOT EXISTS(SELECT 1 FROM transfer_object_refs r WHERE r.blob_key=o.sealed_key) AND NOT EXISTS(SELECT 1 FROM transfer_pins p WHERE p.stage=o.sealed_key) ORDER BY stage_id LIMIT 2",&[session.0.as_str().into()])?.all().await.map_err(error)?.results::<Key>().map_err(error)?;
    if !stages.is_empty(){return Ok(ReleaseWork::Stages(stages.into_iter().map(|row|row.key).collect()));}
    for(table,key)in [
        ("transfer_inventory_fragments","CAST(novel_ordinal AS TEXT)||'|'||CAST(field AS TEXT)||'|'||CAST(part AS TEXT)"),
        ("transfer_export_members","CAST(parent_id AS TEXT)||'|'||CAST(position AS TEXT)"),
        ("transfer_inventory_metadata","CAST(novel_ordinal AS TEXT)"),
        ("transfer_export_sources","CAST(novel_id AS TEXT)"),
        ("transfer_catalog_indexes","CAST(novel_ordinal AS TEXT)"),
        ("transfer_source_materializations","CAST(novel_ordinal AS TEXT)||'|'||logical_path"),
        ("transfer_export_selection_state","session_id"),("transfer_snapshot_state","session_id"),
    ]{
        let rows=statement(&backend.db,&format!("SELECT {key} key FROM {table} WHERE session_id=? ORDER BY key LIMIT 100"),&[session.0.as_str().into()])?.all().await.map_err(error)?.results::<Key>().map_err(error)?;
        if !rows.is_empty(){return Ok(ReleaseWork::Rows{table:table.into(),key:key.into(),keys:rows.into_iter().map(|row|row.key).collect()});}
    }
    Ok(ReleaseWork::Done)
}

pub(super) async fn release(backend:&WorkerTransferBackend,session:&SessionRow,_cursor:Option<&str>)->Result<PublicationStep,TransferError>{
    if !session.state.is_terminal(){return Err(invalid("invalid_state","resources cannot be reclaimed before terminal CAS"));}
    backend.journal.ensure_execution(&session.session_id).await?;
    if !backend.journal.active_publications(&session.session_id,1).await?.is_empty(){return Err(TransferError::RecoveryRequired{message:"unresolved publications protect all private data".into()});}
    let work=release_work(backend,&session.session_id).await?;
    let step=backend.begin_backend_step(&session.session_id,"release_resources",&serde_json::to_string(&work).map_err(error)?).await?;
    if let Some(result)=step.result_json{return Ok(PublicationStep{done:serde_json::from_str(&result).map_err(error)?,..Default::default()});}
    let work:ReleaseWork=serde_json::from_str(&step.input_json).map_err(error)?;
    let mut statements=vec![backend.journal.execution_guard(&session.session_id)?];let mut done=false;
    match work{
        ReleaseWork::Gates(gates)=>for gate in gates{let prefix=ObjectPrefix::new(&gate.prefix).map_err(error)?;let current=backend.d1.gate_info(&prefix).await.map_err(error)?;if current.owner.as_deref()==Some(&gate.owner){backend.d1.open_gate(&prefix,&gate.owner,current.epoch).await.map_err(error)?;}statements.push(statement(&backend.db,"DELETE FROM transfer_snapshot_gates WHERE session_id=? AND prefix=? AND owner=?",&[session.session_id.0.as_str().into(),gate.prefix.into(),gate.owner.into()])?);},
        ReleaseWork::Pins(pins)=>statements.push(statement(&backend.db,"DELETE FROM transfer_pins WHERE session_id=? AND pin IN(SELECT value FROM json_each(?))",&[session.session_id.0.as_str().into(),serde_json::to_string(&pins).map_err(error)?.into()])?),
        ReleaseWork::Stages(stages)=>for stage in stages{backend.abort_owned_stage(&session.session_id,&stage).await?;},
        ReleaseWork::Rows{table,key,keys}=>{
            let valid=matches!(table.as_str(),"transfer_inventory_fragments"|"transfer_export_members"|"transfer_inventory_metadata"|"transfer_export_sources"|"transfer_catalog_indexes"|"transfer_source_materializations"|"transfer_export_selection_state"|"transfer_snapshot_state");
            if !valid||!matches!(key.as_str(),"CAST(novel_ordinal AS TEXT)||'|'||CAST(field AS TEXT)||'|'||CAST(part AS TEXT)"|"CAST(parent_id AS TEXT)||'|'||CAST(position AS TEXT)"|"CAST(novel_ordinal AS TEXT)"|"CAST(novel_id AS TEXT)"|"CAST(novel_ordinal AS TEXT)||'|'||logical_path"|"session_id"){return Err(invalid("metadata_corrupt","invalid retained cleanup descriptor"));}
            statements.push(statement(&backend.db,&format!("DELETE FROM {table} WHERE session_id=? AND ({key}) IN(SELECT value FROM json_each(?))"),&[session.session_id.0.as_str().into(),serde_json::to_string(&keys).map_err(error)?.into()])?);
        },
        ReleaseWork::Done=>done=true,
    }
    backend.journal.ensure_execution(&session.session_id).await?;
    statements.push(backend.finish_backend_step(&session.session_id,"release_resources",if done{"true"}else{"false"})?);
    let result=backend.db.batch(statements).await.map_err(error)?;crate::d1_repository::ensure_batch_success(&result).map_err(error)?;
    Ok(PublicationStep{done,..Default::default()})
}

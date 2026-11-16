//! Authenticated client-first transfer v2; no server ZIP or Base64 path.
use base64::Engine;
use futures::StreamExt;
use narou_rs::application::transfer::*;
use narou_rs::transfer::protocol::{BatchHeader,FrameDecoder};
use serde::{Deserialize,Serialize};
use worker::{Env,Method,Request,Response};
pub(crate) mod journal;
mod ticket;
mod error;
#[path="transfer/protocol.rs"]
pub(crate) mod backend;
#[path="transfer/progress.rs"]
mod publication;
#[path="transfer/source.rs"]
mod source;
#[path="transfer/steps.rs"]
mod steps;
const PREFIX:&str="/api/transfer/v2";
#[derive(Deserialize)]
struct Operation{operation_id:String}
#[derive(Deserialize)]
struct Seal{operation_id:String,#[serde(with="narou_rs::transfer::protocol::decimal_u64")]size:u64,sha256:String}
#[derive(Deserialize)]
struct Select{operation_id:String,#[serde(flatten)]selection:SelectionRequest}
#[derive(Deserialize)]
struct Publish{operation_id:String,plan_revision:String}
fn invalid(code:&'static str,message:impl Into<String>)->TransferError{TransferError::InvalidRequest{code,message:message.into()}}
fn platform(e:impl std::fmt::Display)->TransferError{TransferError::Platform{message:e.to_string()}}
fn header(req:&Request,name:&str)->Result<Option<String>,TransferError>{req.headers().get(name).map_err(platform)}
fn secret(req:&Request)->Result<Vec<u8>,TransferError>{
    let value=header(req,"x-narou-transfer-secret")?.ok_or(TransferError::SecretMismatch)?;
    let bytes=base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(value).map_err(|_|TransferError::SecretMismatch)?;
    if bytes.len()!=32{return Err(TransferError::SecretMismatch);}Ok(bytes)
}
fn control(req:&Request,operation_id:String)->Result<ControlOp,TransferError>{
    if operation_id.is_empty()||operation_id.len()>128{return Err(invalid("invalid_operation","operation_id is required"));}
    let value=header(req,"if-match")?.ok_or(TransferError::PreconditionRequired)?;
    let text=value.strip_prefix('"').and_then(|v|v.strip_suffix('"')).unwrap_or(&value);
    let revision=text.parse().map_err(|_|invalid("invalid_revision","If-Match must be a decimal revision"))?;
    Ok(ControlOp{operation_id:OperationId(operation_id),if_match_revision:revision})
}
async fn json<T:serde::de::DeserializeOwned>(req:&mut Request)->Result<T,TransferError>{
    if !header(req,"content-type")?.is_some_and(|v|v.split(';').next().is_some_and(|s|s.trim().eq_ignore_ascii_case("application/json"))){return Err(invalid("unsupported_media_type","control body must be application/json"));}
    let mut input=req.stream().map_err(platform)?;let mut bytes=Vec::new();
    while let Some(part)=input.next().await{let part=part.map_err(platform)?;
        if part.len()>MAX_CONTROL_BODY_BYTES.saturating_sub(bytes.len()){return Err(TransferError::ExceedsLimit{code:"control_too_large",message:"control body exceeds 256 KiB".into()});}
        bytes.extend_from_slice(&part);
    }
    serde_json::from_slice(&bytes).map_err(|e|invalid("invalid_json",e.to_string()))
}
fn decorate(mut response:Response)->worker::Result<Response>{
    response.headers_mut().set("Cache-Control","no-store, no-transform")?;
    response.headers_mut().set("X-Content-Type-Options","nosniff")?;Ok(response)
}
fn response<T:Serialize>(value:&T)->worker::Result<Response>{
    let bytes=serde_json::to_vec(value).map_err(|e|worker::Error::RustError(e.to_string()))?;
    if bytes.len()>MAX_CONTROL_BODY_BYTES{return error::response(&TransferError::ExceedsLimit{code:"metadata_budget",message:"control response exceeds 256 KiB".into()});}
    let mut response=Response::from_bytes(bytes)?;response.headers_mut().set("Content-Type","application/json; charset=utf-8")?;decorate(response)
}
fn status_response(status:&SessionStatus)->worker::Result<Response>{
    let mut response=response(status)?;response.headers_mut().set("ETag",&format!("\"{}\"",status.revision))?;Ok(response)
}
fn binary(bytes:Vec<u8>,permit:crate::memory_budget::MemoryPermit)->worker::Result<Response>{
    let len=bytes.len();
    // Admission is owned by the pull stream, including cancellation/error/EOF.
    let stream=futures::stream::try_unfold((bytes,0,permit),|(bytes,offset,permit)|async move{
        if offset==bytes.len(){return Ok::<_,worker::Error>(None);}
        let end=(offset+64*1024).min(bytes.len());let part=bytes[offset..end].to_vec();
        Ok(Some((part,(bytes,end,permit))))
    });
    let mut response=Response::from_stream(stream)?;response.headers_mut().set("Content-Type","application/octet-stream")?;
    response.headers_mut().set("Content-Length",&len.to_string())?;response.headers_mut().set("x-transfer-body-bytes",&len.to_string())?;decorate(response)
}
pub async fn handle(mut req:Request,env:Env)->worker::Result<Response>{
    if let Some(response)=crate::auth_failure(&req,&env).await{return response;}
    if crate::worker_role::is_reader(&env)?{return error::response(&TransferError::Unsupported{code:"writer_required",reason:"transfer belongs to the writer Worker".into()});}
    if !matches!(req.method(),Method::Get|Method::Head)&&!crate::same_origin(&req){return error::response(&invalid("origin_mismatch","same-origin request required"));}
    if header(&req,"content-encoding").ok().flatten().is_some_and(|v|!v.eq_ignore_ascii_case("identity")){return error::response(&invalid("unsupported_content_encoding","transfer bodies must be unencoded"));}
    let Some(permit)=crate::memory_budget::try_heavy()else{return error::response(&TransferError::Busy{code:"memory_capacity",message:"another heavy request is active".into()});};
    match dispatch(&mut req,&env,permit).await{Ok(response)=>Ok(response),Err(error)=>error::response(&error)}
}
async fn dispatch(req:&mut Request,env:&Env,permit:crate::memory_budget::MemoryPermit)->Result<Response,TransferError>{
    let (service,scope,journal)=crate::composition::build_transfer_service(env).await?;
    let result=dispatch_routes(req,permit,&service,&scope).await;
    let released=journal.release_request_leases().await;
    match result {Ok(response)=>{released?;Ok(response)},Err(error)=>Err(error)}
}
async fn dispatch_routes(req:&mut Request,permit:crate::memory_budget::MemoryPermit,service:&TransferService,scope:&AuthScope)->Result<Response,TransferError>{
    let path=req.path();let rest=path.strip_prefix(PREFIX).ok_or(TransferError::SessionNotFound)?;
    let parts:Vec<&str>=rest.trim_start_matches('/').split('/').collect();
    let url=req.url().map_err(platform)?;let cursor=crate::webui::query_param(&url,"cursor");let method=req.method();
    if method==Method::Get&&parts==["capabilities"]{return response(&service.capabilities(&scope).await?).map_err(platform);}
    if method==Method::Get&&parts==["sessions"]{return response(&service.list_sessions(&scope,cursor.as_deref()).await?).map_err(platform);}
    if method==Method::Post&&matches!(parts.as_slice(),["imports"]|["exports"]){
        let secret=secret(req)?;let result=if parts[0]=="imports"{service.create_import(&scope,&json(req).await?,&secret).await?}
        else{service.create_export(&scope,&json(req).await?,&secret).await?};
        return status_response(&result.status).map_err(platform);
    }
    let [direction,id,tail @ ..]=parts.as_slice()else{return Err(invalid("not_found","unknown transfer route"));};
    if !matches!(*direction,"imports"|"exports"|"sessions")||id.is_empty(){return Err(invalid("not_found","unknown transfer route"));}
    let session=SessionId((*id).into());let admin=*direction=="sessions";
    let supplied=if admin{None}else{Some(secret(req)?)};
    let status=service.session(&scope,&session,supplied.as_deref()).await?;
    if !admin&&((*direction=="imports")!=(status.direction==TransferDirection::Import)){return Err(TransferError::SessionNotFound);}
    if method==Method::Delete&&tail.is_empty(){
        let opid=crate::webui::query_param(&url,"operation_id").or(header(req,"x-transfer-operation-id")?).ok_or_else(||invalid("invalid_operation","DELETE requires operation_id"))?;
        return status_response(&service.delete_session(&scope,&session,supplied.as_deref(),&control(req,opid)?).await?).map_err(platform);
    }
    if admin{return Err(invalid("not_found","management routes never serve transfer bytes"));}
    let secret=supplied.as_deref().ok_or(TransferError::SecretMismatch)?;
    match(method,tail){
        (Method::Get,[])=>status_response(&status).map_err(platform),
        (Method::Get,["files"])=>response(&service.files(&scope,&session,secret,cursor.as_deref()).await?).map_err(platform),
        (Method::Get,["plan"])=>response(&service.plan(&scope,&session,secret,cursor.as_deref()).await?).map_err(platform),
        (Method::Get,["catalog"])=>response(&service.catalog(&scope,&session,secret,cursor.as_deref()).await?).map_err(platform),
        (Method::Put,["chunks"])=>upload(req,&service,&scope,&session,secret,None).await,
        (Method::Put,["manifest","chunks",index])=>upload(req,&service,&scope,&session,secret,Some(index.parse().map_err(|_|invalid("invalid_chunk","invalid manifest chunk index"))?)).await,
        (Method::Get,["manifest","chunks",index])=>{
            let chunk=service.export_manifest(&scope,&session,secret,index.parse().map_err(|_|invalid("invalid_chunk","invalid chunk index"))?).await?;
            let mut response=binary(chunk.bytes,permit).map_err(platform)?;
            response.headers_mut().set("ETag",&format!("\"{}\"",chunk.sha256)).map_err(platform)?;
            response.headers_mut().set("x-transfer-manifest-bytes",&chunk.total_bytes.to_string()).map_err(platform)?;
            response.headers_mut().set("x-transfer-last",if chunk.last{"true"}else{"false"}).map_err(platform)?;Ok(response)
        },
        (Method::Get,["batches",index])=>{
            let batch=service.export_batch(&scope,&session,secret,index.parse().map_err(|_|invalid("invalid_chunk","invalid batch index"))?).await?;
            let mut response=binary(batch.body,permit).map_err(platform)?;
            response.headers_mut().set("x-transfer-record-count",&batch.record_count.to_string()).map_err(platform)?;
            response.headers_mut().set("x-transfer-body-bytes",&batch.body_bytes.to_string()).map_err(platform)?;
            response.headers_mut().set("x-transfer-last",if batch.last{"true"}else{"false"}).map_err(platform)?;Ok(response)
        },
        (Method::Get,["files",file,"chunks",index])=>{
            let chunk=service.export_file_chunk(&scope,&session,secret,file.parse().map_err(|_|invalid("invalid_file","invalid file id"))?,index.parse().map_err(|_|invalid("invalid_chunk","invalid chunk index"))?).await?;
            binary(chunk.bytes,permit).map_err(platform)
        },
        (Method::Post,["manifest","seal"])=>{let body:Seal=json(req).await?;status_response(&service.seal_manifest(&scope,&session,secret,&control(req,body.operation_id)?,body.size,&body.sha256).await?).map_err(platform)},
        (Method::Post,["select"])=>{let body:Select=json(req).await?;status_response(&service.select(&scope,&session,secret,&control(req,body.operation_id)?,&body.selection).await?).map_err(platform)},
        (Method::Post,["publish"])=>{let body:Publish=json(req).await?;status_response(&service.publish(&scope,&session,secret,&control(req,body.operation_id)?,&body.plan_revision).await?).map_err(platform)},
        (Method::Post,["advance"])=>{let body:Operation=json(req).await?;status_response(&service.advance(&scope,&session,secret,&OperationId(body.operation_id)).await?.status).map_err(platform)},
        (Method::Post,[action @ ("ready"|"pause"|"resume"|"complete")])=>{
            let body:Operation=json(req).await?;let op=control(req,body.operation_id)?;
            let status=match *action{"ready"=>service.ready(&scope,&session,secret,&op).await?,"pause"=>service.pause(&scope,&session,secret,&op).await?,"resume"=>service.resume(&scope,&session,secret,&op).await?,_=>service.complete(&scope,&session,secret,&op).await?};status_response(&status).map_err(platform)
        },
        _=>Err(invalid("not_found","unknown transfer route or method")),
    }
}
/// Scheduled caller already holds its per-invocation MemoryPermit. Recovery
/// never runs detached and never reclaims resources merely because TTL passed.
pub(crate) async fn collect(env:&Env)->Result<(),TransferError>{
    let(service,scope,journal)=crate::composition::build_transfer_service(env).await?;
    let result=async{
        let now=chrono::Utc::now();
        service.recover(&scope).await?;
        service.expire_idle(&scope).await?;
        let page=journal.list_sessions(&scope,None,100).await?;
        for mut row in page.items.into_iter().filter(|row|row.state.is_terminal()&&row.next_action==NextAction::Advance).take(16){
            let operation=OperationRow{session_id:row.session_id.clone(),operation_id:OperationId(format!("gc_{}",row.revision)),kind:"maintenance".into(),request_fingerprint:row.revision.to_string(),if_match_revision:Some(row.revision),result:None,completed:false,created_at:now};
            if journal.begin_operation(&operation).await.is_err(){continue;}
            let step=service.backend().release_session_resources(&row,row.gc_cursor.as_deref()).await?;let before=row.revision;row.gc_cursor=step.cursor;row.revision+=1;row.updated_at=now;
            if step.done{row.next_action=NextAction::None;row.operation_id=None;}journal.update_session(&row,before).await?;
        }
        #[derive(Deserialize)]struct Orphan{stage_id:String,session_id:String}
        let orphans=backend::statement(journal.db(),"SELECT o.stage_id,o.session_id FROM transfer_stage_owners o JOIN transfer_sessions s USING(session_id) WHERE s.state IN ('completed','completed_with_errors','released','cancelled','failed','expired') AND NOT EXISTS(SELECT 1 FROM transfer_publications p WHERE p.session_id=s.session_id AND (p.state NOT IN ('pending','committed','rolled_back','skipped') OR (p.state='committed' AND COALESCE(p.phase_cursor,'')<>'finished'))) AND NOT EXISTS(SELECT 1 FROM transfer_object_refs r WHERE r.blob_key=o.sealed_key) AND NOT EXISTS(SELECT 1 FROM transfer_pins p WHERE p.stage=o.sealed_key) ORDER BY o.stage_id LIMIT 16",&[])?.all().await.map_err(platform)?.results::<Orphan>().map_err(platform)?;
        for orphan in orphans{journal.queue_gc(&orphan.session_id,&orphan.stage_id,"stage",now,now).await?;}
        for task in journal.due_gc(now,16).await?{
            let session=journal.session(&scope,&SessionId(task.session_id.clone())).await?;
            if !session.as_ref().is_some_and(|row|row.state.is_terminal())||!journal.active_publications(&SessionId(task.session_id.clone()),1).await?.is_empty(){journal.retry_gc(task.id,now+chrono::Duration::minutes(5)).await?;continue;}
            if journal.ensure_execution(&SessionId(task.session_id.clone())).await.is_err(){journal.retry_gc(task.id,now+chrono::Duration::minutes(5)).await?;continue;}
            let exists=backend::statement(journal.db(),"SELECT 1 live FROM transfer_stage_owners WHERE stage_id=? AND session_id=?",&[task.target.as_str().into(),task.session_id.as_str().into()])?.first::<i64>(Some("live")).await.map_err(platform)?.is_some();
            if !exists{journal.finish_gc(task.id).await?;continue;}
            match service.backend().stage_abort(&SessionId(task.session_id.clone()),&task.target).await{Ok(())=>{let retained=backend::statement(journal.db(),"SELECT 1 retained FROM transfer_stage_owners WHERE stage_id=?",&[task.target.as_str().into()])?.first::<i64>(Some("retained")).await.map_err(platform)?.is_some();if retained{journal.retry_gc(task.id,now+chrono::Duration::minutes(5)).await?;}else{journal.finish_gc(task.id).await?;}},Err(_)=>journal.retry_gc(task.id,now+chrono::Duration::minutes(5)).await?}
        }
        backend::run(journal.db(),"DELETE FROM transfer_requests WHERE created_at<? AND session_id IN(SELECT session_id FROM transfer_sessions WHERE state IN ('completed','completed_with_errors','released','cancelled','failed','expired') AND next_action='none' AND updated_at<?)",&[(now-chrono::Duration::days(7)).to_rfc3339().into(),(now-chrono::Duration::days(7)).to_rfc3339().into()]).await?;
        Ok(())
    }.await;
    let released=journal.release_request_leases().await;result.and(released)
}

async fn upload(req:&mut Request,service:&TransferService,scope:&AuthScope,session:&SessionId,secret:&[u8],manifest_index:Option<u32>)->Result<Response,TransferError>{
    if !header(req,"content-type")?.is_some_and(|v|v.split(';').next().is_some_and(|s|s.trim().eq_ignore_ascii_case("application/octet-stream"))){return Err(invalid("unsupported_media_type","binary batches require application/octet-stream"));}
    let count=header(req,"x-transfer-record-count")?.and_then(|v|v.parse::<usize>().ok()).ok_or_else(||invalid("invalid_batch","record-count header is required"))?;
    let declared=header(req,"x-transfer-body-bytes")?.and_then(|v|v.parse::<u64>().ok()).ok_or_else(||invalid("invalid_batch","body-bytes header is required"))?;
    if count==0||count>MAX_BATCH_RECORDS||declared>MAX_BINARY_BODY_BYTES as u64{return Err(TransferError::ExceedsLimit{code:"transfer_chunk_too_large",message:"batch exceeds negotiated limits".into()});}
    let mut input=req.stream().map_err(platform)?;let mut decoder=FrameDecoder::new();let mut receipts=Vec::new();let mut first=true;
    while let Some(bytes)=input.next().await{let bytes=bytes.map_err(platform)?;for part in bytes.chunks(64*1024){
        if decoder.bytes_seen()+part.len() as u64>declared{return Err(invalid("invalid_batch","body exceeds declared length"));}
        for frame in decoder.feed(part).map_err(|e|invalid("invalid_batch",e.to_string()))?{
            let BatchHeader{file_id,chunk_index,is_final,sha256,..}=frame.header;
            if manifest_index.is_some()&&(file_id!=MANIFEST_FILE_ID||(first&&Some(chunk_index)!=manifest_index)){return Err(invalid("invalid_manifest_chunk","manifest route and record identity differ"));}
            if manifest_index.is_none()&&file_id==MANIFEST_FILE_ID{return Err(invalid("invalid_file","manifest requires its own route"));}
            first=false;let sha=sha256.as_deref().and_then(sha256_from_hex).ok_or_else(||invalid("missing_sha256","upload record requires SHA-256"))?;
            let record=BatchRecord{file_id,chunk_index,payload:frame.payload,sha256:Some(sha),is_final};
            let mut result=if manifest_index.is_some(){service.put_manifest(scope,session,secret,vec![record]).await?}else{service.put_records(scope,session,secret,vec![record]).await?};
            receipts.append(&mut result.receipts);if result.failure.is_some(){result.receipts=receipts;return response(&result).map_err(platform);}
        }
    }}
    decoder.finish().map_err(|e|invalid("truncated_batch",e.to_string()))?;
    if decoder.bytes_seen()!=declared||decoder.records_seen()!=count{return Err(invalid("truncated_batch","actual batch byte/record count differs from headers"));}
    let status=service.session(scope,session,Some(secret)).await?;response(&BatchResult{receipts,failure:None,status}).map_err(platform)
}

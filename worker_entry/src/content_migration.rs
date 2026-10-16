//! Forward-only, bounded D1 episode -> S3 migration. No production auto-run.
use std::sync::Arc;
use chrono::{Duration, Utc};
use narou_rs::error::{NarouError, Result};
use serde::{Deserialize, Serialize};
use worker::{Env, Method, Request, Response};
use crate::{content_store::{self, ContentBackend, ContentStore}, db_handle::DbHandle};

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;
const STATE_KEY: &str = "migrate_content";
const LEASE_MINUTES: i64 = 15;

pub(crate) const ACQUIRE_LEASE: &str = r#"
INSERT INTO content_migration_lock(id,token,expires_at) VALUES (1,?1,?2)
ON CONFLICT(id) DO UPDATE SET token=excluded.token,expires_at=excluded.expires_at
WHERE content_migration_lock.expires_at <= ?3
RETURNING token
"#;
pub(crate) const SAVE_STATE: &str = r#"
INSERT INTO app_state(scope,key,value_json,value_yaml)
SELECT 'inv','migrate_content',?1,'{}'
WHERE EXISTS(SELECT 1 FROM content_migration_lock WHERE id=1 AND token=?2 AND expires_at>?3)
ON CONFLICT(scope,key) DO UPDATE SET value_json=excluded.value_json,value_yaml=excluded.value_yaml
RETURNING key
"#;

pub(crate) const INVENTORY_SQL: &str = r#"
SELECT (SELECT COUNT(*) FROM legacy_content_objects) AS remaining_legacy,
       (SELECT COALESCE(SUM(size),0) FROM legacy_content_objects) AS legacy_bytes,
       (SELECT COUNT(*) FROM content_objects) AS "references",
       (SELECT COUNT(*) FROM content_garbage) AS pending_garbage
"#;

#[derive(Clone, Default, Deserialize, Serialize)]
struct State {
    #[serde(default)] action: String,
    #[serde(default)] copied: u64,
    #[serde(default)] verified: u64,
    #[serde(default)] purged: u64,
    #[serde(default)] garbage_collected: u64,
    #[serde(default)] planned: u64,
    #[serde(default)] planned_bytes: u64,
    #[serde(default)] done: bool,
    #[serde(default)] next_cursor: Option<String>,
    #[serde(default)] copy_done: bool,
    #[serde(default)] verify_done: bool,
    #[serde(default)] purge_done: bool,
    #[serde(default)] failed: Vec<String>,
}
#[derive(Default, Deserialize, Serialize)]
struct Inventory { remaining_legacy: u64, legacy_bytes: u64, references: u64, pending_garbage: u64 }
#[derive(Serialize)]
struct Report {
    success: bool,
    backend: ContentBackend,
    #[serde(flatten)] state: State,
    #[serde(flatten)] inventory: Inventory,
}
#[derive(Deserialize)]
struct ValueRow { value_json: String }
#[derive(Deserialize)]
struct LeaseRow { token: String }
#[derive(Deserialize)]
struct KeyRow { key: String }
#[derive(Deserialize)]
struct Body { action: String, limit: Option<usize> }

fn error(value: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(format!("content migration: {value}"))
}
async fn inventory(db: &DbHandle) -> Result<Inventory> {
    db.prepare(INVENTORY_SQL)
        .first::<Inventory>(None).await.map_err(error)?.ok_or_else(||error("inventory unavailable"))
}
async fn load_state(db: &DbHandle) -> Result<State> {
    let row=db.prepare("SELECT value_json FROM app_state WHERE scope='inv' AND key='migrate_content'")
        .first::<ValueRow>(None).await.map_err(error)?;
    match row { Some(row)=>serde_json::from_str(&row.value_json).map_err(|_|error("invalid saved migration state")),None=>Ok(State::default()) }
}
async fn report(db: &DbHandle, state: State, success: bool) -> Result<Report> {
    Ok(Report { success, backend:content_store::backend(db).await?,state,inventory:inventory(db).await? })
}
async fn save_state(db: &DbHandle, token: &str, state: &State) -> Result<()> {
    let value=serde_json::to_string(state).map_err(error)?;
    let result=db.prepare(SAVE_STATE).bind(&[value.into(),token.into(),Utc::now().to_rfc3339().into()]).map_err(error)?
        .all().await.map_err(error)?;
    let rows=result.results::<KeyRow>().map_err(error)?;
    if !rows.iter().any(|row|row.key==STATE_KEY) { return Err(error("migration lease expired; retry from saved cursor")); }
    Ok(())
}
async fn renew(db: &DbHandle, token: &str) -> Result<()> {
    let now=Utc::now();
    let rows=db.prepare("UPDATE content_migration_lock SET expires_at=?1 WHERE id=1 AND token=?2 AND expires_at>?3 RETURNING token")
        .bind(&[(now+Duration::minutes(LEASE_MINUTES)).to_rfc3339().into(),token.into(),now.to_rfc3339().into()]).map_err(error)?
        .all().await.map_err(error)?.results::<LeaseRow>().map_err(error)?;
    if !rows.iter().any(|row|row.token==token) { return Err(error("migration lease expired; retry from saved cursor")); }
    Ok(())
}

pub async fn handle(mut req: Request, env: Env) -> worker::Result<Response> {
    if let Some(response)=crate::auth_failure(&req,&env).await { return response; }
    if !matches!(req.method(),Method::Get|Method::Post) { return crate::webui::json_error(405,"method_not_allowed",None); }
    let (action,limit)=if req.method()==Method::Get { ("status".to_string(),DEFAULT_LIMIT) } else {
        let body=match req.json::<Body>().await { Ok(body)=>body,Err(_)=>return crate::webui::json_error(400,"invalid_content_migration_request",None) };
        (body.action,body.limit.unwrap_or(DEFAULT_LIMIT))
    };
    if !matches!(action.as_str(),"status"|"plan"|"copy"|"verify"|"cutover"|"purge"|"gc") || !(1..=MAX_LIMIT).contains(&limit) {
        return crate::webui::json_error(400,"invalid_content_migration_request",Some("action must be status, plan, copy, verify, cutover, purge, or gc; limit must be 1..100"));
    }
    let db=DbHandle::primary(Arc::new(env.d1("DB")?));
    let mut state=match load_state(&db).await { Ok(state)=>state,Err(err)=>return failure(err) };
    if matches!(action.as_str(),"status"|"plan") {
        if action=="plan" {
            let counts=match inventory(&db).await { Ok(counts)=>counts,Err(err)=>return failure(err) };
            state.action="plan".into(); state.planned=counts.remaining_legacy; state.planned_bytes=counts.legacy_bytes;
            state.done=true; state.next_cursor=None;
        }
        return match report(&db,state,true).await { Ok(report)=>Response::from_json(&report),Err(err)=>failure(err) };
    }
    let token=match content_store::nonce() { Ok(token)=>token,Err(err)=>return failure(err) };
    let now=Utc::now();
    let lease=match db.prepare(ACQUIRE_LEASE).bind(&[token.as_str().into(),(now+Duration::minutes(LEASE_MINUTES)).to_rfc3339().into(),now.to_rfc3339().into()]) {
        Ok(statement)=>statement.all().await,
        Err(err)=>return failure(error(err)),
    };
    let rows=match lease { Ok(result)=>result.results::<LeaseRow>(),Err(err)=>return failure(error(err)) };
    match rows {
        Ok(rows) if rows.iter().any(|row|row.token==token)=>{},
        Ok(_)=>return crate::webui::json_error(409,"content_migration_busy",Some("another content migration action is running")),
        Err(err)=>return failure(error(err)),
    }
    // Read state only after claiming the lease; a concurrent caller may have
    // committed a page while this request was waiting for the atomic claim.
    let result=run(&env,&db,&token,&action,limit).await;
    let release=db.prepare("DELETE FROM content_migration_lock WHERE id=1 AND token=?").bind(&[token.as_str().into()]);
    if let Ok(statement)=release && let Err(err)=statement.run().await {
        worker::console_warn!("content migration lease release failed: {err}");
    }
    match result { Ok((report,status))=>Ok(Response::from_json(&report)?.with_status(status)),Err(err)=>failure(err) }
}

fn failure(err: NarouError) -> worker::Result<Response> {
    worker::console_warn!("{err}");
    crate::webui::json_error(503,"content_migration_failed",Some(&err.to_string()))
}

async fn run(env: &Env, db: &DbHandle, token: &str, action: &str, limit: usize) -> Result<(Report,u16)> {
    let mut state=load_state(db).await?;
    let mode=content_store::backend(db).await?;
    if action=="verify" && !state.copy_done || action=="cutover" && (!state.copy_done || !state.verify_done) || action=="purge" && mode!=ContentBackend::S3 {
        state.action=action.into(); state.done=false; state.next_cursor=None;
        state.failed=vec!["required previous migration phase is incomplete".into()];
        return Ok((report(db,state,false).await?,409));
    }
    let remote=crate::s3_store::store_from_env(env,crate::budget::SubrequestBudget::new()).await.map_err(error)?;
    let store=ContentStore::new(db.clone(),remote.clone(),remote);
    if state.action!=action {
        state.next_cursor=None;
        state.done=false;
    }
    state.action=action.into();
    state.failed.clear();
    let result:Result<()>=async {
        match action {
            "copy"=>{
                if state.copy_done { state.done=true; return Ok(()); }
                // S3-default initialization may already have selected migrating.
                if mode==ContentBackend::D1 || state.planned==0 {
                    let counts=inventory(db).await?;
                    state.planned=counts.remaining_legacy; state.planned_bytes=counts.legacy_bytes;
                }
                store.begin_migration().await?;
                let page=store.legacy_page(state.next_cursor.as_deref(),limit).await?;
                let done=page.len()<limit;
                for metadata in page {
                    renew(db,token).await?;
                    if store.copy_legacy(&metadata.key).await? { state.copied+=1; }
                    state.next_cursor=Some(metadata.key.to_string());
                    save_state(db,token,&state).await?;
                }
                state.done=done; state.copy_done=done;
            }
            "verify"=>{
                if state.verify_done { state.done=true; return Ok(()); }
                let page=store.reference_page(state.next_cursor.as_deref(),limit).await?;
                let done=page.len()<limit;
                for metadata in page {
                    renew(db,token).await?;
                    store.verify_reference(&metadata.key).await?;
                    state.verified+=1; state.next_cursor=Some(metadata.key.to_string());
                    save_state(db,token,&state).await?;
                }
                state.done=done; state.verify_done=done;
            }
            "cutover"=>{ renew(db,token).await?; store.cutover().await?; state.done=true; }
            "purge"=>{
                renew(db,token).await?;
                let removed=store.purge_legacy_page(limit).await?;
                state.purged+=removed;
                state.done=inventory(db).await?.remaining_legacy==0;
                if removed==0 && !state.done { return Err(error("legacy rows lack verified references")); }
                state.purge_done=state.done;
            }
            "gc"=>{
                renew(db,token).await?;
                let removed=store.collect_garbage(limit).await?;
                state.garbage_collected+=removed; state.done=removed<limit as u64;
            }
            _=>return Err(error("invalid migration action")),
        }
        Ok(())
    }.await;
    let status=match result {
        Ok(())=>200,
        Err(err)=>{ state.failed=vec![err.to_string()]; state.done=false; 503 }
    };
    save_state(db,token,&state).await?;
    Ok((report(db,state,status==200).await?,status))
}

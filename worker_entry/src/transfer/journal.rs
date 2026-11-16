//! Durable transfer journal on primary D1 — the Worker implementation of
//! `narou_rs::application::transfer::TransferJournal`.
//!
//! Every read goes through a `DbHandle` that the caller guarantees is
//! primary-bound (`for_writes`): confirmed receipts, plan state and seals are
//! never decided on a read replica. One row per session/file/chunk/
//! operation/create/publication/journal-blob — no app_state blobs.
//!
//! Opaque byte columns (spec/receipt/result/journal values, hash
//! checkpoints) are stored base64-encoded in TEXT columns: control-plane
//! reads never materialize D1 BLOBs into JSON rows.

use base64::Engine;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use worker::wasm_bindgen::JsValue;
use worker::{D1PreparedStatement, D1Result};

use narou_rs::application::transfer::{
    AuthScope, CatalogItem, ChunkRow, CreateRow, FileRow, OperationRow, Page, PlanEntry,
    PublicationRow, SessionRow, SessionState, TransferError, TransferJournal,
};
use narou_rs::platform::PlatformFuture;

use crate::db_handle::DbHandle;

fn platform(error: impl std::fmt::Display) -> TransferError {
    TransferError::Platform { message: format!("transfer journal: {error}") }
}

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

fn b64_encode(data: &[u8]) -> String {
    B64.encode(data)
}

fn b64_decode(value: &str) -> Result<Vec<u8>, TransferError> {
    B64.decode(value)
        .map_err(|error| platform(format!("corrupt journal blob: {error}")))
}

// ---------------------------------------------------------------------------
// Bind helpers
// ---------------------------------------------------------------------------

enum B {
    T(String),
    I(i64),
    N,
}

fn t(value: impl Into<String>) -> B {
    B::T(value.into())
}
fn i(value: i64) -> B {
    B::I(value)
}
fn u(value: u64) -> B {
    B::I(value as i64)
}
fn u32v(value: u32) -> B {
    B::I(value as i64)
}
fn opt(value: Option<String>) -> B {
    value.map(B::T).unwrap_or(B::N)
}
fn opt_i(value: Option<i64>) -> B {
    value.map(B::I).unwrap_or(B::N)
}
fn opt_u(value: Option<u64>) -> B {
    value.map(|v| B::I(v as i64)).unwrap_or(B::N)
}
fn b64(value: &[u8]) -> B {
    B::T(b64_encode(value))
}
fn opt_b64(value: &Option<Vec<u8>>) -> B {
    value.as_ref().map(|v| B::T(b64_encode(v))).unwrap_or(B::N)
}

fn bind(statement: D1PreparedStatement, values: Vec<B>) -> Result<D1PreparedStatement, TransferError> {
    let values: Vec<JsValue> = values
        .into_iter()
        .map(|value| match value {
            B::T(v) => JsValue::from_str(&v),
            B::I(v) => JsValue::from_f64(v as f64),
            B::N => JsValue::NULL,
        })
        .collect();
    statement.bind(&values).map_err(platform)
}

fn changed(result: &D1Result) -> Result<u64, TransferError> {
    if !result.success() {
        return Err(platform(result.error().unwrap_or_else(|| "D1 statement failed".into())));
    }
    Ok(result
        .meta()
        .map_err(platform)?
        .and_then(|meta| meta.changes)
        .unwrap_or(0) as u64)
}

fn rfc3339(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

fn parse_time(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .unwrap_or_else(|_| DateTime::<Utc>::UNIX_EPOCH)
}

/// Enum -> its snake_case wire name via serde.
fn enum_name<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn enum_parse<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, TransferError> {
    serde_json::from_str(&format!("\"{value}\""))
        .map_err(|error| platform(format!("invalid enum value {value:?}: {error}")))
}

// ---------------------------------------------------------------------------
// D1 row shapes (columns ↔ serde)
// ---------------------------------------------------------------------------


#[derive(Debug, Deserialize)]
struct SessionDbRow {
    session_id: String,
    library_id: String,
    principal: String,
    secret_hash: String,
    direction: String,
    state: String,
    resume_phase: Option<String>,
    revision: i64,
    conflict_policy: String,
    profile: String,
    extra_roles_json: String,
    policy_version: i64,
    plan_revision: Option<String>,
    gc_cursor: Option<String>,
    catalog_revision: i64,
    chunk_bytes: i64,
    confirmed_bytes: i64,
    published_novels: u32,
    next_action: String,
    operation_id: Option<String>,
    last_operation_id: Option<String>,
    last_operation_result: Option<String>,
    error: Option<String>,
    ext_json: String,
    created_at: String,
    updated_at: String,
    expires_at: String,
}

impl SessionDbRow {
    fn into_row(self) -> Result<SessionRow, TransferError> {
        use narou_rs::application::transfer::*;
        let ext: SessionExt = serde_json::from_str(&self.ext_json)
            .map_err(|error| platform(format!("session ext: {error}")))?;
        let direction: TransferDirection = enum_parse(&self.direction)?;
        // SessionExt kind must agree with the direction column.
        match (&direction, &ext) {
            (TransferDirection::Import, SessionExt::Import(_))
            | (TransferDirection::Export, SessionExt::Export(_)) => {}
            _ => return Err(platform("session ext kind does not match direction")),
        }
        Ok(SessionRow {
            session_id: SessionId(self.session_id),
            scope: AuthScope { library_id: self.library_id, principal: self.principal },
            secret_hash: SecretHash(self.secret_hash),
            direction,
            state: enum_parse(&self.state)?,
            resume_phase: self
                .resume_phase
                .as_deref()
                .map(enum_parse)
                .transpose()?,
            revision: self.revision as u64,
            conflict_policy: enum_parse(&self.conflict_policy)?,
            profile: enum_parse(&self.profile)?,
            extra_roles: serde_json::from_str(&self.extra_roles_json)
                .map_err(|error| platform(format!("session extra_roles: {error}")))?,
            policy_version: self.policy_version as u32,
            plan_revision: self.plan_revision,
            catalog_revision: self.catalog_revision as u64,
            chunk_bytes: self.chunk_bytes as u64,
            confirmed_bytes: self.confirmed_bytes as u64,
            published_novels: self.published_novels,
            next_action: enum_parse(&self.next_action)?,
            operation_id: self.operation_id.map(OperationId),
            last_operation_id: self.last_operation_id.map(OperationId),
            last_operation_result: self.last_operation_result.as_deref().map(b64_decode).transpose()?,
            error: self.error,
            created_at: parse_time(&self.created_at),
            updated_at: parse_time(&self.updated_at),
            expires_at: parse_time(&self.expires_at),
            gc_cursor: self.gc_cursor,
            ext,
        })
    }
}

#[derive(Debug, Deserialize)]
struct FileDbRow {
    session_id: String,
    file_id: i64,
    novel_ordinal: i64,
    logical_path: String,
    role: String,
    size: i64,
    sha256: String,
    state: String,
    confirmed_bytes: i64,
    next_chunk_index: i64,
    confirmed_prefix_sha256: Option<String>,
    hash_checkpoint: Option<String>,
    stage: Option<String>,
    pin: Option<String>,
    finalize_operation: Option<String>,
}

impl FileDbRow {
    fn into_row(self) -> Result<FileRow, TransferError> {
        Ok(FileRow {
            session_id: narou_rs::application::transfer::SessionId(self.session_id),
            file_id: self.file_id as u32,
            novel_ordinal: self.novel_ordinal as u32,
            logical_path: self.logical_path,
            role: enum_parse(&self.role)?,
            size: self.size as u64,
            sha256: self.sha256,
            state: enum_parse(&self.state)?,
            confirmed_bytes: self.confirmed_bytes as u64,
            next_chunk_index: self.next_chunk_index as u32,
            confirmed_prefix_sha256: self.confirmed_prefix_sha256,
            hash_checkpoint: self
                .hash_checkpoint
                .as_deref()
                .map(b64_decode)
                .transpose()?,
            stage: self.stage,
            pin: self.pin,
            finalize_operation: self.finalize_operation.map(narou_rs::application::transfer::OperationId),
        })
    }
}

#[derive(Debug, Deserialize)]
struct ChunkDbRow {
    session_id: String,
    file_id: i64,
    chunk_index: i64,
    payload_size: i64,
    declared_sha256: Option<String>,
    stored_sha256: String,
    prefix_sha256: String,
    hash_checkpoint: Option<String>,
    offset: i64,
    is_final: i64,
}

impl ChunkDbRow {
    fn into_row(self) -> Result<ChunkRow, TransferError> {
        Ok(ChunkRow {
            session_id: narou_rs::application::transfer::SessionId(self.session_id),
            file_id: self.file_id as u32,
            chunk_index: self.chunk_index as u32,
            payload_size: self.payload_size as u64,
            declared_sha256: self.declared_sha256,
            stored_sha256: self.stored_sha256,
            prefix_sha256: self.prefix_sha256,
            hash_checkpoint: self
                .hash_checkpoint
                .as_deref()
                .map(b64_decode)
                .transpose()?,
            offset: self.offset as u64,
            is_final: self.is_final != 0,
        })
    }
}

#[derive(Debug, Deserialize)]
struct OperationDbRow {
    session_id: String,
    operation_id: String,
    kind: String,
    request_fingerprint: String,
    if_match_revision: Option<i64>,
    result: Option<String>,
    completed: i64,
    created_at: String,
}

impl OperationDbRow {
    fn into_row(self) -> Result<OperationRow, TransferError> {
        Ok(OperationRow {
            session_id: narou_rs::application::transfer::SessionId(self.session_id),
            operation_id: narou_rs::application::transfer::OperationId(self.operation_id),
            kind: self.kind,
            request_fingerprint: self.request_fingerprint,
            if_match_revision: self.if_match_revision.map(|v| v as u64),
            result: self.result.as_deref().map(b64_decode).transpose()?,
            completed: self.completed != 0,
            created_at: parse_time(&self.created_at),
        })
    }
}

#[derive(Debug, Deserialize)]
struct CreateDbRow {
    request_id: String,
    library_id: String,
    principal: String,
    request_fingerprint: String,
    session_id: Option<String>,
    receipt: Option<String>,
    created_at: String,
}

impl CreateDbRow {
    fn into_row(self) -> Result<CreateRow, TransferError> {
        Ok(CreateRow {
            request_id: self.request_id,
            scope: AuthScope { library_id: self.library_id, principal: self.principal },
            request_fingerprint: self.request_fingerprint,
            session_id: self.session_id.map(narou_rs::application::transfer::SessionId),
            receipt: self.receipt.as_deref().map(b64_decode).transpose()?,
            created_at: parse_time(&self.created_at),
        })
    }
}

#[derive(Debug, Deserialize)]
struct PublicationDbRow {
    session_id: String,
    publication_id: i64,
    state: String,
    spec: String,
    phase_cursor: Option<String>,
    assigned_id: Option<i64>,
    receipt: Option<String>,
    error: Option<String>,
}

impl PublicationDbRow {
    fn into_row(self) -> Result<PublicationRow, TransferError> {
        Ok(PublicationRow {
            session_id: narou_rs::application::transfer::SessionId(self.session_id),
            publication_id: self.publication_id as u32,
            state: enum_parse(&self.state)?,
            spec: b64_decode(&self.spec)?,
            phase_cursor: self.phase_cursor,
            assigned_id: self.assigned_id,
            receipt: self.receipt.as_deref().map(b64_decode).transpose()?,
            error: self.error,
        })
    }
}

#[derive(Debug, Deserialize)]
struct PlanDbRow {
    novel_ordinal: i64,
    toc_url: String,
    title: String,
    kind: String,
    action: String,
    reason: Option<String>,
    existing_id: Option<i64>,
    record_revision: Option<String>,
    content_revision: Option<String>,
    membership_digest: Option<String>,
    file_count: i64,
    total_bytes: i64,
    receipt: Option<String>,
}

impl PlanDbRow {
    fn into_entry(self) -> Result<PlanEntry, TransferError> {
        Ok(PlanEntry {
            novel_ordinal: self.novel_ordinal as u32,
            toc_url: self.toc_url,
            title: self.title,
            kind: enum_parse(&self.kind)?,
            action: enum_parse(&self.action)?,
            reason: self.reason,
            existing_id: self.existing_id,
            record_revision: self.record_revision,
            content_revision: self.content_revision,
            membership_digest: self.membership_digest,
            file_count: self.file_count as u32,
            total_bytes: self.total_bytes as u64,
            receipt: self.receipt.as_deref().map(b64_decode).transpose()?.map(|value|serde_json::from_slice(&value).map_err(platform)).transpose()?,
        })
    }
}

#[derive(Debug, Deserialize)]
struct CatalogDbRow {
    novel_ordinal: i64,
    logical_path: String,
    existing_id: i64,
    toc_url: String,
    title: String,
    kind: String,
    role: String,
    size: i64,
    selectable: i64,
    selected: i64,
    required: i64,
    exclusion_reason: Option<String>,
    policy_version: i64,
    runtime_json: String,
    complete_json: String,
}

impl CatalogDbRow {
    fn into_item(self) -> Result<CatalogItem, TransferError> {
        Ok(CatalogItem {
            novel_ordinal: self.novel_ordinal as u32,
            existing_id: self.existing_id,
            toc_url: self.toc_url,
            title: self.title,
            kind: enum_parse(&self.kind)?,
            logical_path: self.logical_path,
            role: enum_parse(&self.role)?,
            size: self.size as u64,
            selectable: self.selectable != 0,
            selected: self.selected != 0,
            required: self.required != 0,
            exclusion_reason: self.exclusion_reason,
            policy_version: self.policy_version as u32,
            runtime: serde_json::from_str(&self.runtime_json).map_err(platform)?,
            complete: serde_json::from_str(&self.complete_json).map_err(platform)?,
        })
    }
}

#[derive(Debug, Deserialize)]
struct GcDbRow {
    id: i64,
    session_id: String,
    target: String,
}

/// A queued GC target (returned by [`TransferJournal::due_gc`]).
#[derive(Debug)]
pub struct GcTask {
    pub id: i64,
    pub session_id: String,
    pub target: String,
}

// ---------------------------------------------------------------------------
// Journal
// ---------------------------------------------------------------------------

pub struct D1TransferJournal {
    db: DbHandle,
    request_owner: String,
    claimed: parking_lot::Mutex<std::collections::HashSet<String>>,
}

impl D1TransferJournal {
    pub fn new(db: DbHandle) -> Self {
        Self { db: db.primary_view(), request_owner: crate::content_store::nonce().expect("Worker Web Crypto is required"), claimed: parking_lot::Mutex::new(std::collections::HashSet::new()) }
    }

    pub fn db(&self) -> &DbHandle {
        &self.db
    }

    pub(crate) fn execution_owner(&self)->&str {&self.request_owner}


    pub(crate) fn execution_guard(&self, session: &narou_rs::application::transfer::SessionId) -> Result<D1PreparedStatement,TransferError> {
        self.db.prepare("INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM transfer_execution_leases WHERE session_id=? AND owner=? AND expires_at>unixepoch())")
            .bind(&[session.0.as_str().into(),self.request_owner.as_str().into()]).map_err(platform)
    }

    pub(crate) async fn ensure_execution(&self,session:&narou_rs::application::transfer::SessionId)->Result<(),TransferError> {
        let claimed=self.claimed.lock().contains(&session.0);
        let statement=if claimed {
            self.db.prepare("UPDATE transfer_execution_leases SET expires_at=unixepoch()+60 WHERE session_id=? AND owner=? AND expires_at>unixepoch()").bind(&[session.0.as_str().into(),self.request_owner.as_str().into()]).map_err(platform)?
        } else {
            self.db.prepare("INSERT INTO transfer_execution_leases(session_id,owner,operation_id,expires_at) VALUES(?,?,'internal',unixepoch()+60) ON CONFLICT(session_id) DO UPDATE SET owner=excluded.owner,operation_id=excluded.operation_id,expires_at=excluded.expires_at WHERE transfer_execution_leases.expires_at<=unixepoch()").bind(&[session.0.as_str().into(),self.request_owner.as_str().into()]).map_err(platform)?
        };
        let results=self.db.batch(vec![statement,self.execution_guard(session)?]).await.map_err(|_|TransferError::Busy{code:"operation_busy",message:"session invocation lease is unavailable or expired".into()})?;
        for result in &results {changed(result)?;}self.claimed.lock().insert(session.0.clone());Ok(())
    }

    async fn guarded_batch(&self,session:&narou_rs::application::transfer::SessionId,mut statements:Vec<D1PreparedStatement>)->Result<Vec<D1Result>,TransferError>{
        self.ensure_execution(session).await?;statements.insert(0,self.execution_guard(session)?);let results=self.db.batch(statements).await.map_err(platform)?;for result in &results {changed(result)?;}Ok(results)
    }

    pub(crate) async fn release_request_leases(&self) -> Result<(),TransferError> {
        self.db.prepare("DELETE FROM transfer_execution_leases WHERE owner=?").bind(&[self.request_owner.as_str().into()]).map_err(platform)?.run().await.map_err(platform)?;Ok(())
    }

    async fn session_row(&self, id: &narou_rs::application::transfer::SessionId) -> Result<Option<SessionRow>, TransferError> {
        let row = self
            .db
            .prepare("SELECT * FROM transfer_sessions WHERE session_id = ?")
            .bind(&[JsValue::from_str(&id.0)])
            .map_err(platform)?
            .first::<SessionDbRow>(None)
            .await
            .map_err(platform)?;
        row.map(SessionDbRow::into_row).transpose()
    }

    /// GC sweep: expired idle sessions (also see `find_idle_sessions` which
    /// the shared engine drives per scope; this helper covers the periodic
    /// whole-table sweep the scheduled path uses).
    pub async fn due_gc(&self, now: DateTime<Utc>, limit: usize) -> Result<Vec<GcTask>, TransferError> {
        let result = self
            .db
            .prepare("SELECT id,session_id,target FROM transfer_gc WHERE not_before <= ? ORDER BY id ASC LIMIT ?")
            .bind(&[
                JsValue::from_str(&rfc3339(now)),
                JsValue::from_f64(limit as f64),
            ])
            .map_err(platform)?
            .all()
            .await
            .map_err(platform)?;
        Ok(result
            .results::<GcDbRow>()
            .map_err(platform)?
            .into_iter()
            .map(|row| GcTask {
                id: row.id,
                session_id: row.session_id,
                target: row.target,
            })
            .collect())
    }

    pub async fn queue_gc(
        &self,
        session_id: &str,
        target: &str,
        kind: &str,
        not_before: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<(), TransferError> {
        let statement = bind(
            self.db.prepare(
                "INSERT OR IGNORE INTO transfer_gc (session_id, target, kind, attempts, not_before, created_at)
                 VALUES (?,?,?,0,?,?)",
            ),
            vec![
                t(session_id),
                t(target),
                t(kind),
                t(rfc3339(not_before)),
                t(rfc3339(now)),
            ],
        )?;
        changed(&statement.run().await.map_err(platform)?)?;
        Ok(())
    }

    pub async fn finish_gc(&self, id: i64) -> Result<(), TransferError> {
        self.db
            .prepare("DELETE FROM transfer_gc WHERE id = ?")
            .bind(&[JsValue::from_f64(id as f64)])
            .map_err(platform)?
            .run()
            .await
            .map_err(platform)?;
        Ok(())
    }

    pub async fn retry_gc(&self, id: i64, not_before: DateTime<Utc>) -> Result<(), TransferError> {
        let statement = bind(
            self.db.prepare(
                "UPDATE transfer_gc SET attempts = attempts + 1, not_before = ? WHERE id = ?",
            ),
            vec![t(rfc3339(not_before)), i(id)],
        )?;
        changed(&statement.run().await.map_err(platform)?)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// TransferJournal implementation
// ---------------------------------------------------------------------------

fn session_binds(row: &SessionRow) -> Vec<B> {
    vec![
        t(row.session_id.0.clone()),
        t(row.scope.library_id.clone()),
        t(row.scope.principal.clone()),
        t(row.secret_hash.0.clone()),
        t(enum_name(&row.direction)),
        t(enum_name(&row.state)),
        opt(row.resume_phase.map(|phase| enum_name(&phase))),
        u(row.revision),
        t(enum_name(&row.conflict_policy)),
        t(enum_name(&row.profile)),
        t(serde_json::to_string(&row.extra_roles).unwrap_or_else(|_| "[]".into())),
        u32v(row.policy_version),
        opt(row.plan_revision.clone()),
        opt(row.gc_cursor.clone()),
        u(row.catalog_revision),
        u(row.chunk_bytes),
        u(row.confirmed_bytes),
        u32v(row.published_novels),
        t(enum_name(&row.next_action)),
        opt(row.operation_id.as_ref().map(|id| id.0.clone())),
        opt(row.last_operation_id.as_ref().map(|id| id.0.clone())),
        opt_b64(&row.last_operation_result),
        opt(row.error.clone()),
        t(serde_json::to_string(&row.ext).unwrap_or_else(|_| "{}".into())),
        t(rfc3339(row.created_at)),
        t(rfc3339(row.updated_at)),
        t(rfc3339(row.expires_at)),
    ]
}

const SESSION_COLUMNS: &str = "session_id, library_id, principal, secret_hash,
    direction, state, resume_phase, revision, conflict_policy, profile,
    extra_roles_json, policy_version, plan_revision, gc_cursor, catalog_revision,
    chunk_bytes, confirmed_bytes, published_novels, next_action, operation_id, last_operation_id, last_operation_result, error, ext_json,
    created_at, updated_at, expires_at";

const SESSION_UPDATE_SETS: &str = "library_id=?, principal=?, secret_hash=?,
    direction=?, state=?, resume_phase=?, revision=?, conflict_policy=?,
    profile=?, extra_roles_json=?, policy_version=?, plan_revision=?,
    gc_cursor=?,
    catalog_revision=?, chunk_bytes=?, confirmed_bytes=?, published_novels=MAX(published_novels,?), next_action=?,
    operation_id=?, last_operation_id=?, last_operation_result=?, error=?, ext_json=?, created_at=?, updated_at=?, expires_at=?";

fn file_binds(row: &FileRow) -> Vec<B> {
    vec![
        t(row.session_id.0.clone()),
        u32v(row.file_id),
        u32v(row.novel_ordinal),
        t(row.logical_path.clone()),
        t(enum_name(&row.role)),
        u(row.size),
        t(row.sha256.clone()),
        t(enum_name(&row.state)),
        u(row.confirmed_bytes),
        u32v(row.next_chunk_index),
        opt(row.confirmed_prefix_sha256.clone()),
        opt_b64(&row.hash_checkpoint),
        opt(row.stage.clone()),
        opt(row.pin.clone()),
        opt(row.finalize_operation.as_ref().map(|operation|operation.0.clone())),
    ]
}

const FILE_COLUMNS: &str = "session_id, file_id, novel_ordinal, logical_path,
    role, size, sha256, state, confirmed_bytes, next_chunk_index,
    confirmed_prefix_sha256, hash_checkpoint, stage, pin, finalize_operation";

fn publication_binds(row: &PublicationRow) -> Vec<B> {
    vec![
        t(row.session_id.0.clone()),
        u32v(row.publication_id),
        t(enum_name(&row.state)),
        b64(&row.spec),
        opt(row.phase_cursor.clone()),
        opt_i(row.assigned_id),
        opt_b64(&row.receipt),
        opt(row.error.clone()),
    ]
}


fn byte_page<T: serde::Serialize>(mut items: Vec<T>, next_cursor: Option<String>, key: impl Fn(&T)->String) -> Result<Page<T>,TransferError> {
    let mut bytes=64usize;let mut take=0;
    for item in &items {let size=serde_json::to_vec(item).map_err(platform)?.len()+1;
        if bytes+size>200*1024 {break;}bytes+=size;take+=1;
    }
    if take==0&&!items.is_empty(){return Err(TransferError::ExceedsLimit{code:"metadata_budget",message:"one metadata item exceeds the response page budget".into()});}
    let next=if take<items.len(){items.get(take-1).map(key)}else{next_cursor};items.truncate(take);
    Ok(Page{items,next_cursor:next})
}

impl TransferJournal for D1TransferJournal {
    // -- sessions -----------------------------------------------------------

    fn create_session<'a>(&'a self, row: &'a SessionRow) -> PlatformFuture<'a, Result<(), TransferError>> {
        Box::pin(async move {
            let mut values=session_binds(row);
            values.push(t(row.scope.library_id.clone()));
            values.push(t(row.scope.principal.clone()));
            let statement=bind(self.db.prepare(&format!(
                "INSERT INTO transfer_sessions ({SESSION_COLUMNS}) SELECT {} WHERE
                 (SELECT COUNT(*) FROM transfer_sessions WHERE library_id=? AND principal=?
                  AND state NOT IN ('completed','completed_with_errors','released','cancelled','failed','expired')) < 2",
                vec!["?";27].join(","))),values)?;
            if changed(&statement.run().await.map_err(platform)?)?==0 {
                return Err(TransferError::TooManySessions{max:2});
            }
            Ok(())
        })
    }

    fn session<'a>(
        &'a self,
        scope: &'a AuthScope,
        id: &'a narou_rs::application::transfer::SessionId,
    ) -> PlatformFuture<'a, Result<Option<SessionRow>, TransferError>> {
        Box::pin(async move {
            let Some(row) = self.session_row(id).await? else {
                return Ok(None);
            };
            if row.scope != *scope {
                return Err(TransferError::SessionNotFound);
            }
            Ok(Some(row))
        })
    }

    fn update_session<'a>(
        &'a self,
        row: &'a SessionRow,
        expected_revision: u64,
    ) -> PlatformFuture<'a, Result<(), TransferError>> {
        Box::pin(async move {
            let mut values = session_binds(row);
            // Drop the PK value; keep the remaining 22 column values.
            values.remove(0);
            values.push(t(row.session_id.0.clone()));
            values.push(u(expected_revision));
            let statement = bind(
                self.db.prepare(&format!(
                    "UPDATE transfer_sessions SET {SESSION_UPDATE_SETS} WHERE session_id = ? AND revision = ?"
                )),
                values,
            )?;
            let results=self.guarded_batch(&row.session_id,vec![statement]).await?;
            if changed(&results[1])? == 0 {
                let current = self.session_row(&row.session_id).await?.map(|r| r.revision).unwrap_or(0);
                return Err(TransferError::RevisionMismatch { current });
            }
            Ok(())
        })
    }

    fn list_sessions<'a>(
        &'a self,
        scope: &'a AuthScope,
        cursor: Option<&'a str>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Page<SessionRow>, TransferError>> {
        Box::pin(async move {
            // `updated_at` ascending; the cursor is the previous page's last
            // `updated_at|session_id` pair (opaque to clients).
            let (after_time, after_id) = cursor
                .and_then(|c| c.split_once('|').map(|(a, b)| (a.to_string(), b.to_string())))
                .unwrap_or_default();
            let rows: Vec<SessionDbRow> = if cursor.is_some() {
                self.db
                    .prepare(
                        "SELECT * FROM transfer_sessions
                         WHERE library_id = ? AND principal = ?
                           AND (updated_at > ? OR (updated_at = ? AND session_id > ?))
                         ORDER BY updated_at ASC, session_id ASC LIMIT ?",
                    )
                    .bind(&[
                        JsValue::from_str(&scope.library_id),
                        JsValue::from_str(&scope.principal),
                        JsValue::from_str(&after_time),
                        JsValue::from_str(&after_time),
                        JsValue::from_str(&after_id),
                        JsValue::from_f64(limit as f64 + 1.0),
                    ])
                    .map_err(platform)?
                    .all()
                    .await
                    .map_err(platform)?
                    .results::<SessionDbRow>()
                    .map_err(platform)?
            } else {
                self.db
                    .prepare(
                        "SELECT * FROM transfer_sessions
                         WHERE library_id = ? AND principal = ?
                         ORDER BY updated_at ASC, session_id ASC LIMIT ?",
                    )
                    .bind(&[
                        JsValue::from_str(&scope.library_id),
                        JsValue::from_str(&scope.principal),
                        JsValue::from_f64(limit as f64 + 1.0),
                    ])
                    .map_err(platform)?
                    .all()
                    .await
                    .map_err(platform)?
                    .results::<SessionDbRow>()
                    .map_err(platform)?
            };
            let mut items = Vec::with_capacity(rows.len().min(limit));
            let mut next_cursor = None;
            for (index, row) in rows.into_iter().enumerate() {
                if index == limit {
                    break;
                }
                next_cursor = Some(format!("{}|{}", row.updated_at, row.session_id));
                items.push(row.into_row()?);
            }
            if items.len() < limit || next_cursor.is_none() {
                next_cursor = None;
            }
            Ok(Page { items, next_cursor })
        })
    }

    fn count_live_sessions<'a>(
        &'a self,
        scope: &'a AuthScope,
    ) -> PlatformFuture<'a, Result<u32, TransferError>> {
        Box::pin(async move {
            let states = live_state_set();
            let count = self
                .db
                .prepare(&format!(
                    "SELECT COUNT(*) AS n FROM transfer_sessions
                     WHERE library_id = ? AND principal = ? AND state IN ({states})"
                ))
                .bind(&[
                    JsValue::from_str(&scope.library_id),
                    JsValue::from_str(&scope.principal),
                ])
                .map_err(platform)?
                .first::<i64>(Some("n"))
                .await
                .map_err(platform)?
                .unwrap_or(0);
            Ok(count as u32)
        })
    }

    fn find_idle_sessions<'a>(
        &'a self,
        scope: &'a AuthScope,
        now: DateTime<Utc>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Vec<SessionRow>, TransferError>> {
        Box::pin(async move {
            let states = live_state_set();
            let result = self
                .db
                .prepare(&format!(
                    "SELECT * FROM transfer_sessions
                     WHERE library_id = ? AND principal = ? AND expires_at <= ?
                       AND state IN ({states})
                     ORDER BY expires_at ASC LIMIT ?"
                ))
                .bind(&[
                    JsValue::from_str(&scope.library_id),
                    JsValue::from_str(&scope.principal),
                    JsValue::from_str(&rfc3339(now)),
                    JsValue::from_f64(limit as f64),
                ])
                .map_err(platform)?
                .all()
                .await
                .map_err(platform)?;
            result
                .results::<SessionDbRow>()
                .map_err(platform)?
                .into_iter()
                .map(SessionDbRow::into_row)
                .collect()
        })
    }

    fn sessions_with_pending_publications<'a>(
        &'a self,
        scope: &'a AuthScope,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Vec<SessionRow>, TransferError>> {
        Box::pin(async move {
            let result = self
                .db
                .prepare(
                    "SELECT s.* FROM transfer_sessions s
                     WHERE s.library_id = ? AND s.principal = ? AND (s.updated_at <= ? OR s.expires_at <= ?)
                       AND NOT EXISTS(SELECT 1 FROM transfer_execution_leases l WHERE l.session_id=s.session_id AND l.owner<>? AND l.expires_at>unixepoch())
                       AND (s.state IN ('publishing','cancelling') OR s.resume_phase='publishing' OR EXISTS (
                         SELECT 1 FROM transfer_publications p
                         WHERE p.session_id = s.session_id
                           AND (p.state NOT IN ('pending','committed','rolled_back','skipped') OR (p.state='committed' AND COALESCE(p.phase_cursor,'')<>'finished'))))
                     ORDER BY s.updated_at ASC LIMIT ?",
                )
                .bind(&[
                    JsValue::from_str(&scope.library_id),
                    JsValue::from_str(&scope.principal),
                    JsValue::from_str(&rfc3339(Utc::now()-chrono::Duration::seconds(60))),
                    JsValue::from_str(&rfc3339(Utc::now())),
                    JsValue::from_str(&self.request_owner),
                    JsValue::from_f64(limit as f64),
                ])
                .map_err(platform)?
                .all()
                .await
                .map_err(platform)?;
            result
                .results::<SessionDbRow>()
                .map_err(platform)?
                .into_iter()
                .map(SessionDbRow::into_row)
                .collect()
        })
    }

    // -- create idempotency --------------------------------------------------

    fn find_create<'a>(
        &'a self,
        scope: &'a AuthScope,
        request_id: &'a str,
    ) -> PlatformFuture<'a, Result<Option<CreateRow>, TransferError>> {
        Box::pin(async move {
            let row = self
                .db
                .prepare(
                    "SELECT * FROM transfer_requests
                     WHERE request_id = ? AND library_id = ? AND principal = ?",
                )
                .bind(&[
                    JsValue::from_str(request_id),
                    JsValue::from_str(&scope.library_id),
                    JsValue::from_str(&scope.principal),
                ])
                .map_err(platform)?
                .first::<CreateDbRow>(None)
                .await
                .map_err(platform)?;
            row.map(CreateDbRow::into_row).transpose()
        })
    }

    fn put_create<'a>(&'a self, row: &'a CreateRow) -> PlatformFuture<'a, Result<(), TransferError>> {
        Box::pin(async move {
            let statement = bind(
                self.db.prepare(
                    "INSERT OR IGNORE INTO transfer_requests
                     (request_id, library_id, principal, request_fingerprint,
                      session_id, receipt, created_at)
                     VALUES (?,?,?,?,?,?,?)",
                ),
                vec![
                    t(row.request_id.clone()),
                    t(row.scope.library_id.clone()),
                    t(row.scope.principal.clone()),
                    t(row.request_fingerprint.clone()),
                    opt(row.session_id.as_ref().map(|id| id.0.clone())),
                    opt_b64(&row.receipt),
                    t(rfc3339(row.created_at)),
                ],
            )?;
            changed(&statement.run().await.map_err(platform)?)?;
            Ok(())
        })
    }

    fn update_create<'a>(&'a self, row: &'a CreateRow) -> PlatformFuture<'a, Result<(), TransferError>> {
        Box::pin(async move {
            let statement = bind(
                self.db.prepare(
                    "UPDATE transfer_requests SET session_id = ?, receipt = ?
                     WHERE request_id = ? AND library_id = ? AND principal = ?
                       AND request_fingerprint = ? AND (session_id IS NULL OR session_id IS ?)",
                ),
                vec![
                    opt(row.session_id.as_ref().map(|id| id.0.clone())),
                    opt_b64(&row.receipt),
                    t(row.request_id.clone()),
                    t(row.scope.library_id.clone()),
                    t(row.scope.principal.clone()),
                    t(row.request_fingerprint.clone()),
                    opt(row.session_id.as_ref().map(|id| id.0.clone())),
                ],
            )?;
            if changed(&statement.run().await.map_err(platform)?)? == 0 {
                return Err(TransferError::IdempotencyConflict);
            }
            Ok(())
        })
    }

    // -- operations -----------------------------------------------------------

    fn operation<'a>(
        &'a self,
        session_id: &'a narou_rs::application::transfer::SessionId,
        operation_id: &'a narou_rs::application::transfer::OperationId,
    ) -> PlatformFuture<'a, Result<Option<OperationRow>, TransferError>> {
        Box::pin(async move {
            let row = self
                .db
                .prepare(
                    "SELECT * FROM transfer_operations WHERE session_id = ? AND operation_id = ?",
                )
                .bind(&[
                    JsValue::from_str(&session_id.0),
                    JsValue::from_str(&operation_id.0),
                ])
                .map_err(platform)?
                .first::<OperationDbRow>(None)
                .await
                .map_err(platform)?;
            row.map(OperationDbRow::into_row).transpose()
        })
    }

    fn begin_operation<'a>(&'a self,row:&'a OperationRow)->PlatformFuture<'a,Result<bool,TransferError>> {
        Box::pin(async move {
            let acquire=self.db.prepare("INSERT INTO transfer_execution_leases(session_id,owner,operation_id,expires_at) VALUES(?,?,?,unixepoch()+60) ON CONFLICT(session_id) DO UPDATE SET owner=excluded.owner,operation_id=excluded.operation_id,expires_at=excluded.expires_at WHERE transfer_execution_leases.expires_at<=unixepoch() OR transfer_execution_leases.owner=excluded.owner")
                .bind(&[row.session_id.0.as_str().into(),self.request_owner.as_str().into(),row.operation_id.0.as_str().into()]).map_err(platform)?;
            let insert=bind(self.db.prepare("INSERT OR IGNORE INTO transfer_operations(session_id,operation_id,kind,request_fingerprint,if_match_revision,result,completed,created_at) VALUES(?,?,?,?,?,?,?,?)"),vec![t(row.session_id.0.clone()),t(row.operation_id.0.clone()),t(row.kind.clone()),t(row.request_fingerprint.clone()),opt_u(row.if_match_revision),opt_b64(&row.result),i(row.completed as i64),t(rfc3339(row.created_at))])?;
            match self.db.batch(vec![acquire,self.execution_guard(&row.session_id)?,insert]).await {
                Ok(results)=>{for result in &results{changed(result)?;}self.claimed.lock().insert(row.session_id.0.clone());Ok(changed(&results[2])?>0)},
                Err(_)=>Err(TransferError::Busy{code:"operation_busy",message:"another invocation owns this session operation".into()}),
            }
        })
    }

    fn finish_operation<'a>(&'a self, row: &'a OperationRow) -> PlatformFuture<'a, Result<(), TransferError>> {
        Box::pin(async move {
            let statement = bind(
                self.db.prepare(
                    "UPDATE transfer_operations SET result = ?, completed = ?
                     WHERE session_id = ? AND operation_id = ?",
                ),
                vec![
                    opt_b64(&row.result),
                    i(row.completed as i64),
                    t(row.session_id.0.clone()),
                    t(row.operation_id.0.clone()),
                ],
            )?;
            self.guarded_batch(&row.session_id,vec![statement]).await?;
            Ok(())
        })
    }

    // -- files ---------------------------------------------------------------

    fn put_file<'a>(&'a self, row: &'a FileRow) -> PlatformFuture<'a, Result<(), TransferError>> {
        Box::pin(async move {
            let statement = bind(
                self.db.prepare(&format!(
                    "INSERT OR IGNORE INTO transfer_files ({FILE_COLUMNS}) VALUES ({})",
                    vec!["?"; 15].join(",")
                )),
                file_binds(row),
            )?;
            changed(&statement.run().await.map_err(platform)?)?;
            Ok(())
        })
    }

    fn file<'a>(
        &'a self,
        session_id: &'a narou_rs::application::transfer::SessionId,
        file_id: u32,
    ) -> PlatformFuture<'a, Result<Option<FileRow>, TransferError>> {
        Box::pin(async move {
            let row = self
                .db
                .prepare("SELECT * FROM transfer_files WHERE session_id = ? AND file_id = ?")
                .bind(&[
                    JsValue::from_str(&session_id.0),
                    JsValue::from_f64(file_id as f64),
                ])
                .map_err(platform)?
                .first::<FileDbRow>(None)
                .await
                .map_err(platform)?;
            row.map(FileDbRow::into_row).transpose()
        })
    }

    fn files<'a>(
        &'a self,
        session_id: &'a narou_rs::application::transfer::SessionId,
        cursor: Option<&'a str>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Page<FileRow>, TransferError>> {
        Box::pin(async move {
            let after: i64 = cursor.and_then(|c| c.parse().ok()).unwrap_or(-1);
            let result = self
                .db
                .prepare(
                    "SELECT * FROM transfer_files WHERE session_id = ? AND file_id > ?
                     ORDER BY file_id ASC LIMIT ?",
                )
                .bind(&[
                    JsValue::from_str(&session_id.0),
                    JsValue::from_f64(after as f64),
                    JsValue::from_f64(limit as f64 + 1.0),
                ])
                .map_err(platform)?
                .all()
                .await
                .map_err(platform)?;
            let mut rows = result.results::<FileDbRow>().map_err(platform)?;
            let next = (rows.len() > limit).then(|| rows[limit - 1].file_id.to_string());
            rows.truncate(limit);
            let mut items = Vec::with_capacity(rows.len());
            for row in rows {
                items.push(row.into_row()?);
            }
            byte_page(items,next,|row:&FileRow|row.file_id.to_string())
        })
    }

    fn reset_file<'a>(&'a self,row:&'a FileRow)->PlatformFuture<'a,Result<(),TransferError>> {
        Box::pin(async move {
            if row.confirmed_bytes!=0||row.next_chunk_index!=0||row.stage.is_some()||row.finalize_operation.is_some()||matches!(row.state,narou_rs::application::transfer::FileState::Verified|narou_rs::application::transfer::FileState::Reused|narou_rs::application::transfer::FileState::Finalizing){return Err(platform("invalid file restart state"));}
            let guard=bind(self.db.prepare("INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM transfer_files WHERE session_id=? AND file_id=? AND state IN ('pending','receiving','failed'))"),vec![t(row.session_id.0.clone()),u32v(row.file_id)])?;
            let count=bind(self.db.prepare("UPDATE transfer_sessions SET confirmed_bytes=confirmed_bytes-(SELECT confirmed_bytes FROM transfer_files WHERE session_id=? AND file_id=?),revision=revision+1,updated_at=?,expires_at=?,ext_json=CASE WHEN ?=4294967295 THEN json_set(ext_json,'$.manifest_confirmed',0) ELSE ext_json END WHERE session_id=?"),vec![t(row.session_id.0.clone()),u32v(row.file_id),t(rfc3339(Utc::now())),t(rfc3339(Utc::now()+chrono::Duration::seconds(24*3600))),u32v(row.file_id),t(row.session_id.0.clone())])?;
            let chunks=bind(self.db.prepare("DELETE FROM transfer_chunks WHERE session_id=? AND file_id=?"),vec![t(row.session_id.0.clone()),u32v(row.file_id)])?;
            let update=bind(self.db.prepare(&format!("INSERT OR REPLACE INTO transfer_files ({FILE_COLUMNS}) VALUES ({})",vec!["?";15].join(","))),file_binds(row))?;
            self.guarded_batch(&row.session_id,vec![guard,count,chunks,update]).await?;Ok(())
        })
    }

    fn update_file<'a>(&'a self, row: &'a FileRow) -> PlatformFuture<'a, Result<(), TransferError>> {
        Box::pin(async move {
            let mut values = file_binds(row);
            // Move PK binds to the WHERE clause.
            values.remove(0);
            values.remove(0);
            values.push(t(row.session_id.0.clone()));
            values.push(u32v(row.file_id));
            let statement = bind(
                self.db.prepare(&format!(
                    "UPDATE transfer_files SET {} WHERE session_id = ? AND file_id = ?",
                    "novel_ordinal=?, logical_path=?, role=?, size=?, sha256=?, state=?,
                     confirmed_bytes=?, next_chunk_index=?, confirmed_prefix_sha256=?,
                     hash_checkpoint=?, stage=?, pin=?, finalize_operation=?"
                )),
                values,
            )?;
            self.guarded_batch(&row.session_id,vec![statement]).await?;
            Ok(())
        })
    }

    fn pending_files<'a>(
        &'a self,
        session_id: &'a narou_rs::application::transfer::SessionId,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Vec<FileRow>, TransferError>> {
        Box::pin(async move {
            let result = self
                .db
                .prepare(
                    "SELECT * FROM transfer_files
                     WHERE session_id = ? AND state IN ('pending','receiving','finalizing','failed')
                     ORDER BY file_id ASC LIMIT ?",
                )
                .bind(&[
                    JsValue::from_str(&session_id.0),
                    JsValue::from_f64(limit as f64),
                ])
                .map_err(platform)?
                .all()
                .await
                .map_err(platform)?;
            result
                .results::<FileDbRow>()
                .map_err(platform)?
                .into_iter()
                .map(FileDbRow::into_row)
                .collect()
        })
    }

    fn finalizing_files<'a>(&'a self,session_id:&'a narou_rs::application::transfer::SessionId,limit:usize)->PlatformFuture<'a,Result<Vec<FileRow>,TransferError>>{
        Box::pin(async move{
            let rows=self.db.prepare("SELECT * FROM transfer_files WHERE session_id=? AND state='finalizing' ORDER BY file_id LIMIT ?").bind(&[session_id.0.as_str().into(),(limit.min(100)as f64).into()]).map_err(platform)?.all().await.map_err(platform)?;
            rows.results::<FileDbRow>().map_err(platform)?.into_iter().map(FileDbRow::into_row).collect()
        })
    }

    fn staged_files<'a>(
        &'a self,
        session_id: &'a narou_rs::application::transfer::SessionId,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Vec<FileRow>, TransferError>> {
        Box::pin(async move {
            let result = self
                .db
                .prepare(
                    "SELECT * FROM transfer_files
                     WHERE session_id = ? AND (stage IS NOT NULL OR pin IS NOT NULL)
                     ORDER BY file_id ASC LIMIT ?",
                )
                .bind(&[
                    JsValue::from_str(&session_id.0),
                    JsValue::from_f64(limit as f64),
                ])
                .map_err(platform)?
                .all()
                .await
                .map_err(platform)?;
            result
                .results::<FileDbRow>()
                .map_err(platform)?
                .into_iter()
                .map(FileDbRow::into_row)
                .collect()
        })
    }

    // -- chunks --------------------------------------------------------------

    fn chunk<'a>(
        &'a self,
        session_id: &'a narou_rs::application::transfer::SessionId,
        file_id: u32,
        chunk_index: u32,
    ) -> PlatformFuture<'a, Result<Option<ChunkRow>, TransferError>> {
        Box::pin(async move {
            let row = self
                .db
                .prepare(
                    "SELECT * FROM transfer_chunks
                     WHERE session_id = ? AND file_id = ? AND chunk_index = ?",
                )
                .bind(&[
                    JsValue::from_str(&session_id.0),
                    JsValue::from_f64(file_id as f64),
                    JsValue::from_f64(chunk_index as f64),
                ])
                .map_err(platform)?
                .first::<ChunkDbRow>(None)
                .await
                .map_err(platform)?;
            row.map(ChunkDbRow::into_row).transpose()
        })
    }

    fn commit_chunk<'a>(
        &'a self,
        chunk: &'a ChunkRow,
        file: &'a FileRow,
        _session_confirmed_bytes: u64,
    ) -> PlatformFuture<'a, Result<(), TransferError>> {
        Box::pin(async move {
            if chunk.session_id != file.session_id || chunk.file_id != file.file_id
                || file.confirmed_bytes != chunk.offset.checked_add(chunk.payload_size).ok_or_else(|| platform("byte count overflow"))?
                || file.next_chunk_index != chunk.chunk_index.checked_add(1).ok_or_else(|| platform("chunk index overflow"))? {
                return Err(platform("inconsistent chunk commit"));
            }
            if let Some(existing) = self.chunk(&chunk.session_id, chunk.file_id, chunk.chunk_index).await? {
                return if existing == *chunk { Ok(()) } else { Err(TransferError::ChunkConflict {
                    code: "chunk_conflict", message: "chunk identity already has different bytes".into(),
                }) };
            }
            // The first statement elects exactly one offset owner. A failed
            // election aborts the *entire* D1 batch, not just the file UPDATE.
            let insert = bind(self.db.prepare(
                "INSERT INTO transfer_chunks
                 (session_id,file_id,chunk_index,payload_size,declared_sha256,stored_sha256,prefix_sha256,hash_checkpoint,offset,is_final)
                 SELECT ?,?,?,?,?,?,?,?,?,? WHERE EXISTS(
                   SELECT 1 FROM transfer_files f JOIN transfer_sessions s USING(session_id)
                   WHERE f.session_id=? AND f.file_id=? AND f.next_chunk_index=?
                     AND f.confirmed_bytes=? AND s.state IN ('receiving_manifest','transferring'))",
            ), vec![t(chunk.session_id.0.clone()),u32v(chunk.file_id),u32v(chunk.chunk_index),
                u(chunk.payload_size),opt(chunk.declared_sha256.clone()),t(chunk.stored_sha256.clone()),
                t(chunk.prefix_sha256.clone()),opt_b64(&chunk.hash_checkpoint),u(chunk.offset),i(chunk.is_final as i64),
                t(chunk.session_id.0.clone()),u32v(chunk.file_id),u32v(chunk.chunk_index),u(chunk.offset)])?;
            let guard = self.db.prepare("INSERT INTO transfer_batch_guard(id) SELECT 1 WHERE changes()=0");
            let update = bind(self.db.prepare(
                "UPDATE transfer_files SET confirmed_bytes=?,next_chunk_index=?,confirmed_prefix_sha256=?,hash_checkpoint=?,stage=?,state=?,finalize_operation=?
                 WHERE session_id=? AND file_id=?",
            ), vec![u(file.confirmed_bytes),u32v(file.next_chunk_index),opt(file.confirmed_prefix_sha256.clone()),
                opt_b64(&file.hash_checkpoint),opt(file.stage.clone()),t(enum_name(&file.state)),opt(file.finalize_operation.as_ref().map(|operation|operation.0.clone())),
                t(file.session_id.0.clone()),u32v(file.file_id)])?;
            let count = bind(self.db.prepare(
                "UPDATE transfer_sessions SET confirmed_bytes=confirmed_bytes+?,revision=revision+1,updated_at=?,expires_at=?,
                 next_action=CASE WHEN ? IS NOT NULL THEN 'finalize_file' ELSE next_action END,
                 operation_id=COALESCE(?,operation_id),
                 ext_json=CASE WHEN ?=4294967295 THEN json_set(ext_json,'$.manifest_confirmed',CAST(? AS INTEGER)) ELSE ext_json END WHERE session_id=?",
            ), vec![u(chunk.payload_size),t(rfc3339(Utc::now())),
                t(rfc3339(Utc::now()+chrono::Duration::seconds(24*3600))),opt(file.finalize_operation.as_ref().map(|operation|operation.0.clone())),opt(file.finalize_operation.as_ref().map(|operation|operation.0.clone())),u32v(chunk.file_id),u(file.confirmed_bytes),t(chunk.session_id.0.clone())])?;
            self.ensure_execution(&chunk.session_id).await?;
            match self.db.batch(vec![self.execution_guard(&chunk.session_id)?,insert,guard,update,count]).await {
                Ok(results) => { for result in &results { changed(result)?; } Ok(()) }
                Err(error) => {
                    if self.chunk(&chunk.session_id,chunk.file_id,chunk.chunk_index).await? == Some(chunk.clone()) {
                        Ok(())
                    } else {
                        Err(TransferError::ChunkConflict { code: "unexpected_offset", message: format!("chunk CAS rejected: {error}") })
                    }
                }
            }
        })
    }

    // -- publications ----------------------------------------------------------

    fn put_publication<'a>(&'a self, row: &'a PublicationRow) -> PlatformFuture<'a, Result<(), TransferError>> {
        Box::pin(async move {
            let statement = bind(
                self.db.prepare(
                    "INSERT INTO transfer_publications
                     (session_id, publication_id, state, spec, phase_cursor,
                      assigned_id, receipt, error)
                     VALUES (?,?,?,?,?,?,?,?)
                     ON CONFLICT(session_id,publication_id) DO UPDATE SET
                     state=excluded.state,spec=excluded.spec,phase_cursor=excluded.phase_cursor,assigned_id=excluded.assigned_id,receipt=excluded.receipt,error=excluded.error
                     WHERE transfer_publications.state IN ('pending','skipped','rolled_back')",
                ),
                publication_binds(row),
            )?;
            self.guarded_batch(&row.session_id,vec![statement]).await?;
            Ok(())
        })
    }

    fn publication<'a>(
        &'a self,
        session_id: &'a narou_rs::application::transfer::SessionId,
        publication_id: u32,
    ) -> PlatformFuture<'a, Result<Option<PublicationRow>, TransferError>> {
        Box::pin(async move {
            let row = self
                .db
                .prepare(
                    "SELECT * FROM transfer_publications WHERE session_id = ? AND publication_id = ?",
                )
                .bind(&[
                    JsValue::from_str(&session_id.0),
                    JsValue::from_f64(publication_id as f64),
                ])
                .map_err(platform)?
                .first::<PublicationDbRow>(None)
                .await
                .map_err(platform)?;
            row.map(PublicationDbRow::into_row).transpose()
        })
    }

    fn update_publication<'a>(&'a self, row: &'a PublicationRow) -> PlatformFuture<'a, Result<(), TransferError>> {
        Box::pin(async move {
            let mut values = publication_binds(row);
            values.remove(0);
            values.remove(0);
            values.push(t(row.session_id.0.clone()));
            values.push(u32v(row.publication_id));
            values.push(t(enum_name(&row.state)));
            values.push(opt(row.phase_cursor.clone()));
            values.push(t(enum_name(&row.state)));
            let statement = bind(
                self.db.prepare(
                    "UPDATE transfer_publications SET
                        state = ?, spec = ?, phase_cursor = ?, assigned_id = ?,
                        receipt = ?, error = ?
                     WHERE session_id = ? AND publication_id = ?
                       AND NOT(state='committed' AND (?<>'committed' OR (COALESCE(phase_cursor,'')='finished' AND COALESCE(?,'')<>'finished')))
                       AND NOT(state IN ('rolled_back','skipped') AND state<>?)",
                ),
                values,
            )?;
            let count=bind(self.db.prepare("UPDATE transfer_sessions SET published_novels=published_novels+1 WHERE session_id=? AND ?='committed' AND ?='finished' AND EXISTS(SELECT 1 FROM transfer_publications WHERE session_id=? AND publication_id=? AND (state<>'committed' OR COALESCE(phase_cursor,'')<>'finished'))"),vec![t(row.session_id.0.clone()),t(enum_name(&row.state)),opt(row.phase_cursor.clone()),t(row.session_id.0.clone()),u32v(row.publication_id)])?;
            self.guarded_batch(&row.session_id,vec![count,statement]).await?;
            Ok(())
        })
    }

    fn pending_publications<'a>(
        &'a self,
        session_id: &'a narou_rs::application::transfer::SessionId,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Vec<PublicationRow>, TransferError>> {
        Box::pin(async move {
            let result = self
                .db
                .prepare(
                    "SELECT * FROM transfer_publications
                     WHERE session_id = ?
                       AND (state NOT IN ('committed','rolled_back','skipped') OR (state='committed' AND COALESCE(phase_cursor,'')<>'finished'))
                     ORDER BY publication_id ASC LIMIT ?",
                )
                .bind(&[
                    JsValue::from_str(&session_id.0),
                    JsValue::from_f64(limit as f64),
                ])
                .map_err(platform)?
                .all()
                .await
                .map_err(platform)?;
            result
                .results::<PublicationDbRow>()
                .map_err(platform)?
                .into_iter()
                .map(PublicationDbRow::into_row)
                .collect()
        })
    }

    fn active_publications<'a>(&'a self,session_id:&'a narou_rs::application::transfer::SessionId,limit:usize)->PlatformFuture<'a,Result<Vec<PublicationRow>,TransferError>>{
        Box::pin(async move{
            let rows=self.db.prepare("SELECT * FROM transfer_publications WHERE session_id=? AND (state NOT IN ('pending','skipped','rolled_back','committed') OR (state='committed' AND COALESCE(phase_cursor,'')<>'finished')) ORDER BY publication_id LIMIT ?")
                .bind(&[session_id.0.as_str().into(),(limit.min(100)as f64).into()]).map_err(platform)?.all().await.map_err(platform)?;
            rows.results::<PublicationDbRow>().map_err(platform)?.into_iter().map(PublicationDbRow::into_row).collect()
        })
    }

    fn publications<'a>(
        &'a self,
        session_id: &'a narou_rs::application::transfer::SessionId,
        cursor: Option<&'a str>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Page<PublicationRow>, TransferError>> {
        Box::pin(async move {
            let after: i64 = cursor.and_then(|c| c.parse().ok()).unwrap_or(-1);
            let result = self
                .db
                .prepare(
                    "SELECT * FROM transfer_publications
                     WHERE session_id = ? AND publication_id > ?
                     ORDER BY publication_id ASC LIMIT ?",
                )
                .bind(&[
                    JsValue::from_str(&session_id.0),
                    JsValue::from_f64(after as f64),
                    JsValue::from_f64(limit as f64 + 1.0),
                ])
                .map_err(platform)?
                .all()
                .await
                .map_err(platform)?;
            let mut rows = result.results::<PublicationDbRow>().map_err(platform)?;
            let next = (rows.len() > limit).then(|| rows[limit - 1].publication_id.to_string());
            rows.truncate(limit);
            let mut items = Vec::with_capacity(rows.len());
            for row in rows {
                items.push(row.into_row()?);
            }
            Ok(Page { items, next_cursor: next })
        })
    }

    // -- journal blobs ---------------------------------------------------------

    fn put_journal_entry<'a>(
        &'a self,
        session_id: &'a narou_rs::application::transfer::SessionId,
        publication_id: u32,
        key: &'a str,
        value: &'a [u8],
    ) -> PlatformFuture<'a, Result<(), TransferError>> {
        Box::pin(async move {
            if value.len() > 256 * 1024 {
                return Err(TransferError::ExceedsLimit { code: "metadata_budget", message: "journal record exceeds 256 KiB".into() });
            }
            let statement = bind(
                self.db.prepare(
                    "INSERT OR REPLACE INTO transfer_journal_entries
                     (session_id, publication_id, key, value) VALUES (?,?,?,?)",
                ),
                vec![
                    t(session_id.0.clone()),
                    u32v(publication_id),
                    t(key),
                    b64(value),
                ],
            )?;
            self.guarded_batch(session_id,vec![statement]).await?;
            Ok(())
        })
    }

    fn journal_entry<'a>(
        &'a self,
        session_id: &'a narou_rs::application::transfer::SessionId,
        publication_id: u32,
        key: &'a str,
    ) -> PlatformFuture<'a, Result<Option<Vec<u8>>, TransferError>> {
        Box::pin(async move {
            let row = self
                .db
                .prepare(
                    "SELECT value FROM transfer_journal_entries
                     WHERE session_id = ? AND publication_id = ? AND key = ?",
                )
                .bind(&[
                    JsValue::from_str(&session_id.0),
                    JsValue::from_f64(publication_id as f64),
                    JsValue::from_str(key),
                ])
                .map_err(platform)?
                .first::<String>(Some("value"))
                .await
                .map_err(platform)?;
            row.map(|v| b64_decode(&v)).transpose()
        })
    }


    fn delete_journal_entries<'a>(
        &'a self,
        session_id: &'a narou_rs::application::transfer::SessionId,
        publication_id: u32,
    ) -> PlatformFuture<'a, Result<(), TransferError>> {
        Box::pin(async move {
            self.db
                .prepare(
                    "DELETE FROM transfer_journal_entries
                     WHERE session_id = ? AND publication_id = ?",
                )
                .bind(&[
                    JsValue::from_str(&session_id.0),
                    JsValue::from_f64(publication_id as f64),
                ])
                .map_err(platform)?
                .run()
                .await
                .map_err(platform)?;
            Ok(())
        })
    }

    // -- plan / catalog --------------------------------------------------------

    fn put_plan_entries<'a>(
        &'a self,
        session_id: &'a narou_rs::application::transfer::SessionId,
        plan_revision: &'a str,
        entries: &'a [PlanEntry],
    ) -> PlatformFuture<'a, Result<(), TransferError>> {
        Box::pin(async move {
            let mut statements = Vec::with_capacity(entries.len());
            for entry in entries {
                statements.push(bind(
                    self.db.prepare(
                        "INSERT OR REPLACE INTO transfer_plan_entries
                         (session_id, novel_ordinal, plan_revision, toc_url, title, kind,
                          action, reason, existing_id, record_revision, content_revision,
                          membership_digest, file_count, total_bytes)
                         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                    ),
                    vec![
                        t(session_id.0.clone()),
                        u32v(entry.novel_ordinal),
                        t(plan_revision),
                        t(entry.toc_url.clone()),
                        t(entry.title.clone()),
                        t(enum_name(&entry.kind)),
                        t(enum_name(&entry.action)),
                        opt(entry.reason.clone()),
                        opt_i(entry.existing_id),
                        opt(entry.record_revision.clone()),
                        opt(entry.content_revision.clone()),
                        opt(entry.membership_digest.clone()),
                        u32v(entry.file_count),
                        u(entry.total_bytes),
                    ],
                )?);
            }
            if !statements.is_empty() {
                self.db.batch(statements).await.map_err(platform)?;
            }
            Ok(())
        })
    }

    fn plan_entries<'a>(
        &'a self,
        session_id: &'a narou_rs::application::transfer::SessionId,
        cursor: Option<&'a str>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Page<PlanEntry>, TransferError>> {
        Box::pin(async move {
            let after: i64 = cursor.and_then(|c| c.parse().ok()).unwrap_or(-1);
            let result = self
                .db
                .prepare(
                    "SELECT p.*,u.receipt FROM transfer_plan_entries p LEFT JOIN transfer_publications u ON u.session_id=p.session_id AND u.publication_id=p.novel_ordinal
                     WHERE p.session_id = ? AND p.novel_ordinal > ?
                     ORDER BY p.novel_ordinal ASC LIMIT ?",
                )
                .bind(&[
                    JsValue::from_str(&session_id.0),
                    JsValue::from_f64(after as f64),
                    JsValue::from_f64(limit as f64 + 1.0),
                ])
                .map_err(platform)?
                .all()
                .await
                .map_err(platform)?;
            let mut rows = result.results::<PlanDbRow>().map_err(platform)?;
            let next = (rows.len() > limit).then(|| rows[limit - 1].novel_ordinal.to_string());
            rows.truncate(limit);
            let mut items = Vec::with_capacity(rows.len());
            for row in rows {
                items.push(row.into_entry()?);
            }
            byte_page(items,next,|row:&PlanEntry|row.novel_ordinal.to_string())
        })
    }

    fn put_catalog_entries<'a>(
        &'a self,
        session_id: &'a narou_rs::application::transfer::SessionId,
        catalog_revision: u64,
        entries: &'a [CatalogItem],
    ) -> PlatformFuture<'a, Result<(), TransferError>> {
        Box::pin(async move {
            let mut statements = Vec::with_capacity(entries.len());
            for entry in entries {
                statements.push(bind(
                    self.db.prepare(
                        "INSERT OR REPLACE INTO transfer_catalog_entries
                         (session_id, novel_ordinal, logical_path, catalog_revision,
                          existing_id, toc_url, title, kind, role, size, selectable,
                          selected, required, exclusion_reason, policy_version, runtime_json, complete_json)
                         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                    ),
                    vec![
                        t(session_id.0.clone()),
                        u32v(entry.novel_ordinal),
                        t(entry.logical_path.clone()),
                        u(catalog_revision),
                        i(entry.existing_id),
                        t(entry.toc_url.clone()),
                        t(entry.title.clone()),
                        t(enum_name(&entry.kind)),
                        t(enum_name(&entry.role)),
                        u(entry.size),
                        i(entry.selectable as i64),
                        i(entry.selected as i64),
                        i(entry.required as i64),
                        opt(entry.exclusion_reason.clone()),
                        u32v(entry.policy_version),
                        t(serde_json::to_string(&entry.runtime).map_err(platform)?),
                        t(serde_json::to_string(&entry.complete).map_err(platform)?),
                    ],
                )?);
            }
            if !statements.is_empty() {
                self.db.batch(statements).await.map_err(platform)?;
            }
            Ok(())
        })
    }

    fn catalog_entries<'a>(
        &'a self,
        session_id: &'a narou_rs::application::transfer::SessionId,
        cursor: Option<&'a str>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Page<CatalogItem>, TransferError>> {
        Box::pin(async move { self.catalog_page(session_id, cursor, limit, false).await })
    }

    fn selected_catalog_entries<'a>(
        &'a self,
        session_id: &'a narou_rs::application::transfer::SessionId,
        cursor: Option<&'a str>,
        limit: usize,
    ) -> PlatformFuture<'a, Result<Page<CatalogItem>, TransferError>> {
        Box::pin(async move { self.catalog_page(session_id, cursor, limit, true).await })
    }

    fn selection_catalog_entries<'a>(&'a self,session_id:&'a narou_rs::application::transfer::SessionId,cursor:Option<&'a str>,limit:usize)->PlatformFuture<'a,Result<Page<CatalogItem>,TransferError>>{
        Box::pin(async move{
            let limit=limit.clamp(1,100);
            let rows=self.db.prepare("SELECT * FROM transfer_catalog_entries WHERE session_id=? AND (CAST(novel_ordinal AS TEXT)||char(0)||logical_path) COLLATE BINARY>? ORDER BY (CAST(novel_ordinal AS TEXT)||char(0)||logical_path) COLLATE BINARY LIMIT ?")
                .bind(&[session_id.0.as_str().into(),cursor.unwrap_or("").into(),((limit+1)as f64).into()]).map_err(platform)?.all().await.map_err(platform)?;
            let mut rows=rows.results::<CatalogDbRow>().map_err(platform)?;
            let next=(rows.len()>limit).then(||format!("{}\0{}",rows[limit-1].novel_ordinal,rows[limit-1].logical_path));rows.truncate(limit);
            let items=rows.into_iter().map(CatalogDbRow::into_item).collect::<Result<Vec<_>,_>>()?;
            byte_page(items,next,|row:&CatalogItem|format!("{}\0{}",row.novel_ordinal,row.logical_path))
        })
    }

    fn mark_catalog_selected<'a>(
        &'a self,
        session_id: &'a narou_rs::application::transfer::SessionId,
        keys: &'a [(u32, String)],
        selected: bool,
    ) -> PlatformFuture<'a, Result<(), TransferError>> {
        Box::pin(async move {
            let mut statements = Vec::with_capacity(keys.len());
            for (ordinal, path) in keys {
                statements.push(bind(
                    self.db.prepare(
                        "UPDATE transfer_catalog_entries SET selected = ?
                         WHERE session_id = ? AND novel_ordinal = ? AND logical_path = ?",
                    ),
                    vec![
                        i(selected as i64),
                        t(session_id.0.clone()),
                        u32v(*ordinal),
                        t(path.clone()),
                    ],
                )?);
            }
            if !statements.is_empty() {
                self.db.batch(statements).await.map_err(platform)?;
            }
            Ok(())
        })
    }
}

impl D1TransferJournal {
    async fn catalog_page(
        &self,
        session_id: &narou_rs::application::transfer::SessionId,
        cursor: Option<&str>,
        limit: usize,
        selected_only: bool,
    ) -> Result<Page<CatalogItem>, TransferError> {
        // Cursor is `novel_ordinal|logical_path` (opaque).
        let (after_ord, after_path) = cursor
            .and_then(|c| {
                let (a, b) = c.split_once('|')?;
                Some((a.parse::<i64>().ok()?, b.to_string()))
            })
            .unwrap_or((-1, String::new()));
        let filter = if selected_only { "AND selected = 1" } else { "" };
        let result = self
            .db
            .prepare(&format!(
                "SELECT * FROM transfer_catalog_entries
                 WHERE session_id = ? {filter}
                   AND (novel_ordinal > ? OR (novel_ordinal = ? AND logical_path > ?))
                 ORDER BY novel_ordinal ASC, logical_path ASC LIMIT ?"
            ))
            .bind(&[
                JsValue::from_str(&session_id.0),
                JsValue::from_f64(after_ord as f64),
                JsValue::from_f64(after_ord as f64),
                JsValue::from_str(&after_path),
                JsValue::from_f64(limit as f64 + 1.0),
            ])
            .map_err(platform)?
            .all()
            .await
            .map_err(platform)?;
        let mut rows = result.results::<CatalogDbRow>().map_err(platform)?;
        let next = (rows.len() > limit).then(|| {
            let row = &rows[limit - 1];
            format!("{}|{}", row.novel_ordinal, row.logical_path)
        });
        rows.truncate(limit);
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            items.push(row.into_item()?);
        }
        byte_page(items,next,|row:&CatalogItem|format!("{}|{}",row.novel_ordinal,row.logical_path))
    }
}

/// `state IN (...)` fragment listing every non-terminal `SessionState` wire
/// name — kept in one place so the live-session queries agree.
fn live_state_set() -> String {
    const LIVE: &[SessionState] = &[
        SessionState::ReceivingManifest,
        SessionState::Planning,
        SessionState::Transferring,
        SessionState::Ready,
        SessionState::Publishing,
        SessionState::Cataloguing,
        SessionState::Selecting,
        SessionState::Preparing,
        SessionState::Reading,
        SessionState::Paused,
        SessionState::Cancelling,
    ];
    LIVE.iter()
        .map(|state| format!("'{}'", enum_name(state)))
        .collect::<Vec<_>>()
        .join(",")
}


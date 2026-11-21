//! D1 → S3 の挿絵移行（P0d）。
//!
//! 保存先の振り分け (`SplitStore`) により、S3 へ移すのは**挿絵のバイナリだけ**。
//! 本文・メタデータは D1 に残る。移行ロジックは
//! `narou_rs::platform::store_migration::migrate_page`（core, テスト付き）にあり、
//! ここは D1/S3 を挿して進捗を `app_state` に残すだけ。
//!
//! 1 回の呼び出しは `limit` 件で区切り、進捗 (カーソルと件数) を `app_state` に
//! 残すので、途中で失敗しても続きから再開できる。移行後も D1 側は消さないので、
//! `asset_backend` を `d1` に戻せば即座に元の経路へ復帰できる。

use std::sync::Arc;
use std::rc::Rc;

use narou_rs::error::{NarouError, Result};
use narou_rs::platform::store_migration::{StoreMigrationState, migrate_page};
use narou_rs::platform::{AssetStore, ObjectStore, S3Store};
use worker::{Env, wasm_bindgen::JsValue};

use crate::db_handle::DbHandle;
use crate::d1_object_store::D1ObjectStore;

use narou_rs::application::novel_access::{NovelMigrationOrigins, NovelPrefixResolver, NovelMigrationStore, NovelReadScope};
use narou_rs::platform::{ObjectKey, ObjectPrefix, PlatformFuture};

/// Authoritative canonical prefix. Global/private keys are not novel content;
/// a novel key without its durable binding is an error, never an unguarded path.
pub(crate) async fn novel_prefix(db: &DbHandle, key: &ObjectKey) -> Result<Option<ObjectPrefix>> {
    if !key.as_ref().starts_with("novels/") { return Ok(None); }
    #[derive(serde::Deserialize)] struct PrefixRow { prefix: String }
    let row = db.primary_view().prepare(
        "SELECT prefix FROM novel_gate_bindings WHERE ?1=prefix OR substr(?1,1,length(prefix)+1)=prefix||'/' ORDER BY length(prefix) DESC LIMIT 1",
    ).bind(&[JsValue::from_str(key.as_ref())]).map_err(state_error)?
        .first::<PrefixRow>(None).await.map_err(state_error)?
        .ok_or_else(|| state_error(format!("novel gate binding is missing: {key}")))?;
    ObjectPrefix::new(row.prefix).map(Some)
}

/// One bounded writer-only registration page. The cursor belongs to the
/// repository's durable bootstrap state, so a new invocation resumes without
/// scanning the whole library.
pub(crate) async fn prepare_bindings(db: &DbHandle) -> Result<bool> {
    crate::d1_repository::D1NovelRepository::new(db.clone()).bootstrap_record_origins().await
}

struct PrefixResolver(DbHandle);

impl NovelPrefixResolver for PrefixResolver {
    fn novel_prefix<'a>(&'a self, key: &'a ObjectKey) -> PlatformFuture<'a, Result<Option<ObjectPrefix>>> {
        Box::pin(async move { novel_prefix(&self.0, key).await })
    }
}

pub(crate) fn migration_origins(db: &DbHandle) -> Result<Rc<NovelMigrationOrigins>> {
    NovelMigrationOrigins::new(
        Arc::new(D1ObjectStore::new(db.primary_view())),
        Arc::new(PrefixResolver(db.primary_view())),
    )
}

/// 進捗を置く `app_state` のキー（`scope='inv'`）。
const STATE_KEY: &str = "migrate_illustrations";

#[derive(Debug, Clone, serde::Serialize)]
pub struct MigrationReport {
    pub action: String,
    pub copied: u64,
    pub verified: u64,
    /// `plan` が数えた対象件数と合計バイト。
    pub planned: u64,
    pub planned_bytes: u64,
    pub failed: Vec<String>,
    pub done: bool,
    pub next_cursor: Option<String>,
}

/// `plan` / `copy` / `verify` を `limit` 件だけ進める。
///
/// `plan` は対象を数えるだけで書き込まない（本番前に件数と容量を確認する）。
pub async fn run(
    env: &Env,
    db: &DbHandle,
    action: &str,
    limit: usize,
    subrequests: crate::budget::SubrequestBudget,
) -> Result<MigrationReport> {
    let d1 = Arc::new(D1ObjectStore::new(db.primary_view()));
    let s3: Arc<S3Store> = crate::s3_store::store_from_env(env, subrequests)
        .await
        .map_err(|error| NarouError::Platform(error.to_string()))?;

    let mut state = load_state(db).await?;
    if action == "plan" {
        // This branch only enumerates metadata; it never opens old D1 bytes.
        let objects: Arc<dyn ObjectStore> = d1.clone();
        let assets: Arc<dyn AssetStore> = d1.clone();
        migrate_page(&objects, &assets, &(s3.clone() as Arc<dyn AssetStore>), &(s3 as Arc<dyn ObjectStore>), action, limit, &mut state).await?;
        save_state(db, &state).await?;
        return Ok(report(action, &state));
    }
    if !prepare_bindings(db).await? {
        state.done = false;
        return Ok(report(action, &state));
    }
    // Reads must resolve the current logical reference before considering a
    // retained legacy D1 object; a fresh epoch never authorizes stale bytes.
    let source = Arc::new(crate::content_store::TransferReferenceStore::new(
        db.primary_view(), d1.clone(), d1.clone(), d1.clone(), Some(s3.clone() as Arc<dyn AssetStore>),
    ));
    let target = Arc::new(crate::content_store::TransferReferenceStore::new(
        db.primary_view(), s3.clone(), s3.clone(), d1, Some(s3.clone() as Arc<dyn AssetStore>),
    ));
    let origins = migration_origins(db)?;
    let source_objects: Arc<dyn ObjectStore> = source.clone();
    let source_reads = NovelReadScope::new(&source_objects, "object-migration-source");
    let target_objects: Arc<dyn ObjectStore> = target.clone();
    let target_reads = NovelReadScope::new(&target_objects, "object-migration-target");
    let source = Arc::new(NovelMigrationStore::new(source.clone(), source, origins.clone(), source_reads.clone())?);
    let target = Arc::new(NovelMigrationStore::new(target.clone(), target, origins, target_reads.clone())?);
    let copied = migrate_page(
        &(source.clone() as Arc<dyn ObjectStore>),
        &(source.clone() as Arc<dyn AssetStore>),
        &(target.clone() as Arc<dyn AssetStore>),
        &(target.clone() as Arc<dyn ObjectStore>),
        action,
        limit,
        &mut state,
    )
    .await;
    drop(source); drop(target);
    let source_released = source_reads.release_all().await;
    let target_released = target_reads.release_all().await;
    copied?; source_released?; target_released?;
    save_state(db, &state).await?;
    Ok(report(action, &state))
}

/// Retry one durable physical-GC page while cron owns its memory permit.
/// Logical references and shared pool objects are never deleted here.
pub(crate) async fn collect_legacy_retirements(
    env: &Env, db: &DbHandle, subrequests: crate::budget::SubrequestBudget,
) -> Result<()> {
    const CURSOR: &str = "legacy_retirement_gc_cursor";
    let db = db.primary_view();
    let pending = db.prepare("SELECT EXISTS(SELECT 1 FROM legacy_object_retirements LIMIT 1) AS pending")
        .first::<i64>(Some("pending")).await.map_err(state_error)?
        .ok_or_else(|| state_error("legacy retirement inventory result missing"))?;
    if pending == 0 { return Ok(()); }
    let previous = db.prepare("SELECT value_json FROM app_state WHERE scope='inv' AND key=?")
        .bind(&[JsValue::from_str(CURSOR)]).map_err(state_error)?
        .first::<String>(Some("value_json")).await.map_err(state_error)?;
    let cursor: Option<String> = match previous.as_deref() {
        Some(raw) => serde_json::from_str(raw).map_err(state_error)?,
        None => None,
    };
    let s3 = crate::s3_store::store_from_env(env, subrequests).await.map_err(state_error)?;
    let store = crate::content_store::TransferReferenceStore::new(
        db.clone(), s3.clone(), s3.clone(), Arc::new(D1ObjectStore::new(db.clone())), Some(s3 as Arc<dyn AssetStore>),
    );
    let next = store.reclaim_legacy_retirements(cursor.as_deref(), 16).await?;
    let payload = serde_json::to_string(&next).map_err(state_error)?;
    db.prepare("INSERT INTO app_state(scope,key,value_json) VALUES('inv',?1,?2) ON CONFLICT(scope,key) DO UPDATE SET value_json=excluded.value_json WHERE app_state.value_json IS ?3")
        .bind(&[JsValue::from_str(CURSOR), JsValue::from_str(&payload), previous.as_deref().map(JsValue::from_str).unwrap_or(JsValue::NULL)])
        .map_err(state_error)?.run().await.map_err(state_error)?;
    Ok(())
}

/// 現在の進捗を返す。
pub async fn status(db: &DbHandle) -> Result<MigrationReport> {
    let state = load_state(db).await?;
    Ok(report("status", &state))
}

fn report(action: &str, state: &StoreMigrationState) -> MigrationReport {
    MigrationReport {
        action: action.to_string(),
        copied: state.copied,
        verified: state.verified,
        planned: state.planned,
        planned_bytes: state.planned_bytes,
        failed: state.failed.clone(),
        done: state.done,
        next_cursor: state.cursor.clone(),
    }
}

async fn load_state(db: &DbHandle) -> Result<StoreMigrationState> {
    let raw = db
        .prepare("SELECT value_json FROM app_state WHERE scope = 'inv' AND key = ?")
        .bind(&[JsValue::from_str(STATE_KEY)])
        .map_err(state_error)?
        .first::<String>(Some("value_json"))
        .await
        .map_err(state_error)?;
    match raw {
        Some(raw) => serde_json::from_str(&raw)
            .map_err(|error| NarouError::Platform(format!("invalid migration state: {error}"))),
        None => Ok(StoreMigrationState::default()),
    }
}

async fn save_state(db: &DbHandle, state: &StoreMigrationState) -> Result<()> {
    let payload = serde_json::to_string(state)
        .map_err(|error| NarouError::Platform(format!("cannot encode migration state: {error}")))?;
    db.prepare(
        "INSERT INTO app_state (scope, key, value_json) VALUES ('inv', ?, ?)
         ON CONFLICT(scope, key) DO UPDATE SET value_json = excluded.value_json",
    )
    .bind(&[
        JsValue::from_str(STATE_KEY),
        JsValue::from_str(&payload),
    ])
    .map_err(state_error)?
    .run()
    .await
    .map_err(state_error)?;
    Ok(())
}

fn state_error(error: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(format!("D1 {STATE_KEY} state: {error}"))
}

/// API 側が使う既定・上限件数 (core と同じ値)。
pub use narou_rs::platform::store_migration::DEFAULT_LIMIT;

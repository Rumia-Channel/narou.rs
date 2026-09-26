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

use narou_rs::error::{NarouError, Result};
use narou_rs::platform::store_migration::{StoreMigrationState, migrate_page};
use narou_rs::platform::{AssetStore, ObjectStore};
use worker::{D1Database, Env, wasm_bindgen::JsValue};

use crate::d1_object_store::D1ObjectStore;
use crate::s3_object_store::S3ObjectStore;

/// 進捗を置く `app_state` のキー（`scope='inv'`）。
const STATE_KEY: &str = "migrate_illustrations";

#[derive(Debug, Clone, serde::Serialize)]
pub struct MigrationReport {
    pub action: String,
    pub copied: u64,
    pub verified: u64,
    pub failed: Vec<String>,
    pub done: bool,
    pub next_cursor: Option<String>,
}

/// `copy` / `verify` を `limit` 件だけ進める。
pub async fn run(
    env: &Env,
    db: &Arc<D1Database>,
    action: &str,
    limit: usize,
) -> Result<MigrationReport> {
    let d1: Arc<dyn ObjectStore> = Arc::new(D1ObjectStore::new(db.clone()));
    let d1_assets: Arc<dyn AssetStore> = Arc::new(D1ObjectStore::new(db.clone()));
    let s3: Arc<S3ObjectStore> = Arc::new(
        S3ObjectStore::from_env(env)
            .await
            .map_err(|error| NarouError::Platform(error.to_string()))?,
    );

    let mut state = load_state(db).await?;
    migrate_page(
        &d1,
        &d1_assets,
        &(s3.clone() as Arc<dyn AssetStore>),
        &(s3 as Arc<dyn ObjectStore>),
        action,
        limit,
        &mut state,
    )
    .await?;
    save_state(db, &state).await?;
    Ok(report(action, &state))
}

/// 現在の進捗を返す。
pub async fn status(db: &Arc<D1Database>) -> Result<MigrationReport> {
    let state = load_state(db).await?;
    Ok(report("status", &state))
}

fn report(action: &str, state: &StoreMigrationState) -> MigrationReport {
    MigrationReport {
        action: action.to_string(),
        copied: state.copied,
        verified: state.verified,
        failed: state.failed.clone(),
        done: state.done,
        next_cursor: state.cursor.clone(),
    }
}

async fn load_state(db: &Arc<D1Database>) -> Result<StoreMigrationState> {
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

async fn save_state(db: &Arc<D1Database>, state: &StoreMigrationState) -> Result<()> {
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

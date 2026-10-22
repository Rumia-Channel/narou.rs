//! S3 挿絵の共通プール移行 (admin API)。
//!
//! 旧レイアウトの `<novel>/挿絵/<sha256-hex>.<ext>` を、全作品共通の
//! `illustrations/<base64url(sha256)>.<ext>` プールへ移す。移行はページ単位で
//! 進捗 (`app_state`) を残すので、HTTP の実行時間制限を跨いでも続きから再開できる。
//! 対象は hex 名のオブジェクトだけ (mitemin の `iNNN` などは index の対応を
//! 保ったままでは移せないため、新規取得分から hex 名になる)。

use std::sync::Arc;

use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{IllustrationDedupState, S3Store};
use worker::wasm_bindgen::JsValue;

use crate::db_handle::DbHandle;

/// 進捗を置く `app_state` のキー (`scope='inv'`)。
const STATE_KEY: &str = "illustration_dedup";
/// 1 回の API 呼び出しで走査する既定件数。
const DEFAULT_LIMIT: usize = 50;
/// 1 回の呼び出し上限。1 件あたり最大 4 subrequest (pool 確認 + 読取 + 書込 +
/// 旧配置削除) を使うため、Worker の subrequest 予算に収まる値にする。
const MAX_LIMIT: usize = 50;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct StoredProgress {
    /// 進行中の action (`plan` / `copy`)。action が変わったら進捗をリセットする。
    action: Option<String>,
    #[serde(flatten)]
    state: IllustrationDedupState,
}

#[derive(Debug, serde::Serialize)]
pub struct DedupReport {
    pub action: String,
    #[serde(flatten)]
    pub state: IllustrationDedupState,
}

/// `status` / `plan` / `copy` / `reset` を実行する。
pub async fn run(
    env: &worker::Env,
    db: &DbHandle,
    action: &str,
    limit: usize,
    subrequests: crate::budget::SubrequestBudget,
) -> Result<DedupReport> {
    let mut progress = load_progress(db).await?;
    match action {
        "status" | "reset" => {
            if action == "reset" {
                progress = StoredProgress::default();
                save_progress(db, &progress).await?;
            }
            Ok(report(action, progress.state))
        }
        "plan" | "copy" => {
            if progress.action.as_deref() != Some(action) {
                progress = StoredProgress {
                    action: Some(action.to_string()),
                    state: IllustrationDedupState::default(),
                };
            }
            let s3: Arc<S3Store> = crate::s3_store::store_from_env(env, subrequests)
                .await
                .map_err(|error| NarouError::Platform(error.to_string()))?;
            let limit = limit.clamp(1, MAX_LIMIT);
            narou_rs::platform::deduplicate_illustrations_page(
                s3.as_ref(),
                action,
                limit,
                &mut progress.state,
            )
            .await?;
            progress.state.trim_failures(20);
            save_progress(db, &progress).await?;
            Ok(report(action, progress.state))
        }
        other => Err(NarouError::Platform(format!(
            "unknown illustration dedup action: {other:?}"
        ))),
    }
}

/// `limit` 省略時の既定値。
pub fn default_limit() -> usize {
    DEFAULT_LIMIT
}

fn report(action: &str, state: IllustrationDedupState) -> DedupReport {
    DedupReport {
        action: action.to_string(),
        state,
    }
}

async fn load_progress(db: &DbHandle) -> Result<StoredProgress> {
    let raw = db
        .prepare("SELECT value_json FROM app_state WHERE scope = 'inv' AND key = ?")
        .bind(&[JsValue::from_str(STATE_KEY)])
        .map_err(state_error)?
        .first::<String>(Some("value_json"))
        .await
        .map_err(state_error)?;
    match raw {
        Some(raw) => serde_json::from_str(&raw).map_err(|error| {
            NarouError::Platform(format!("invalid illustration dedup state: {error}"))
        }),
        None => Ok(StoredProgress::default()),
    }
}

async fn save_progress(db: &DbHandle, progress: &StoredProgress) -> Result<()> {
    let payload = serde_json::to_string(progress)
        .map_err(|error| NarouError::Platform(format!("cannot encode dedup state: {error}")))?;
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

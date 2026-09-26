//! 一括行操作エンドポイント (native: `src/web/batch.rs` の
//! `batch_freeze_toggle` / `batch_freeze` / `batch_unfreeze` / `batch_remove`)。
//!
//! - `POST /api/freeze` — 凍結トグル。native は `{"success", "message",
//!   "count"}` を返す。
//! - `POST /api/novels/freeze` / `/api/novels/unfreeze` — 一括凍結/解除。
//!   native は `ApiResponse` (`{"success","message"}`) を返す。
//! - `POST /api/novels/remove` — 一括削除。`with_file: true` で本文・挿絵の
//!   オブジェクトも消す。native はローカル FS の削除をファイル削除として
//!   扱うが、Worker の実態はオブジェクトストア (D1 / S3) なので
//!   `NovelActionService::remove` の `ObjectStore` 経由削除に対応させる
//!   (composition 側で `SplitStore` が挿絵を含めて振り分ける)。オブジェクト
//!   削除に失敗した行がある場合は native と同じく `success: false` を返す。
//!
//! 入力本文は native `BatchIdsBody` と同じ (`{"ids": [i64],
//! "with_file"?: bool, "sort_state"?: _, "timestamp"?: _}`)。
//! native と同じく `sort_state` / `timestamp` は受理するが順序の決定には
//! 使わず、サーバー保存の `current_sort` (global スコープ) を唯一の真実源
//! にする (`request_preserves_input_order` / `requested_or_current_sort_state`
//! の「常にサーバー側を使う」実装に対応)。
//!
//! native の `push_server.broadcast_event("table.reload", ...)` は Worker に
//! broadcast 経路 (WebSocket push) が無いので送れない。フロントは
//! `postJson` の成功後に `refreshList()` を自前で呼ぶため実害は無い
//! (`ui/actions.js` の `batchAction` / 削除モーダル参照)。

use std::collections::HashSet;

use narou_rs::application::{ApplicationError, FileDeletionStatus, FreezeRequest, RemoveRequest};
use narou_rs::db::{NovelRecord, compare_records_by_key, sort_keys};
use narou_rs::platform::NovelId;
use narou_rs::setting_core::SettingScope;
use serde::Deserialize;
use worker::{Env, Method, Request, Response, console_log};

use crate::composition::WorkerRuntime;

/// native `crate::web::MAX_WEB_TARGETS_PER_REQUEST` — `server-max-targets-
/// per-request` 設定が無い/不正なときのフォールバック上限。
const MAX_WEB_TARGETS_PER_REQUEST: usize = 100_000;

/// native `sort_state.rs` の既定ソート (current_sort 未保存時)。
const DEFAULT_CURRENT_SORT_COLUMN: usize = 2;
const DEFAULT_CURRENT_SORT_DIR: &str = "desc";
/// native の `server_setting` 内キー名。Worker では global スコープの同名行。
const CURRENT_SORT_KEY: &str = "current_sort";

/// native `BatchIdsBody` (`src/web/state.rs`) と同じ受理形。
#[derive(Debug, Deserialize)]
struct BatchIdsBody {
    ids: Vec<i64>,
    #[serde(default)]
    with_file: Option<bool>,
    #[serde(default)]
    #[allow(dead_code)]
    sort_state: Option<serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    timestamp: Option<u64>,
}

/// Entry point; `lib.rs` routes the four POST endpoints here.
pub async fn handle(mut req: Request, env: Env) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    enum Route {
        FreezeToggle,
        Freeze,
        Unfreeze,
        Remove,
    }
    let path = req.path();
    let route = match (req.method(), path.as_str()) {
        (Method::Post, "/api/freeze") => Route::FreezeToggle,
        (Method::Post, "/api/novels/freeze") => Route::Freeze,
        (Method::Post, "/api/novels/unfreeze") => Route::Unfreeze,
        (Method::Post, "/api/novels/remove") => Route::Remove,
        (
            _,
            "/api/freeze"
            | "/api/novels/freeze"
            | "/api/novels/unfreeze"
            | "/api/novels/remove",
        ) => return json_error(405, "method_not_allowed", None),
        _ => {
            return json_error(404, "not_found", Some("route is not handled by this Worker"));
        }
    };

    // axum `Json` と同じく、本文が壊れていればここで 400。
    let body: BatchIdsBody = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };

    let runtime = match WorkerRuntime::build(&env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("service composition failed: {error}");
            return json_error(503, "service_unavailable", None);
        }
    };

    match route {
        Route::FreezeToggle => freeze_toggle(&runtime, &body).await,
        Route::Freeze => batch_freeze(&runtime, &body, true).await,
        Route::Unfreeze => batch_freeze(&runtime, &body, false).await,
        Route::Remove => batch_remove(&runtime, &body).await,
    }
}

/// `lib.rs::json_error` と同じ JSON 形 (`{error: {code, message?}}`)。あちらは
/// private なので形だけ合わせてここに持つ。
fn json_error(status: u16, code: &str, message: Option<&str>) -> worker::Result<Response> {
    let payload = match message {
        Some(message) => serde_json::json!({ "error": { "code": code, "message": message } }),
        None => serde_json::json!({ "error": { "code": code } }),
    };
    Response::from_json(&payload).map(|response| response.with_status(status))
}

/// native `ApiResponse` と同じ形 (`{success, message}`)。
fn api_response(success: bool, message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({
        "success": success,
        "message": message.into(),
    })
}

/// native `map_application_error` と同じステータス対応。応答本体は Worker
/// 共通の `json_error` 形 (native は文字列ボディだが、クライアントは非 2xx
/// をテキストとして読むのでメッセージ本文は native の文字列をそのまま乗せる)。
fn application_error(error: &ApplicationError) -> (u16, &'static str, String) {
    match error {
        ApplicationError::InvalidRequest(message) => (400, "bad_request", message.clone()),
        ApplicationError::NotFound(message) => (404, "not_found", message.clone()),
        ApplicationError::Platform(message) => (500, "internal_error", message.clone()),
    }
}

/// native `ensure_no_missing`: 先頭の missing id を 404 にする。
fn missing_error(missing: &[NovelId]) -> Option<worker::Result<Response>> {
    missing
        .first()
        .map(|id| json_error(404, "not_found", Some(&format!("ID: {}", id.0))))
}

// ---------------------------------------------------------------------------
// sort_state (native `src/web/sort_state.rs` のサーバー側ソート)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct CurrentSortState {
    column: usize,
    dir: String,
}

fn default_current_sort_state() -> CurrentSortState {
    CurrentSortState {
        column: DEFAULT_CURRENT_SORT_COLUMN,
        dir: DEFAULT_CURRENT_SORT_DIR.to_string(),
    }
}

/// native `normalize_current_sort_value` と同じ受理形式: `column`/`dir` キー
/// (Ruby シンボル由来の `:column`/`:dir` も許容)、列は番号または数字文字列、
/// 方向は `asc`/`desc` (先頭 `:` は剥がす)。
fn normalize_current_sort_value(sort_state: &serde_yaml::Value) -> Option<CurrentSortState> {
    let sort_state = sort_state.as_mapping()?;
    let column = sort_state
        .get(serde_yaml::Value::String("column".to_string()))
        .or_else(|| sort_state.get(serde_yaml::Value::String(":column".to_string())))
        .and_then(normalize_sort_column)?;
    let dir = sort_state
        .get(serde_yaml::Value::String("dir".to_string()))
        .or_else(|| sort_state.get(serde_yaml::Value::String(":dir".to_string())))
        .and_then(normalize_sort_dir)?;
    Some(CurrentSortState { column, dir })
}

fn normalize_sort_column(value: &serde_yaml::Value) -> Option<usize> {
    let column = match value {
        serde_yaml::Value::Number(number) => number.as_u64().map(|value| value as usize)?,
        serde_yaml::Value::String(text) if text.chars().all(|ch| ch.is_ascii_digit()) => {
            text.parse::<usize>().ok()?
        }
        _ => return None,
    };
    sort_keys().get(column).map(|_| column)
}

fn normalize_sort_dir(value: &serde_yaml::Value) -> Option<String> {
    let text = match value {
        serde_yaml::Value::String(text) => text.as_str(),
        _ => return None,
    };
    let text = text.trim_start_matches(':');
    match text {
        "asc" | "desc" => Some(text.to_string()),
        _ => None,
    }
}

/// native `load_current_sort_state`: `server_setting` (global) の
/// `current_sort` キーを読み、無ければ既定値。Worker では global スコープの
/// `current_sort` 行がその値そのもの (`webui::ui_prefs` の保存先と同じ)。
async fn load_current_sort_state(runtime: &WorkerRuntime) -> CurrentSortState {
    runtime
        .services
        .settings
        .get_raw(SettingScope::Global, CURRENT_SORT_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|value| normalize_current_sort_value(&value))
        .unwrap_or_else(default_current_sort_state)
}

/// native `sort_records` と同じ比較: 主キーが Equal のとき id で安定化し、
/// `desc` なら反転する。
fn sort_records(records: &mut [NovelRecord], sort_state: &CurrentSortState) {
    let sort_key = sort_keys().get(sort_state.column).copied().unwrap_or("id");
    let reverse = sort_state.dir == "desc";
    records.sort_by(|a, b| {
        let ordering = compare_records_by_key(a, b, sort_key).then_with(|| a.id.cmp(&b.id));
        if reverse { ordering.reverse() } else { ordering }
    });
}

/// native `sort_ids_for_request` → `sort_ids_from_records` の写し。
/// 要求が送ってくる sort_state/timestamp は native と同じく無視し
/// (`request_preserves_input_order` は常に false)、選択 id を現在の
/// サーバーソート順に並べ直す。選択外のレコードは捨てる。
async fn sort_ids_for_request(runtime: &WorkerRuntime, ids: &[i64]) -> Vec<i64> {
    let sort_state = load_current_sort_state(runtime).await;
    let records = runtime
        .services
        .library
        .records()
        .await
        .unwrap_or_default();
    let selected: HashSet<i64> = ids.iter().copied().collect();
    let mut records = records
        .into_iter()
        .filter(|record| selected.contains(&record.id))
        .collect::<Vec<_>>();
    sort_records(&mut records, &sort_state);
    records.into_iter().map(|record| record.id).collect()
}

/// `super::max_web_targets_per_request` parity: 設定ポート経由で上限を解決。
async fn max_targets(runtime: &WorkerRuntime) -> usize {
    runtime
        .services
        .settings
        .web_target_limit(MAX_WEB_TARGETS_PER_REQUEST)
        .await
}

// ---------------------------------------------------------------------------
// POST /api/freeze — native `batch_freeze_toggle`
// ---------------------------------------------------------------------------

async fn freeze_toggle(
    runtime: &WorkerRuntime,
    body: &BatchIdsBody,
) -> worker::Result<Response> {
    if body.ids.len() > max_targets(runtime).await {
        return json_error(400, "bad_request", Some("too many ids"));
    }
    let ids = sort_ids_for_request(runtime, &body.ids).await;
    let ids: Vec<NovelId> = ids.iter().copied().map(Into::into).collect();
    let result = match runtime.services.novel_actions.toggle_freeze(&ids).await {
        Ok(result) => result,
        Err(error) => {
            let (status, code, message) = application_error(&error);
            return json_error(status, code, Some(&message));
        }
    };
    if let Some(response) = missing_error(&result.missing) {
        return response;
    }
    Response::from_json(&serde_json::json!({
        "success": result.store_failed.is_empty(),
        "message": "凍結状態を切り替えました",
        "count": body.ids.len(),
    }))
}

// ---------------------------------------------------------------------------
// POST /api/novels/freeze|unfreeze — native `batch_freeze` / `batch_unfreeze`
// ---------------------------------------------------------------------------

async fn batch_freeze(
    runtime: &WorkerRuntime,
    body: &BatchIdsBody,
    freeze: bool,
) -> worker::Result<Response> {
    if body.ids.len() > max_targets(runtime).await {
        return json_error(400, "bad_request", Some("too many ids"));
    }
    let ids = sort_ids_for_request(runtime, &body.ids).await;
    let result = match runtime
        .services
        .novel_actions
        .apply_freeze(&FreezeRequest {
            ids: ids.iter().copied().map(Into::into).collect(),
            freeze,
        })
        .await
    {
        Ok(result) => result,
        Err(error) => {
            let (status, code, message) = application_error(&error);
            return json_error(status, code, Some(&message));
        }
    };
    if let Some(response) = missing_error(&result.missing) {
        return response;
    }
    let message = if freeze {
        format!("Froze {} novels", ids.len())
    } else {
        format!("Unfroze {} novels", ids.len())
    };
    Response::from_json(&api_response(result.store_failed.is_empty(), message))
}

// ---------------------------------------------------------------------------
// POST /api/novels/remove — native `batch_remove`
// ---------------------------------------------------------------------------

async fn batch_remove(runtime: &WorkerRuntime, body: &BatchIdsBody) -> worker::Result<Response> {
    if body.ids.len() > max_targets(runtime).await {
        return json_error(400, "bad_request", Some("too many ids"));
    }
    let with_file = body.with_file.unwrap_or(false);
    let ids = sort_ids_for_request(runtime, &body.ids).await;
    let result = match runtime
        .services
        .novel_actions
        .remove(&RemoveRequest {
            ids: ids.iter().copied().map(Into::into).collect(),
            delete_files: with_file,
        })
        .await
    {
        Ok(result) => result,
        Err(error) => {
            let (status, code, message) = application_error(&error);
            return json_error(status, code, Some(&message));
        }
    };
    if let Some(response) = missing_error(&result.missing) {
        return response;
    }
    // native `batch_remove`: ファイル削除が全部成功したかだけを `success` に
    // 載せる。Worker の「ファイル」はオブジェクトストア上のオブジェクト群
    // なので、Deleted 以外 (Unavailable / Failed) はここで失敗として見える。
    let files_ok = result
        .files
        .iter()
        .all(|(_, status)| matches!(status, FileDeletionStatus::Deleted));
    Response::from_json(&api_response(
        files_ok,
        format!("Removed {} novels", ids.len()),
    ))
}

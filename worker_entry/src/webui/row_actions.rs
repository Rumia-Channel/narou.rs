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

use narou_rs::application::{ApplicationError, FileDeletionStatus, FreezeRequest, RemoveRequest};
use narou_rs::platform::NovelId;
use serde::Deserialize;
use worker::{Env, Method, Request, Response, console_log};

use crate::composition::WorkerRuntime;

use super::{api_response, json_error, load_current_sort_state};

// ソート状態の型・正規化・レコード比較は `narou_rs::application::webui` の共有実装。
use narou_rs::application::webui::sort_ids_from_records;

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

/// native `map_application_error` と同じステータス対応 (`webui::mod` の共有実装)。
/// 応答本体は Worker 共通の `json_error` 形 (native は文字列ボディだが、
/// クライアントは非 2xx をテキストとして読むのでメッセージ本文は native の
/// 文字列をそのまま乗せる)。
fn application_error(error: &ApplicationError) -> (u16, &'static str, String) {
    super::application_error(error)
}

/// native `ensure_no_missing`: 先頭の missing id を 404 にする。
fn missing_error(missing: &[NovelId]) -> Option<worker::Result<Response>> {
    missing
        .first()
        .map(|id| json_error(404, "not_found", Some(&format!("ID: {}", id.0))))
}

// ---------------------------------------------------------------------------
// sort_state (native `src/web/sort_state.rs` のサーバー側ソート)
// 型・正規化・比較は `narou_rs::application::webui` の共有実装を使う。
// ---------------------------------------------------------------------------

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
    sort_ids_from_records(ids, &records, &sort_state)
}

// ---------------------------------------------------------------------------
// POST /api/freeze — native `batch_freeze_toggle`
// ---------------------------------------------------------------------------

async fn freeze_toggle(
    runtime: &WorkerRuntime,
    body: &BatchIdsBody,
) -> worker::Result<Response> {
    if body.ids.len() > super::max_web_targets(runtime).await {
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
        // native `batch_freeze_toggle` はソート後の実在 id 件数を返す
        // (リクエスト件数ではない — 存在しない id はソートで落ちる)。
        "count": ids.len(),
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
    if body.ids.len() > super::max_web_targets(runtime).await {
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
    if body.ids.len() > super::max_web_targets(runtime).await {
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

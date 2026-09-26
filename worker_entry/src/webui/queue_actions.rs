//! Web UI のキュー操作系エンドポイント (D1 台帳 `worker_jobs` ベース)。
//!
//! JSON parity with the native handlers in `src/web/jobs.rs`:
//! - `queue_clear`      → POST /api/queue/clear
//! - `api_cancel`       → POST /api/cancel (Ruby parity: running only,
//!                        pending は消さない。`/api/queue/cancel` は未割当)
//! - `cancel_running_task` → POST /api/cancel_running_task
//! - `remove_pending_task` → POST /api/remove_pending_task
//! - `restore_pending_tasks` → POST /api/restore_pending_tasks
//! - `defer_restore_pending_tasks` → POST /api/defer_restore_pending_tasks
//! - `reorder_pending_tasks` → POST /api/reorder_pending_tasks (501)
//!
//! 台帳セマンティクス (`ledger.rs` の "Web UI queue mutations" を参照):
//! - 「キューから外す」は行削除ではなく `permanent` tombstone。Cloudflare
//!   Queue は送信済みメッセージを保持するので、行を消すと配信時に
//!   `record_rejected` が `kind='unknown'` の phantom `blocked` 行を挿入して
//!   しまう。tombstone なら再配信は `AlreadyTerminal` → ack で解決し、
//!   台帳の dedupe index からも外れる (native 同様、同じ対象を即再投入可)。
//!   副作用として Web UI では `failed` バケットに数えられる — 第 1 波の
//!   写像 (`permanent` + `blocked` → `failed`) と矛盾しない唯一の状態。
//! - running job の中断は Wasm を途中で殺せないが、execution_token を消す
//!   tombstone により「結果を記録できないジョブ」になる (native の
//!   SIGKILL と同じ終了状態: mark_terminal / yield / record_attempt は
//!   全て失敗し、再配信は即 ack)。
//! - native の restorable tasks (停止時に running だったジョブ) に相当する
//!   のは、実行中の Worker が失われて lease 切れになった `running` 行。
//!   `claim` の lease 期限分岐が回収できるので、envelope を再送して復元する。

use narou_rs::application::{JobId, WorkerJobEnvelope};
use serde::Deserialize;
use serde_json::json;
use worker::{console_log, Env, Method, Request, Response};

use crate::composition::WorkerRuntime;

use super::json_error;

/// `cancel_pending_task` などが `last_error` に残す理由 (台帳の記録用、
/// Web UI の失敗理由表示と同じく英語の機械可読文字列)。
const REASON_REMOVED: &str = "removed from queue by user via Web UI";
const REASON_QUEUE_CLEARED: &str = "queue cleared by user via Web UI";
const REASON_CANCELLED: &str = "cancelled by user via Web UI";

/// `src/web/state.rs::TaskIdBody` と同じ形 (`{"task_id": "..."}`)。
#[derive(Debug, Deserialize)]
struct TaskIdBody {
    task_id: String,
}

/// `src/web/state.rs::ReorderBody` と同じ形。リクエストの検証 (native の
/// `Json<ReorderBody>` による 4xx 拒否と同じタイミング) のためだけに使う;
/// 並べ替え自体は Worker では実現不能 (下の `reorder_pending_tasks` 参照)。
#[derive(Debug, Deserialize)]
struct ReorderBody {
    #[allow(dead_code)]
    task_ids: Vec<String>,
}

/// Entry point; `lib.rs` routes the queue-action paths here.
pub async fn handle(req: Request, env: Env) -> worker::Result<Response> {
    match req.path().as_str() {
        "/api/queue/clear" => queue_clear(req, env).await,
        "/api/cancel" => cancel(req, env).await,
        "/api/cancel_running_task" => cancel_running_task(req, env).await,
        "/api/remove_pending_task" => remove_pending_task(req, env).await,
        "/api/restore_pending_tasks" => restore_pending_tasks(req, env).await,
        "/api/reorder_pending_tasks" => reorder_pending_tasks(req, env).await,
        "/api/defer_restore_pending_tasks" => defer_restore_pending_tasks(req, env).await,
        _ => Response::error("Not Found", 404),
    }
}

/// native `ApiResponse` (`{success, message}`)。成功時も失敗時も HTTP 200 —
/// `queue_clear` / `remove_pending_task` はこの形で返す。
fn api_response(success: bool, message: &str) -> worker::Result<Response> {
    Response::from_json(&super::api_response(success, message))
}

fn api_failure(message: impl std::fmt::Display) -> worker::Result<Response> {
    api_response(false, &message.to_string())
}

/// Build the runtime or finish with the native-style 503 error response
/// (`webui/download.rs` と同じ形)。
macro_rules! runtime_or_503 {
    ($env:expr) => {
        match WorkerRuntime::build(&$env).await {
            Ok(runtime) => runtime,
            Err(error) => {
                console_log!("service composition failed: {error}");
                return json_error(503, "service_unavailable", None);
            }
        }
    };
}

// ---------------------------------------------------------------------------
// POST /api/queue/clear — native `queue_clear` (`clear_non_running`)
// ---------------------------------------------------------------------------

async fn queue_clear(req: Request, env: Env) -> worker::Result<Response> {
    if req.method() != Method::Post {
        return json_error(405, "method_not_allowed", None);
    }
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    let runtime = runtime_or_503!(env);

    // `clear_non_running`: pending (=`pending` + `retryable`) を全部外す;
    // running は残す。native と同じく、外れた分は実行も履歴にも残らない
    // (worker 側は `permanent` tombstone → `failed` バケットに出る差分あり)。
    if let Err(error) = runtime.ledger.cancel_all_pending(REASON_QUEUE_CLEARED).await {
        return api_failure(error);
    }
    let running_count = match runtime.ledger.running_job_ids().await {
        Ok(ids) => ids.len(),
        Err(_) => return json_error(500, "ledger_read_failed", None),
    };
    api_response(
        true,
        if running_count > 0 {
            "Queue cleared (running tasks kept)"
        } else {
            "Queue cleared"
        },
    )
}

// ---------------------------------------------------------------------------
// POST /api/cancel — native `api_cancel`: running を中断 (pending は残す)
// ---------------------------------------------------------------------------

async fn cancel(req: Request, env: Env) -> worker::Result<Response> {
    if req.method() != Method::Post {
        return json_error(405, "method_not_allowed", None);
    }
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    let runtime = runtime_or_503!(env);

    // `kill_running_child` 相当: 実行中ジョブを tombstone して結果を記録
    // 不能にする。native はパイプライン上の共有プロセスを殺すので lane 単位
    // だったが、worker の各ジョブは独立した実行なので関連ジョブを道連れに
    // する必要はない — ここは native 同様「全 running」を対象にする。
    let running_ids = match runtime.ledger.running_job_ids().await {
        Ok(ids) => ids,
        Err(_) => return json_error(500, "ledger_read_failed", None),
    };
    for job_id in running_ids {
        // lease 切れの orphan も tombstone しておくと将来の再配信を止められる。
        if let Err(error) = runtime.ledger.cancel_running_job(&job_id, REASON_CANCELLED).await {
            return api_failure(error);
        }
    }
    api_response(true, "キャンセルしました")
}

// ---------------------------------------------------------------------------
// POST /api/cancel_running_task — native `cancel_running_task`
// ---------------------------------------------------------------------------

async fn cancel_running_task(mut req: Request, env: Env) -> worker::Result<Response> {
    if req.method() != Method::Post {
        return json_error(405, "method_not_allowed", None);
    }
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    let body: TaskIdBody = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };
    let runtime = runtime_or_503!(env);

    match runtime
        .ledger
        .cancel_running_job(&body.task_id, REASON_CANCELLED)
        .await
    {
        Ok(true) => Response::from_json(&json!({ "status": "ok" })),
        Ok(false) => Response::from_json(
            &json!({ "error": "実行中の処理を中断できませんでした" }),
        ),
        Err(error) => api_failure(error),
    }
}

// ---------------------------------------------------------------------------
// POST /api/remove_pending_task — native `remove_pending_task` (`remove_pending`)
// ---------------------------------------------------------------------------

async fn remove_pending_task(mut req: Request, env: Env) -> worker::Result<Response> {
    if req.method() != Method::Post {
        return json_error(405, "method_not_allowed", None);
    }
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    let body: TaskIdBody = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };
    let runtime = runtime_or_503!(env);

    // `remove_pending` parity: pending に無い id (running / 終了済み / 未知)
    // は `Ok(false)` → `success:false` で返す。
    match runtime
        .ledger
        .cancel_pending_job(&body.task_id, REASON_REMOVED)
        .await
    {
        Ok(true) => api_response(true, "Task removed"),
        Ok(false) => api_response(false, "キューから削除できませんでした"),
        Err(error) => api_failure(error),
    }
}

// ---------------------------------------------------------------------------
// POST /api/restore_pending_tasks — native `restore_pending_tasks`
// (`activate_restorable_tasks`)
// ---------------------------------------------------------------------------

async fn restore_pending_tasks(req: Request, env: Env) -> worker::Result<Response> {
    if req.method() != Method::Post {
        return json_error(405, "method_not_allowed", None);
    }
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    let runtime = runtime_or_503!(env);

    // native は停止時に `running` のままディスクに残ったジョブを pending に
    // 戻す。worker で同じ状態にあるのは、実行中の isolate が失われて lease
    // 切れ (または lease 未記録) になった `running` 行だけ。これらは
    // `claim` の lease 期限分岐が回収できるので envelope を再送する。
    // pending/retryable 行は再送不要 — それらのメッセージは Queue 側に
    // 生存中か、配信が消えたか区別できないため、無条件再送は二重投入に
    // なり得る (dedupe は active 行を見るので Ledger 側は防げても Queue 側
    // の順序は汚れる)。
    let stale_ids = match runtime.ledger.stale_running_job_ids().await {
        Ok(ids) => ids,
        Err(error) => {
            return Response::from_json(&json!({ "error": error.to_string() }));
        }
    };
    for job_id in &stale_ids {
        let envelope = WorkerJobEnvelope::v2(JobId::from(job_id.clone()));
        if let Err(error) = runtime.queue.send(&envelope).await {
            // native parity: 失敗は `{error: <string>}` を 200 で返す。
            return Response::from_json(&json!({
                "error": format!("queue send failed: {error}"),
            }));
        }
    }
    Response::from_json(&json!({ "status": "ok", "count": stale_ids.len() }))
}

// ---------------------------------------------------------------------------
// POST /api/defer_restore_pending_tasks — native `defer_restorable_tasks`
// ---------------------------------------------------------------------------

async fn defer_restore_pending_tasks(req: Request, env: Env) -> worker::Result<Response> {
    if req.method() != Method::Post {
        return json_error(405, "method_not_allowed", None);
    }
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    // native は「後で復元する」フラグを立てるだけでジョブ自体は動かさない。
    // worker 側に deferred 集合は無く、lease 切れの running 行はいつでも
    // restore 可能なので、状態を持たないこの環境では承諾だけを返すのが
    // 正直な応答 (native 同様 `{"status": "ok"}`)。
    Response::from_json(&json!({ "status": "ok" }))
}

// ---------------------------------------------------------------------------
// POST /api/reorder_pending_tasks — 実現不能 (501)
// ---------------------------------------------------------------------------

async fn reorder_pending_tasks(mut req: Request, env: Env) -> worker::Result<Response> {
    if req.method() != Method::Post {
        return json_error(405, "method_not_allowed", None);
    }
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    // native 同様、まず body の形だけ検証する (不正 JSON はここで 400)。
    let _body: ReorderBody = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };

    // 実行順序は Cloudflare Queue のメッセージ順であり、台帳側からは
    // 変更できない。`created_at` を書き換えて表示順だけ変えるのは
    // (実行順と表示が食い違うので) 成功の偽装に当たる — 並べ替えと再投入
    // は新しい job_id と追加メッセージを要求し、tombstone と衝突する。
    // よって `webui/library_backup.rs` 同様、成功を偽装せず 501 で返す。
    json_error(
        501,
        "queue_reorder_not_supported",
        Some(
            "reordering pending tasks requires control over Cloudflare Queue \
             message order, which the worker does not have; queue order is \
             fixed at send time",
        ),
    )
}

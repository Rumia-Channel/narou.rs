//! Web UI queue/tag endpoints backed by the D1 job ledger (`worker_jobs`).
//!
//! JSON parity with the native handlers:
//! - `src/web/misc.rs` `tag_list`
//! - `src/web/jobs.rs` `queue_status` / `get_pending_tasks`
//!
//! Status mapping (native `PersistentQueue` → worker ledger):
//! - `pending` ← `pending` + `retryable` (both wait for execution; native
//!   failed-retry jobs stay in the pending set, so retryable ledger rows are
//!   the same thing for the UI)
//! - `running` ← `running`
//! - `completed` ← `succeeded`
//! - `partial` ← `partial`
//! - `cancelled` ← none (the worker ledger has no cancelled state)
//! - `failed` ← `permanent` + `blocked` (both are terminal states that need
//!   the operator to look at them; the UI only has a failed bucket)
//!
//! The ledger does not expose listing primitives (`JobQueue` is per-id only),
//! so reads go through a direct `env.d1("DB")` handle — the same binding
//! `WorkerRuntime::build_ui` wires into `D1JobLedger`.
//!
//! `GET /api/get_pending_tasks` is cursor-paged (`limit` / opaque `cursor`,
//! `next_cursor` in the response; `pending_count` / `running_count` are
//! whole-queue totals) so the modal never loads the entire queue in one body.
//! The keyset, SQL and cursor codec live in `queue/paging.rs`.

use serde::Deserialize;
use serde_json::json;
use wasm_bindgen::JsValue;
use worker::{Method, Request, Response, Result, console_log};

use crate::db_handle::DbHandle;

use paging::{ACTIVE_PAGE_SQL, ACTIVE_TOTALS_SQL};

/// Keyset paging for the queue modal (`GET /api/get_pending_tasks`).
mod paging;

/// Entry point; `lib.rs` routes `/api/tag_list`, `/api/queue/status`,
/// `/api/get_pending_tasks` and `/api/get_queue_size` here.
pub async fn handle(req: Request, env: worker::Env) -> Result<Response> {
    match req.path().as_str() {
        "/api/tag_list" => tag_list(req, env).await,
        "/api/queue/status" => queue_status(req, env).await,
        "/api/get_pending_tasks" => get_pending_tasks(req, env).await,
        "/api/get_queue_size" => get_queue_size(req, env).await,
        _ => Response::error("Not Found", 404),
    }
}

use narou_rs::application::webui::queue_view::{
    format_update_queue_target, queue_job_payload, queue_running_label, queue_target_fallback_text,
    queue_type_label,
};
use narou_rs::application::webui::tag_views::tag_list_html;

use super::{json_error, query_param};
use super::metadata::{self, MetadataServices};

/// Construct only the D1 session required by ledger reads.
macro_rules! database_or_503 {
    ($env:expr) => {
        match metadata::primary_database(&$env) {
            Ok(db) => db,
            Err(error) => {
                console_log!("D1 composition failed: {error}");
                return Response::error("Service unavailable", 503);
            }
        }
    };
}

// Keep these SQL strings explicit: the SQLite regression tests execute the
// production queries, including lane/status mappings and the bounded results.
const TAG_NAMES_SQL: &str = r#"
SELECT t.tag FROM novel_tags t
INNER JOIN novels n ON n.id = t.novel_id
GROUP BY t.tag COLLATE BINARY
ORDER BY COUNT(*) DESC, t.tag COLLATE BINARY ASC
"#;

const QUEUE_COUNTS_SQL: &str = r#"
SELECT status,
       CASE WHEN kind IN ('convert', 'send', 'backup', 'mail') THEN 1 ELSE 0 END AS lane,
       COUNT(*) AS n
FROM worker_jobs
WHERE status IN ('pending', 'retryable', 'running', 'succeeded', 'partial', 'blocked', 'permanent')
GROUP BY status, lane
"#;

// The label needs a target only when exactly one job is running. Read at most
// one row, in the same batch snapshot as QUEUE_COUNTS_SQL.
const FIRST_RUNNING_SQL: &str = r#"
SELECT job_id, kind, target, options, status, created_at
FROM worker_jobs WHERE status = 'running'
ORDER BY created_at ASC, job_id ASC LIMIT 1
"#;

const LANE_COUNTS_SQL: &str = r#"
SELECT CASE WHEN kind IN ('convert', 'send', 'backup', 'mail') THEN 1 ELSE 0 END AS lane,
       COUNT(*) AS n
FROM worker_jobs
WHERE status IN ('pending', 'retryable', 'running')
GROUP BY lane
"#;

#[derive(Debug, Deserialize)]
struct TagName {
    tag: String,
}

async fn select_tag_names(db: &DbHandle) -> Result<Vec<String>> {
    let result = db.prepare(TAG_NAMES_SQL).all().await?;
    Ok(result.results::<TagName>()?.into_iter().map(|row| row.tag).collect())
}

async fn tag_list(req: Request, env: worker::Env) -> Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    if req.method() != Method::Get {
        return Response::error("Method Not Allowed", 405);
    }
    let started_ms = js_sys::Date::now();
    let services = match MetadataServices::new(&env) {
        Ok(services) => services,
        Err(error) => {
            console_log!("D1 composition failed: {error}");
            return Response::error("Service unavailable", 503);
        }
    };
    let format = req
        .url()
        .ok()
        .and_then(|url| query_param(&url, "format"));

    // Independent reads overlap; never materialize all NovelRecords for tags.
    let (new_tag_color, tags) = futures::join!(
        metadata::configured_tag_color(&services.settings),
        select_tag_names(&services.db),
    );
    // Preserve the old empty-list fallback on storage errors.
    let tags = tags.unwrap_or_default();
    let tag_colors = services
        .tag_colors
        .for_tags(tags.clone(), new_tag_color.as_deref())
        .await
        .unwrap_or_default();

    if format.as_deref() == Some("json") {
        return metadata::timed_response(
            Response::from_json(&json!({ "tags": tags, "tag_colors": tag_colors }))?,
            "tags", started_ms,
        );
    }

    metadata::timed_response(
        Response::from_html(tag_list_html(&tags, &tag_colors))?,
        "tags",
        started_ms,
    )
}

// ---------------------------------------------------------------------------
// Ledger reads
// ---------------------------------------------------------------------------

/// One active ledger row as displayed by the Web UI.
#[derive(Debug, Deserialize)]
struct ActiveJobRow {
    job_id: String,
    kind: String,
    target: String,
    options: String,
    status: String,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct StatusCount {
    status: String,
    lane: usize,
    n: i64,
}

#[derive(Debug, Deserialize)]
struct LaneCount {
    lane: usize,
    n: i64,
}

fn is_pending_status(status: &str) -> bool {
    matches!(status, "pending" | "retryable")
}

/// `created_at` is stored as canonical RFC3339; the Web UI expects Unix
/// seconds (it multiplies by 1000 for `new Date`). Unparseable values map to
/// JSON null, matching "no timestamp" for the client.
fn created_at_epoch(created_at: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(created_at)
        .ok()
        .map(|dt| dt.timestamp())
}

fn job_json(row: &ActiveJobRow) -> serde_json::Value {
    queue_job_payload(
        &row.job_id,
        &display_type(row),
        &row.target,
        &display_target(row),
        created_at_epoch(&row.created_at),
    )
}

/// 表示用のジョブ種別名。ledger の `kind` がそのまま実行記録のコマンド名
/// (Worker には legacy cmd が無いので種別の正準名へそのまま写る)。
fn display_type(row: &ActiveJobRow) -> String {
    queue_type_label(None, &row.kind)
}

/// `format_queue_job_target` 相当。update だけが対象説明を組み立て、他の種別は
/// タブ区切りの生ターゲットへフォールバックする (native と同じ選択規則)。
fn display_target(row: &ActiveJobRow) -> String {
    if row.kind == "update" {
        let options: Vec<String> = serde_json::from_str(&row.options).unwrap_or_default();
        return format_update_queue_target(&options, None, Some(&row.target));
    }
    queue_target_fallback_text(&row.target)
}

/// One modal snapshot: the FIFO page for `cursor`/`limit` and the
/// server-side totals (waiting vs running). Both statements go out as one D1
/// batch, so a page costs one round trip and never carries the whole queue.
///
/// `cursor` is the last `(created_at, job_id)` of the previous page; `None`
/// starts from the beginning. The page SQL treats empty strings the same way.
async fn select_pending_page(
    db: &DbHandle,
    cursor: Option<(&str, &str)>,
    limit: usize,
) -> Result<(Vec<ActiveJobRow>, [i64; 2])> {
    let (created_at, job_id) = cursor.unwrap_or(("", ""));
    let page = db
        .prepare(ACTIVE_PAGE_SQL)
        .bind(&[
            JsValue::from_str(created_at),
            JsValue::from_str(created_at),
            JsValue::from_str(job_id),
            JsValue::from_f64(limit as f64),
        ])?;
    let totals = db.prepare(ACTIVE_TOTALS_SQL);
    let results = db.batch(vec![page, totals]).await?;
    crate::d1_repository::ensure_batch_success(&results)
        .map_err(|error| worker::Error::RustError(error.to_string()))?;
    if results.len() != 2 {
        return Err(worker::Error::RustError("incomplete pending tasks batch".into()));
    }
    let rows = results[0].results::<ActiveJobRow>()?;
    let mut counts = [0i64; 2];
    for row in results[1].results::<LaneCount>()? {
        let Some(lane) = counts.get_mut(row.lane) else {
            return Err(worker::Error::RustError("invalid pending task lane".into()));
        };
        *lane += row.n;
    }
    Ok((rows, counts))
}

/// Both status queries share one D1 batch/request. Pending job payloads never
/// cross the JS/Wasm boundary just to count them (at most 14 count rows + 1 job).
async fn select_queue_status(db: &DbHandle) -> Result<(Vec<StatusCount>, Vec<ActiveJobRow>)> {
    let results = db.batch(vec![
        db.prepare(QUEUE_COUNTS_SQL),
        db.prepare(FIRST_RUNNING_SQL),
    ]).await?;
    crate::d1_repository::ensure_batch_success(&results)
        .map_err(|error| worker::Error::RustError(error.to_string()))?;
    if results.len() != 2 {
        return Err(worker::Error::RustError("incomplete queue status batch".into()));
    }
    Ok((results[0].results::<StatusCount>()?, results[1].results::<ActiveJobRow>()?))
}

async fn select_lane_sizes(db: &DbHandle) -> Result<[i64; 2]> {
    let result = db.prepare(LANE_COUNTS_SQL).all().await?;
    let rows = result.results::<LaneCount>()?;
    let mut lanes = [0; 2];
    for row in rows {
        let Some(lane) = lanes.get_mut(row.lane) else {
            return Err(worker::Error::RustError("invalid queue lane".into()));
        };
        *lane += row.n;
    }
    Ok(lanes)
}

// ---------------------------------------------------------------------------
// GET /api/queue/status
// ---------------------------------------------------------------------------

async fn queue_status(req: Request, env: worker::Env) -> Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    if req.method() != Method::Get {
        return Response::error("Method Not Allowed", 405);
    }
    let started_ms = js_sys::Date::now();
    let db = database_or_503!(env);
    let (counts, running) = match select_queue_status(&db).await {
        Ok(snapshot) => snapshot,
        Err(_) => return json_error(500, "ledger_read_failed", None),
    };
    let (mut pending_count, mut running_count, mut completed, mut partial, mut failed) =
        (0i64, 0i64, 0i64, 0i64, 0i64);
    let mut lane_sizes = [0i64; 2];
    for row in &counts {
        match row.status.as_str() {
            "pending" | "retryable" => pending_count += row.n,
            "running" => running_count += row.n,
            "succeeded" => completed += row.n,
            "partial" => partial += row.n,
            "permanent" | "blocked" => failed += row.n,
            _ => {}
        }
        if is_pending_status(&row.status) || row.status == "running" {
            let Some(lane) = lane_sizes.get_mut(row.lane) else {
                return json_error(500, "ledger_read_failed", None);
            };
            *lane += row.n;
        }
    }
    let single_running = running.first().map(display_target);
    if running_count == 1 && single_running.is_none() {
        return json_error(500, "ledger_read_failed", None);
    }
    let running_label = queue_running_label(
        usize::try_from(running_count).unwrap_or(0),
        single_running,
    );
    metadata::timed_response(Response::from_json(&json!({
        "pending": pending_count,
        "completed": completed,
        "partial": partial,
        "failed": failed,
        "cancelled": 0,
        "running": running_label,
        "running_count": running_count,
        "lane_sizes": lane_sizes,
    }))?, "queue", started_ms)
}

// ---------------------------------------------------------------------------
// GET /api/get_pending_tasks
// ---------------------------------------------------------------------------

/// `GET /api/get_pending_tasks[?limit=N&cursor=<opaque>]` — one FIFO page of
/// active jobs plus the true totals. A client walks every row by passing the
/// returned `next_cursor` until it is null; counts are never page-local, so
/// the header stays correct while paging.
async fn get_pending_tasks(req: Request, env: worker::Env) -> Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    if req.method() != Method::Get {
        return Response::error("Method Not Allowed", 405);
    }
    let (limit, cursor) = match req.url() {
        Ok(url) => (
            paging::clamp_limit(query_param(&url, "limit").as_deref()),
            query_param(&url, "cursor").and_then(|raw| paging::decode_cursor(&raw)),
        ),
        // A request URL that cannot be parsed has no query parameters.
        Err(_) => (paging::DEFAULT_PAGE_SIZE, None),
    };
    let db = database_or_503!(env);
    let (rows, counts) = match select_pending_page(
        &db,
        cursor.as_ref().map(|(created_at, job_id)| (created_at.as_str(), job_id.as_str())),
        limit,
    )
    .await
    {
        Ok(snapshot) => snapshot,
        Err(_) => return json_error(500, "ledger_read_failed", None),
    };
    // A full page may or may not be the last one: hand out the keyset cursor
    // of its final row and let the next request decide. An empty / partial
    // page terminates the walk.
    let next_cursor = if rows.len() == limit {
        rows.last()
            .map(|row| paging::encode_cursor(&row.created_at, &row.job_id))
    } else {
        None
    };

    let pending_json: Vec<serde_json::Value> = rows
        .iter()
        .filter(|row| is_pending_status(&row.status))
        .map(job_json)
        .collect();
    let running_json: Vec<serde_json::Value> = rows
        .iter()
        .filter(|row| row.status == "running")
        .map(job_json)
        .collect();

    Response::from_json(&json!({
        "pending": pending_json,
        "running": running_json,
        // Totals for the whole active queue, not just this page.
        "pending_count": counts[0],
        "running_count": counts[1],
        "limit": limit,
        "next_cursor": next_cursor,
        // The worker ledger has no restorable/deferred jobs: nothing survives
        // as a resumable native-style deferred queue entry.
        "restorable_tasks_available": false,
        "restore_prompt_pending": false,
    }))
}

// ---------------------------------------------------------------------------
// GET /api/get_queue_size (native: src/web/jobs.rs get_queue_size)
// ---------------------------------------------------------------------------

/// native `queue_lane_sizes`: `[default, secondary]` の
/// (pending + running) 件数をそのまま 2 要素の配列で返す。
async fn get_queue_size(req: Request, env: worker::Env) -> Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    if req.method() != Method::Get {
        return Response::error("Method Not Allowed", 405);
    }
    let started_ms = js_sys::Date::now();
    let db = database_or_503!(env);
    let lanes = match select_lane_sizes(&db).await {
        Ok(lanes) => lanes,
        Err(_) => return json_error(500, "ledger_read_failed", None),
    };
    metadata::timed_response(Response::from_json(&lanes)?, "queue_size", started_ms)
}

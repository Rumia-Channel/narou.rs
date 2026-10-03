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

use serde::Deserialize;
use serde_json::json;
use worker::{Method, Request, Response, Result, console_log};

use crate::db_handle::DbHandle;

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

use narou_rs::application::webui::{html_escape, tag_color_class};

use super::{json_error, query_param};
use super::metadata::{self, MetadataServices};

/// Construct only the D1 session required by ledger reads.
macro_rules! database_or_503 {
    ($env:expr) => {
        match metadata::database(&$env) {
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

    let mut html = String::from(
        "<div><span class=\"tag-label tag-default tag-reset\" data-tag=\"\">タグ検索を解除</span></div>\
<div class=\"text-muted\" style=\"font-size:0.8em\">Altキーを押しながらで除外検索</div>",
    );
    for tag in &tags {
        let escaped_tag = html_escape(tag);
        let class = tag_color_class(
            tag_colors.get(tag).map(String::as_str).unwrap_or("default"),
        );
        html.push_str(&format!(
            "<div><span class=\"tag-label {}\" data-tag=\"{}\">{}</span> \
<span class=\"select-color-button\" data-target-tag=\"{}\"><span class=\"tag-label {} tag-fixed-width\">a</span></span></div>",
            class, escaped_tag, escaped_tag, escaped_tag, class
        ));
    }
    metadata::timed_response(Response::from_html(html)?, "tags", started_ms)
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
fn created_at_epoch(created_at: &str) -> serde_json::Value {
    match chrono::DateTime::parse_from_rfc3339(created_at) {
        Ok(dt) => json!(dt.timestamp()),
        Err(_) => serde_json::Value::Null,
    }
}

fn job_json(row: &ActiveJobRow) -> serde_json::Value {
    json!({
        "id": row.job_id.as_str(),
        "type": display_type(row),
        "target": row.target.as_str(),
        "display_target": display_target(row),
        "created_at": created_at_epoch(&row.created_at),
    })
}

/// `format_queue_job_type`: the command name when known, else the job type.
/// The ledger's `kind` column already holds the canonical command string.
fn display_type(row: &ActiveJobRow) -> &str {
    &row.kind
}

/// `format_queue_job_target` / `queue_target_fallback_text` parity.
///
/// Update jobs describe their effective targets ("ID 3 の小説を更新"); every
/// other kind falls back to the raw target text with tab-joined components.
fn display_target(row: &ActiveJobRow) -> String {
    if row.kind == "update" {
        let options: Vec<String> = serde_json::from_str(&row.options).unwrap_or_default();
        return format_update_queue_target(&options, &row.target);
    }
    fallback_target_text(&row.target)
}

/// Native fallback: tab-separated multi-target strings are joined with " / ".
fn fallback_target_text(target: &str) -> String {
    target
        .split('\t')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" / ")
}

/// `format_update_queue_target`: builds the same label the native queue UI
/// shows for update jobs. `plan.target` is the effective target list — the
/// worker stores exactly one normalized target per job, or `*` for
/// library-wide auto updates.
fn format_update_queue_target(options: &[String], target: &str) -> String {
    if options.first().map(String::as_str) == Some("--gl") {
        return match options.get(1).map(String::as_str) {
            Some("narou") => "なろうAPIで最新話掲載日を確認".to_string(),
            Some("other") => "その他サイトの最新話掲載日を確認".to_string(),
            _ => "最新話掲載日を確認".to_string(),
        };
    }

    let force = options.iter().any(|option| option == "--force");
    let targets: Vec<String> = if target == "*" {
        Vec::new()
    } else {
        target
            .split('\t')
            .filter(|part| !part.is_empty())
            .map(str::to_string)
            .collect()
    };
    let target_text = describe_update_targets(&targets);
    if force {
        format!("{}を凍結済みも含めて更新", target_text)
    } else {
        format!("{}を更新", target_text)
    }
}

/// `describe_update_targets` parity.
fn describe_update_targets(targets: &[String]) -> String {
    match targets {
        [] => "全ての小説".to_string(),
        [target] => {
            if let Some(tag) = target.strip_prefix("tag:") {
                format!("タグ「{}」の小説", tag)
            } else if target.chars().all(|ch| ch.is_ascii_digit()) {
                format!("ID {} の小説", target)
            } else {
                target.to_string()
            }
        }
        _ => format!("{}件の小説", targets.len()),
    }
}

/// All active (pending / retryable / running) rows in FIFO order.
async fn select_active_rows(db: &DbHandle) -> Result<Vec<ActiveJobRow>> {
    let statement = db.prepare(
        "SELECT job_id, kind, target, options, status, created_at
         FROM worker_jobs
         WHERE status IN ('pending', 'retryable', 'running')
         ORDER BY created_at ASC, job_id ASC",
    );
    statement
        .all()
        .await
        .and_then(|result| result.results::<ActiveJobRow>())
        .map_err(|error| worker::Error::RustError(format!("worker_jobs read: {error}")))
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
    let running_label = match running_count {
        0 => serde_json::Value::Null,
        1 => {
            let Some(job) = running.first() else {
                return json_error(500, "ledger_read_failed", None);
            };
            serde_json::Value::String(display_target(job))
        }
        n => serde_json::Value::String(format!("{} 件実行中", n)),
    };
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

async fn get_pending_tasks(req: Request, env: worker::Env) -> Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    if req.method() != Method::Get {
        return Response::error("Method Not Allowed", 405);
    }
    let db = database_or_503!(env);
    let rows = match select_active_rows(&db).await {
        Ok(rows) => rows,
        Err(_) => return json_error(500, "ledger_read_failed", None),
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
    let pending_count = pending_json.len();
    let running_count = running_json.len();

    Response::from_json(&json!({
        "pending": pending_json,
        "running": running_json,
        "pending_count": pending_count,
        "running_count": running_count,
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

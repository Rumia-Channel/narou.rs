//! Web UI queue/tag endpoints backed by the D1 job ledger (`worker_jobs`).
//!
//! JSON parity with the native handlers:
//! - `src/web/misc.rs` `tag_list`
//! - `src/web/jobs.rs` `queue_status` / `get_pending_tasks`
//!
//! Status mapping (native `PersistentQueue` → worker ledger):
//! - `pending`   ← `pending` + `retryable` (both wait for execution; native
//!                 failed-retry jobs stay in the pending set, so retryable
//!                 ledger rows are the same thing for the UI)
//! - `running`   ← `running`
//! - `completed` ← `succeeded`
//! - `partial`   ← `partial`
//! - `cancelled` ← none (the worker ledger has no cancelled state)
//! - `failed`    ← `permanent` + `blocked` (both are terminal states that need
//!                 the operator to look at them; the UI only has a failed
//!                 bucket)
//!
//! The ledger does not expose listing primitives (`JobQueue` is per-id only),
//! so reads go through a direct `env.d1("DB")` handle — the same binding
//! `WorkerRuntime::build` wires into `D1JobLedger`.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::json;
use worker::{Method, Request, Response, Result, console_log};

use crate::composition::WorkerRuntime;

/// Entry point; `lib.rs` routes `/api/tag_list`, `/api/queue/status` and
/// `/api/get_pending_tasks` here.
pub async fn handle(req: Request, env: worker::Env) -> Result<Response> {
    match req.path().as_str() {
        "/api/tag_list" => tag_list(req, env).await,
        "/api/queue/status" => queue_status(req, env).await,
        "/api/get_pending_tasks" => get_pending_tasks(req, env).await,
        _ => Response::error("Not Found", 404),
    }
}

use narou_rs::application::webui::{html_escape, tag_color_class};

use super::{configured_tag_color, json_error, query_param};

/// Build the runtime or finish with the native-style 503 error response.
macro_rules! runtime_or_503 {
    ($env:expr) => {
        match WorkerRuntime::build(&$env).await {
            Ok(runtime) => runtime,
            Err(error) => {
                console_log!("service composition failed: {error}");
                return Response::error("Service unavailable", 503);
            }
        }
    };
}

async fn tag_list(req: Request, env: worker::Env) -> Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    if req.method() != Method::Get {
        return Response::error("Method Not Allowed", 405);
    }
    let runtime = runtime_or_503!(env);
    let format = req
        .url()
        .ok()
        .and_then(|url| query_param(&url, "format"));

    let new_tag_color = configured_tag_color(&runtime).await;
    let records = match runtime.services.library.records().await {
        Ok(records) => records,
        Err(_) => Vec::new(),
    };
    let mut counts: HashMap<String, usize> = HashMap::new();
    for record in &records {
        for tag in &record.tags {
            *counts.entry(tag.clone()).or_insert(0) += 1;
        }
    }
    let mut list: Vec<(String, usize)> = counts.into_iter().collect();
    list.sort_by(|a, b| b.1.cmp(&a.1));
    let tags = list.into_iter().map(|(tag, _)| tag).collect::<Vec<_>>();
    let tag_colors = runtime
        .services
        .tag_colors
        .for_tags(tags.clone(), new_tag_color.as_deref())
        .await
        .unwrap_or_default();

    if format.as_deref() == Some("json") {
        return Response::from_json(&json!({ "tags": tags, "tag_colors": tag_colors }));
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
    Response::from_html(html)
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
    n: i64,
}

fn is_pending_status(status: &str) -> bool {
    matches!(status, "pending" | "retryable")
}

/// Native `JobType::lane`: download/update/auto_update use the default lane;
/// convert/send/backup/mail share the secondary lane.
fn lane_index(kind: &str) -> usize {
    match kind {
        "convert" | "send" | "backup" | "mail" => 1,
        _ => 0,
    }
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

fn d1(env: &worker::Env) -> Result<worker::D1Database> {
    env.d1("DB")
}

/// All active (pending / retryable / running) rows in FIFO order.
async fn select_active_rows(env: &worker::Env) -> Result<Vec<ActiveJobRow>> {
    let statement = d1(env)?.prepare(
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

/// Terminal-state counts, grouped by ledger status string.
async fn select_terminal_counts(env: &worker::Env) -> Result<HashMap<String, i64>> {
    let statement = d1(env)?.prepare(
        "SELECT status, COUNT(*) AS n
         FROM worker_jobs
         WHERE status IN ('succeeded', 'partial', 'blocked', 'permanent')
         GROUP BY status",
    );
    let rows = statement
        .all()
        .await
        .and_then(|result| result.results::<StatusCount>())
        .map_err(|error| worker::Error::RustError(format!("worker_jobs count: {error}")))?;
    Ok(rows.into_iter().map(|row| (row.status, row.n)).collect())
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
    let _runtime = runtime_or_503!(env);
    let rows = match select_active_rows(&env).await {
        Ok(rows) => rows,
        Err(_) => return json_error(500, "ledger_read_failed", None),
    };
    let terminal = match select_terminal_counts(&env).await {
        Ok(counts) => counts,
        Err(_) => return json_error(500, "ledger_read_failed", None),
    };

    let running: Vec<&ActiveJobRow> = rows
        .iter()
        .filter(|row| row.status == "running")
        .collect();
    let pending_count = rows
        .iter()
        .filter(|row| is_pending_status(&row.status))
        .count();
    let running_label = match running.as_slice() {
        [] => serde_json::Value::Null,
        [job] => serde_json::Value::String(display_target(job)),
        jobs => serde_json::Value::String(format!("{} 件実行中", jobs.len())),
    };
    let mut lane_sizes = [0usize; 2];
    for row in &rows {
        lane_sizes[lane_index(&row.kind)] += 1;
    }

    let failed = terminal.get("permanent").copied().unwrap_or(0)
        + terminal.get("blocked").copied().unwrap_or(0);
    Response::from_json(&json!({
        "pending": pending_count,
        "completed": terminal.get("succeeded").copied().unwrap_or(0),
        "partial": terminal.get("partial").copied().unwrap_or(0),
        "failed": failed,
        "cancelled": 0,
        "running": running_label,
        "running_count": running.len(),
        "lane_sizes": lane_sizes,
    }))
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
    let _runtime = runtime_or_503!(env);
    let rows = match select_active_rows(&env).await {
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

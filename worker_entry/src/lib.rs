#![cfg(target_arch = "wasm32")]

mod bundled_sites;
mod composition;
mod consumer;
mod d1_repository;
mod executor;
pub mod http;
mod ledger;
mod rate_limiter;
mod scheduler;
mod site_rate_limiter;
mod wasabi;
use subtle::ConstantTimeEq;

use serde_json::json;
use worker::*;
use narou_rs::application::JobQueue as _JobQueueTrait;

use crate::composition::{WorkerRuntime, check_ready};
use crate::executor::unsupported_reason;

#[event(fetch)]
pub async fn main(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let path = req.path();
    match path.as_str() {
        "/" | "/health/live" => Response::ok("narou.rs worker is alive"),
        "/health/ready" => match check_ready(&env).await {
            Ok(()) => Response::ok("narou.rs worker is ready"),
            Err(error) => {
                console_log!("readiness failed: {error}");
                Response::error("Not Ready", 503)
            }
        },
        "/api/novels" => api_novels(req, env).await,
        "/api/jobs" => api_jobs(req, env).await,
        _ if path.starts_with("/api/novels/") => api_novel(req, env).await,
        _ if path.starts_with("/api/jobs/") => api_job(req, env).await,
        _ => Response::error("Not Found", 404),
    }
}

async fn api_novels(req: Request, env: Env) -> Result<Response> {
    if req.method() != Method::Get {
        return Response::error("Method Not Allowed", 405);
    }
    if !authorized(&req, &env) {
        return Response::error("Unauthorized", 401);
    }
    let services = match composition::build_services(&env) {
        Ok(services) => services,
        Err(_) => return Response::error("Service unavailable", 503),
    };
    let url = req.url().map_err(|error| Error::RustError(error.to_string()))?;
    let search = query_param(&url, "q");
    let start = query_param(&url, "start").and_then(|value| value.parse().ok()).unwrap_or(0);
    let length = query_param(&url, "limit").and_then(|value| value.parse().ok()).or(Some(100));
    let page = services
        .library
        .list(&narou_rs::application::LibraryListRequest {
            search,
            start,
            length,
            sort_column: narou_rs::application::LibrarySortColumn::Id,
            sort_order: narou_rs::application::LibrarySortOrder::Ascending,
        })
        .await
        .map_err(|error| Error::RustError(error.to_string()))?;
    let data = page
        .data
        .into_iter()
        .map(|row| {
            serde_json::json!({
                "id": row.id,
                "title": row.title,
                "author": row.author,
                "sitename": row.sitename,
                "novel_type": row.novel_type,
                "end": row.end,
                "last_update": row.last_update.to_rfc3339(),
                "general_lastup": row.general_lastup.map(|value| value.to_rfc3339()),
                "last_check_date": row.last_check_date.map(|value| value.to_rfc3339()),
                "new_arrivals_date": row.new_arrivals_date.map(|value| value.to_rfc3339()),
                "tags": row.tags,
                "new_arrivals": row.new_arrivals,
                "frozen": row.frozen,
                "suspend": row.suspend,
                "length": row.length,
                "toc_url": row.toc_url,
                "general_all_no": row.general_all_no,
            })
        })
        .collect::<Vec<_>>();
    Response::from_json(&serde_json::json!({
        "records_total": page.records_total,
        "records_filtered": page.records_filtered,
        "data": data,
    }))
}

async fn api_novel(req: Request, env: Env) -> Result<Response> {
    if req.method() != Method::Get {
        return Response::error("Method Not Allowed", 405);
    }
    if !authorized(&req, &env) {
        return Response::error("Unauthorized", 401);
    }
    let id = req
        .path()
        .strip_prefix("/api/novels/")
        .and_then(|value| value.parse::<i64>().ok());
    let Some(id) = id else {
        return Response::error("Not Found", 404);
    };
    let services = match composition::build_services(&env) {
        Ok(services) => services,
        Err(_) => return Response::error("Service unavailable", 503),
    };
    let record = services
        .library
        .get(narou_rs::platform::NovelId(id))
        .await
        .map_err(|error| Error::RustError(error.to_string()))?;
    let Some(record) = record else {
        return Response::error("Not Found", 404);
    };
    let value = serde_json::to_value(record).map_err(|error| Error::RustError(error.to_string()))?;
    Response::from_json(&value)
}

/// POST /api/jobs — plan a request, enqueue each discrete plan separately,
/// and return ids / invalid / duplicates / blocked.
///
/// Unsupported kinds (Convert/Send/Mail/Backup) and targetless auto-update
/// are routed to a durable `blocked` ledger state — never a subprocess,
/// never a silent ack.
async fn api_jobs(mut req: Request, env: Env) -> Result<Response> {
    if req.method() != Method::Post {
        return Response::error("Method Not Allowed", 405);
    }
    if !authorized(&req, &env) {
        return Response::error("Unauthorized", 401);
    }
    let request: narou_rs::application::JobRequest = match req.json().await {
        Ok(request) => request,
        Err(_) => return Response::error("Invalid JSON body", 400),
    };
    if let Some(reason) = narou_rs::application::validate_request_limits(&request) {
        return Response::error(format!("request exceeds payload limits: {reason}"), 400);
    }
    let runtime = match WorkerRuntime::build(&env) {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("service composition failed: {error}");
            return Response::error("Service unavailable", 503);
        }
    };
    let planned = runtime.services.jobs.plan(&request);
    let mut ids = Vec::new();
    let mut blocked = Vec::new();
    for plan in planned.plans {
        let queued = match runtime.ledger.enqueue(plan.clone()).await {
            Ok(queued) => queued,
            Err(error) => {
                return Response::error(format!("enqueue failed: {error}"), 500);
            }
        };
        if let Some(reason) = unsupported_reason(&plan) {
            // Durable blocked state; no execution, no subprocess.
            if let Err(error) = runtime
                .ledger
                .mark_terminal(
                    &queued.job_id,
                    narou_rs::application::JobLedgerStatus::Blocked,
                    Some(&reason),
                )
                .await
            {
                return Response::error(format!("blocked recording failed: {error}"), 500);
            }
            blocked.push(queued.job_id.as_str().to_string());
        } else {
            let envelope =
                narou_rs::application::WorkerJobEnvelope::v2(queued.job_id.clone(), queued.job);
            // Never send an oversized envelope: durably block it instead.
            let within_limits = narou_rs::application::envelope_bytes(&envelope)
                .is_ok_and(|size| size <= narou_rs::application::job_limits::MAX_ENVELOPE_BYTES);
            if !within_limits {
                let reason = format!(
                    "envelope exceeds queue payload limit (max {} bytes)",
                    narou_rs::application::job_limits::MAX_ENVELOPE_BYTES
                );
                if let Err(error) = runtime
                    .ledger
                    .mark_terminal(
                        &queued.job_id,
                        narou_rs::application::JobLedgerStatus::Blocked,
                        Some(&reason),
                    )
                    .await
                {
                    return Response::error(format!("blocked recording failed: {error}"), 500);
                }
                blocked.push(queued.job_id.as_str().to_string());
                continue;
            }
            if let Err(error) = runtime.queue.send(&envelope).await {
                // The ledger row stays active, so a client retry of the same
                // request returns the same id (dedupe) and re-sends cleanly.
                return Response::error(format!("queue send failed: {error}"), 502);
            }
            ids.push(queued.job_id.as_str().to_string());
        }
    }
    Response::from_json(&json!({
        "ids": ids,
        "invalid": planned.invalid,
        "duplicates": planned.duplicates,
        "blocked": blocked,
    }))
}

/// GET /api/jobs/:id — one ledger row with status/attempts/timestamps.
async fn api_job(req: Request, env: Env) -> Result<Response> {
    if req.method() != Method::Get {
        return Response::error("Method Not Allowed", 405);
    }
    if !authorized(&req, &env) {
        return Response::error("Unauthorized", 401);
    }
    let id = req.path().strip_prefix("/api/jobs/").map(str::to_string);
    let Some(id) = id else {
        return Response::error("Not Found", 404);
    };
    let runtime = match WorkerRuntime::build(&env) {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("service composition failed: {error}");
            return Response::error("Service unavailable", 503);
        }
    };
    match runtime.ledger.get(&narou_rs::application::JobId(id)).await {
        Ok(Some(view)) => Response::from_json(&view),
        Ok(None) => Response::error("Not Found", 404),
        Err(error) => Response::error(format!("ledger read failed: {error}"), 500),
    }
}

fn authorized(req: &Request, env: &Env) -> bool {
    let Ok(secret) = env.secret("NAROU_ADMIN_TOKEN") else {
        return false;
    };
    let expected = format!("Bearer {}", secret);
    let Ok(actual) = req.headers().get("authorization") else {
        return false;
    };
    let Some(actual) = actual else {
        return false;
    };
    actual.as_bytes().ct_eq(expected.as_bytes()).into()
}

fn query_param(url: &worker::Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

#[event(scheduled)]
pub async fn scheduled(event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    // The cron handler only plans and enqueues; execution happens through
    // the queue consumer. Errors are logged: the next scheduled event
    // re-plans, and the ledger's active dedupe keeps it duplicate-free.
    if let Err(error) = scheduler::run_scheduled_plan(event, &env).await {
        console_error!("scheduled planner failed: {error}");
    }
}

#[event(queue)]
pub async fn queue(
    message_batch: MessageBatch<serde_json::Value>,
    env: Env,
    _ctx: Context,
) -> Result<()> {
    consumer::process_batch(message_batch, &env)
        .await
        .map_err(|error| Error::RustError(error.to_string()))
}

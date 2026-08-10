#![cfg(target_arch = "wasm32")]

mod composition;
mod d1_repository;
pub mod http;
mod wasabi;
use subtle::ConstantTimeEq;

use serde::{Deserialize, Serialize};
use worker::*;
pub const WORKER_JOB_ENVELOPE_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkerJobEnvelope {
    pub version: u32,
    pub job: narou_rs::application::JobRequest,
}


#[event(fetch)]
pub async fn main(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let path = req.path();
    match path.as_str() {
        "/" | "/health/live" => Response::ok("narou.rs worker is alive"),
        "/health/ready" => match composition::build_services(&env) {
            Ok(_) => Response::ok("narou.rs worker is ready"),
            Err(_) => Response::error("Not Ready", 503),
        },
        "/api/novels" => api_novels(req, env).await,
        _ if path.starts_with("/api/novels/") => api_novel(req, env).await,
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
pub async fn scheduled(_event: ScheduledEvent, _env: Env, _ctx: ScheduleContext) {}

#[event(queue)]
pub async fn queue(
    message_batch: MessageBatch<WorkerJobEnvelope>,
    _env: Env,
    _ctx: Context,
) -> Result<()> {
    for message in message_batch.messages()? {
        let envelope = message.body();
        if envelope.version != WORKER_JOB_ENVELOPE_VERSION {
            console_log!(
                "unsupported narou job envelope version: {}",
                envelope.version
            );
            message.retry();
            continue;
        }

        console_log!(
            "received unsupported narou job kind: {:?}",
            envelope.job.kind
        );
        // No executor is wired in Phase 7. Retrying is intentional: accepting
        // and acknowledging a job here would silently lose durable work.
        message.retry();
    }
    Ok(())
}

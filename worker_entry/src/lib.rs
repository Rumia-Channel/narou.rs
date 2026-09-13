#![cfg(target_arch = "wasm32")]

mod budget;
mod bundled_sites;
mod composition;
mod consumer;
mod d1_object_store;
mod d1_repository;
mod executor;
pub mod http;
mod ledger;
mod rate_limiter;
mod scheduler;
mod site_rate_limiter;
use subtle::ConstantTimeEq;

use serde_json::json;
use worker::*;
use narou_rs::application::JobQueue as _JobQueueTrait;

use crate::composition::{WorkerRuntime, check_ready};

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
    let path = req.path();
    let rest = path.strip_prefix("/api/novels/").unwrap_or_default();
    if let Some(id) = rest.strip_suffix("/download.epub") {
        return match id.parse::<i64>() {
            Ok(id) => api_novel_download_epub(req, env, id).await,
            Err(_) => Response::error("Not Found", 404),
        };
    }
    let Some(id) = rest.parse::<i64>().ok() else {
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

/// GET /api/novels/:id/download.epub — build the EPUB from the stored
/// converted text (`novel.txt` object) at download time and return it as the
/// response body. Illustrations are read from the object store through a
/// bounded prefetch so peak memory stays capped.
async fn api_novel_download_epub(req: Request, env: Env, id: i64) -> Result<Response> {
    if !authorized(&req, &env) {
        return Response::error("Unauthorized", 401);
    }
    let services = match composition::build_read_services(&env) {
        Ok(services) => services,
        Err(_) => return Response::error("Service unavailable", 503),
    };
    let record = match services
        .app
        .library
        .get(narou_rs::platform::NovelId(id))
        .await
    {
        Ok(Some(record)) => record,
        Ok(None) => return Response::error("Not Found", 404),
        Err(error) => return Response::error(&format!("Repository error: {error}"), 500),
    };
    let keys = match narou_rs::platform::NovelObjectKeys::new(
        &record.sitename,
        &record.file_title,
        record.use_subdirectory,
    ) {
        Ok(keys) => keys,
        Err(error) => return Response::error(&format!("Invalid key: {error}"), 500),
    };

    let text_key = keys.converted_text();
    let text_bytes = match services.objects.read_small(&text_key).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            return Response::error(
                "Converted text not found: run convert (text-only) for this novel first",
                409,
            );
        }
        Err(error) => return Response::error(&format!("Object store error: {error}"), 500),
    };
    let text = match String::from_utf8(text_bytes) {
        Ok(text) => text,
        Err(_) => return Response::error("Stored text is not valid UTF-8", 500),
    };

    // Prefetch illustrations with hard caps; the EPUB writer then resolves
    // them from memory without touching storage mid-stream.
    const MAX_IMAGES: usize = 512;
    const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
    let mut prefetched: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();
    let mut total = 0usize;
    let Ok(illust_prefix) = narou_rs::platform::ObjectPrefix::new(format!(
        "{}/挿絵",
        keys.prefix()
    )) else {
        return Response::error("Invalid illustration prefix", 500);
    };
    let mut cursor = None;
    loop {
        let mut request =
            narou_rs::platform::ObjectListRequest::new(illust_prefix.clone(), 100.try_into().unwrap());
        request.cursor = cursor.take();
        let page = match services.objects.list_page(&request).await {
            Ok(page) => page,
            Err(_) => break,
        };
        for meta in page.objects {
            if prefetched.len() >= MAX_IMAGES {
                break;
            }
            let key = meta.key;
            let name = key.as_ref().rsplit('/').next().unwrap_or_default().to_string();
            if narou_rs::epub_lite::media_type_for(&name).is_none() {
                continue;
            }
            if let Ok(Some(bytes)) = services.objects.read_small(&key).await {
                total += bytes.len();
                prefetched.insert(format!("image/{name}"), bytes);
                if total > MAX_IMAGE_BYTES {
                    break;
                }
            }
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }

    let images = narou_rs::epub_lite::image_entries(&text)
        .into_iter()
        .filter(|image| prefetched.contains_key(&image.epub_path))
        .collect::<Vec<_>>();
    let options = narou_rs::epub_lite::EpubBuildOptions {
        title: record.title.clone(),
        author: record.author.clone(),
        source_id: format!("narou:{id}"),
        vertical: true,
        cover_from_first_image: !images.is_empty(),
    };
    let book = match narou_rs::epub_lite::build_book(&text, &options, &images) {
        Ok(book) => book,
        Err(error) => return Response::error(&format!("EPUB build failed: {error}"), 500),
    };
    let mut body = Vec::new();
    if let Err(error) = narou_rs::epub_lite::stream_epub(&book, &mut body, |path| {
        prefetched.get(path).cloned()
    }) {
        return Response::error(&format!("EPUB write failed: {error}"), 500);
    }

    Ok(
        worker::ResponseBuilder::new()
            .with_header("Content-Type", "application/epub+zip")?
            .with_header(
                "Content-Disposition",
                &format!("attachment; filename=\"novel-{id}.epub\""),
            )?
            .from_bytes(body)?,
    )
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
    let invalid = planned.invalid.clone();
    let duplicates = planned.duplicates.clone();
    let mut ids = Vec::new();
    let mut blocked = Vec::new();
    for plan in planned.plans {
        let outcome = match runtime.enqueue_plan(plan).await {
            Ok(outcome) => outcome,
            Err(error) => {
                return Response::error(format!("enqueue failed: {error}"), 502);
            }
        };
        if let Some(reason) = outcome.blocked {
            console_log!("job {} blocked: {}", outcome.job_id, reason);
            blocked.push(outcome.job_id);
        } else {
            ids.push(outcome.job_id);
        }
    }
    Response::from_json(&json!({
        "ids": ids,
        "invalid": invalid,
        "duplicates": duplicates,
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

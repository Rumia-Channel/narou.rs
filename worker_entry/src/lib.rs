#![cfg(target_arch = "wasm32")]

mod budget;
mod bundled_sites;
mod composition;
mod convert;
mod global_settings;
mod websocket;
mod webui;
mod push_hub;
mod login;
mod secrets;
mod sites;
mod consumer;
mod d1_cookie_store;
mod d1_object_store;
mod d1_repository;
mod executor;
pub mod http;
mod ledger;
mod object_migration;
mod rate_limiter;
mod scheduler;
mod s3_object_store;
mod site_rate_limiter;
use subtle::ConstantTimeEq;
use webui::{json_error, query_param};

use serde_json::json;
use worker::*;
use narou_rs::application::{JobQueue as _JobQueueTrait, TagAction};

use crate::composition::{WorkerRuntime, check_ready};

#[event(fetch)]
pub async fn main(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let path = req.path();
    match path.as_str() {
        "/health/live" => health_payload(&env, "alive").await,
        "/health/ready" => match check_ready(&env).await {
            Ok(()) => health_payload(&env, "ready").await,
            Err(error) => {
                console_log!("readiness failed: {error}");
                health_payload(&env, "not_ready")
                    .await
                    .map(|response| response.with_status(503))
            }
        },
        "/api/novels" => api_novels(req, env).await,
        "/api/login" => api_login(req, env).await,
        "/api/sites" => api_sites(req, env).await,
        // native は `POST /api/login/set` と `POST /api/login/add` を
        // `login_set_or_add(append)` で振り分ける (login.rs と同じ)。
        "/api/login/set" => api_login_set(req, env, false).await,
        "/api/login/add" => api_login_set(req, env, true).await,
        "/api/jobs" => api_jobs(req, env).await,
        "/api/global_setting" => global_settings::api_global_setting(req, env).await,
        "/api/admin/object-migration" => api_object_migration(req, env).await,
        "/api/library_backup" => webui::library_backup::handle(req, env).await,
        "/api/list" => webui::list::handle(req, env).await,
        "/api/sort_state" => webui::ui_prefs::handle(req, env).await,
        "/api/webui/config" => webui::ui_prefs::handle(req, env).await,
        "/api/feature_tour/pending" => webui::ui_prefs::handle(req, env).await,
        "/api/feature_tour/all" => webui::ui_prefs::handle(req, env).await,
        "/api/feature_tour/seen" => webui::ui_prefs::handle(req, env).await,
        "/api/feature_tour/config" => webui::ui_prefs::handle(req, env).await,
        "/api/tag_list" => webui::queue::handle(req, env).await,
        "/api/queue/status" => webui::queue::handle(req, env).await,
        "/api/get_pending_tasks" => webui::queue::handle(req, env).await,
        "/api/download" => webui::download::handle(req, env).await,
        "/api/story" => webui::read_views::handle(req, env).await,
        "/api/diff_list" => webui::read_views::handle(req, env).await,
        "/api/diff_clean" => webui::read_views::handle(req, env).await,
        "/api/inspect" => webui::read_views::handle(req, env).await,
        "/api/notepad/read" => webui::read_views::handle(req, env).await,
        "/api/notepad/save" => webui::read_views::handle(req, env).await,
        "/api/history" => webui::read_views::handle(req, env).await,
        "/api/clear_history" => webui::read_views::handle(req, env).await,
        "/api/taginfo.json" => webui::read_views::handle(req, env).await,
        "/api/version/current.json" => webui::read_views::handle(req, env).await,
        "/api/version/latest.json" => webui::read_views::handle(req, env).await,
        "/api/convert" => webui::job_actions::handle(req, env).await,
        "/api/update" => webui::job_actions::handle(req, env).await,
        "/api/update/start" => webui::job_actions::handle(req, env).await,
        "/api/update_by_tag" => webui::job_actions::handle(req, env).await,
        "/api/update_general_lastup" => webui::job_actions::handle(req, env).await,
        "/api/login/import" => webui::login_actions::handle(req, env).await,
        "/api/login/order" => webui::login_actions::handle(req, env).await,
        "/api/queue/clear" => webui::queue_actions::handle(req, env).await,
        "/api/cancel" => webui::queue_actions::handle(req, env).await,
        "/api/cancel_running_task" => webui::queue_actions::handle(req, env).await,
        "/api/remove_pending_task" => webui::queue_actions::handle(req, env).await,
        "/api/restore_pending_tasks" => webui::queue_actions::handle(req, env).await,
        "/api/defer_restore_pending_tasks" => webui::queue_actions::handle(req, env).await,
        "/api/reorder_pending_tasks" => webui::queue_actions::handle(req, env).await,
        "/api/freeze" => webui::row_actions::handle(req, env).await,
        "/api/novels/freeze" => webui::row_actions::handle(req, env).await,
        "/api/novels/unfreeze" => webui::row_actions::handle(req, env).await,
        "/api/novels/remove" => webui::row_actions::handle(req, env).await,
        "/api/edit_tag" => webui::tag_actions::handle(req, env).await,
        "/api/tag/change_color" => webui::tag_actions::handle(req, env).await,
        "/api/shutdown" => webui::native_only::handle(req, env).await,
        "/api/reboot" => webui::native_only::handle(req, env).await,
        "/api/folder" => webui::native_only::handle(req, env).await,
        "/api/backup" => webui::native_only::handle(req, env).await,
        "/api/backup_bookmark" => webui::native_only::handle(req, env).await,
        "/api/setting_burn" => webui::native_only::handle(req, env).await,
        "/api/csv/download" => webui::native_only::handle(req, env).await,
        "/api/csv/import" => webui::native_only::handle(req, env).await,
        "/api/mail" => webui::native_only::handle(req, env).await,
        "/api/send" => webui::native_only::handle(req, env).await,
        "/api/storage/mode" => webui::native_only::handle(req, env).await,
        "/api/get_queue_size" => webui::queue::handle(req, env).await,
        "/api/devices" => webui::settings::handle(req, env).await,
        "/widget/drag_and_drop" => webui::pages::handle(req, env).await,
        "/_rebooting" => webui::pages::handle(req, env).await,
        "/ws" => websocket::handle(req, env).await,
        _ if path.starts_with("/api/settings/") => webui::settings::handle(req, env).await,
        _ if path.starts_with("/novels/") => webui::pages::handle(req, env).await,
        _ if path.starts_with("/api/novels/") => api_novel(req, env).await,
        _ if path.starts_with("/api/login/") => api_login_host(req, env).await,
        _ if path.starts_with("/api/sites/") => api_site(req, env).await,
        _ if path.starts_with("/api/jobs/") => api_job(req, env).await,
        _ => Response::error("Not Found", 404),
    }
}

async fn api_novels(req: Request, env: Env) -> Result<Response> {
    if req.method() != Method::Get {
        return Response::error("Method Not Allowed", 405);
    }
    if let Some(response) = auth_failure(&req, &env).await {
        return response;
    }
    let services = match composition::build_services(&env).await {
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
    // 認証は native と同じく全メソッドに掛ける (書き込み系も Bearer が要る)。
    if let Some(response) = auth_failure(&req, &env).await {
        return response;
    }
    let path = req.path();
    let rest = path.strip_prefix("/api/novels/").unwrap_or_default();
    let method = req.method();

    // native `POST|DELETE /api/novels/tag` (`batch_tag` / `batch_untag`) は
    // `{id}` より先に解決される ("tag" は数値 id ではない)。
    if rest == "tag" {
        let remove = match method {
            Method::Post => false,
            Method::Delete => true,
            _ => return Response::error("Method Not Allowed", 405),
        };
        let services = match composition::build_services(&env).await {
            Ok(services) => services,
            Err(_) => return Response::error("Service unavailable", 503),
        };
        return webui::novels::batch_tag(&services, req, remove).await;
    }

    let Some((id_str, tail)) = rest.split_once('/') else {
        // bare `{id}`: GET=1 件取得, DELETE=remove_novel。
        let Ok(id) = rest.parse::<i64>() else {
            return Response::error("Not Found", 404);
        };
        return match method {
            Method::Get => api_novel_get(req, env, id).await,
            Method::Delete => {
                let services = match composition::build_services(&env).await {
                    Ok(services) => services,
                    Err(_) => return Response::error("Service unavailable", 503),
                };
                webui::novels::remove_novel(&services, id.into(), req).await
            }
            _ => Response::error("Method Not Allowed", 405),
        };
    };
    let Ok(id) = id_str.parse::<i64>() else {
        return Response::error("Not Found", 404);
    };

    // `{id}/…` のサブパス。native と同じくパスに対応するメソッド以外は 405、
    // 未対応のサブパスは 404。
    let known = matches!(
        tail,
        "download.epub" | "author_comments" | "freeze" | "unfreeze" | "tag" | "tags"
    ) || tail == "tags/remove"
        || tail.starts_with("illustrations/");
    if !known {
        return Response::error("Not Found", 404);
    }
    let services = match composition::build_services(&env).await {
        Ok(services) => services,
        Err(_) => return Response::error("Service unavailable", 503),
    };
    match tail {
        "download.epub" => match method {
            Method::Get | Method::Head => api_novel_download_epub(req, env, id).await,
            _ => Response::error("Method Not Allowed", 405),
        },
        "author_comments" => match method {
            Method::Get | Method::Head => {
                webui::novels::author_comments(&services, id.into()).await
            }
            _ => Response::error("Method Not Allowed", 405),
        },
        "freeze" => match method {
            Method::Post => webui::novels::freeze_novel(&services, id.into()).await,
            _ => Response::error("Method Not Allowed", 405),
        },
        "unfreeze" => match method {
            Method::Post => webui::novels::unfreeze_novel(&services, id.into()).await,
            _ => Response::error("Method Not Allowed", 405),
        },
        "tag" => match method {
            Method::Post => webui::novels::novel_tag(&services, id.into(), req, false).await,
            Method::Delete => webui::novels::novel_tag(&services, id.into(), req, true).await,
            _ => Response::error("Method Not Allowed", 405),
        },
        "tags" => match method {
            Method::Post => {
                webui::novels::novel_tags(&services, id.into(), req, TagAction::Add).await
            }
            Method::Put => {
                webui::novels::novel_tags(&services, id.into(), req, TagAction::Replace).await
            }
            _ => Response::error("Method Not Allowed", 405),
        },
        "tags/remove" => match method {
            Method::Post => {
                webui::novels::novel_tags(&services, id.into(), req, TagAction::Remove).await
            }
            _ => Response::error("Method Not Allowed", 405),
        },
        _ => {
            // illustrations/{name}
            let name = tail.strip_prefix("illustrations/").unwrap_or_default();
            match method {
                Method::Get | Method::Head => {
                    api_novel_illustration(req, env, id, name).await
                }
                _ => Response::error("Method Not Allowed", 405),
            }
        }
    }
}

/// GET /api/novels/:id — 1 件取得 (native `get_novel`)。
async fn api_novel_get(_req: Request, env: Env, id: i64) -> Result<Response> {
    let services = match composition::build_services(&env).await {
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
/// converted text (`novel.txt` object) at download time and stream it as the
/// response body: the book is written entry by entry, each illustration is read
/// from the object store right before the entry that embeds it, and every chunk
/// leaves as soon as it is produced. Neither the finished archive nor the image
/// set is ever held in memory.
async fn api_novel_download_epub(req: Request, env: Env, id: i64) -> Result<Response> {
    if let Some(response) = auth_failure(&req, &env).await {
        return response;
    }
    let services = match composition::build_read_services(&env).await {
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

    // 挿絵は名前とサイズだけを列挙し、バイト列は書き出し直前に 1 枚ずつ読む。
    // 列挙した名前が本文の参照と同じ形 (`挿絵/foo.jpg`) でないと Lite は解決
    // できないので、小説のプレフィックスは付けない。
    const MAX_IMAGES: usize = 512;
    const MAX_IMAGE_BYTES: u64 = 16 * 1024 * 1024;
    let Ok(illust_prefix) = narou_rs::platform::ObjectPrefix::new(format!(
        "{}/挿絵",
        keys.prefix().as_ref()
    )) else {
        return Response::error("Invalid illustration prefix", 500);
    };
    let mut image_names: Vec<String> = Vec::new();
    let mut cursor = None;
    loop {
        let mut request =
            narou_rs::platform::ObjectListRequest::new(illust_prefix.clone(), 100.try_into().unwrap());
        request.cursor = cursor.take();
        let page = match services.objects.list_page(&request).await {
            Ok(page) => page,
            Err(error) => {
                return Response::error(&format!("Object store error: {error}"), 500);
            }
        };
        for meta in page.objects {
            let name = meta.key.as_ref().rsplit('/').next().unwrap_or_default().to_string();
            if !narou_rs::epub_lite::is_supported_image(&name) {
                continue;
            }
            // 応答を始める前に断れるものは断る (1 枚は 1 回の small read で
            // 読めなければならない)。
            if image_names.len() >= MAX_IMAGES {
                return Response::error(
                    format!("Illustrations exceed the Worker budget ({MAX_IMAGES} images)"),
                    413,
                );
            }
            if meta.size > MAX_IMAGE_BYTES {
                return Response::error(
                    format!(
                        "Illustration {name} exceeds the {} MiB read limit",
                        MAX_IMAGE_BYTES / 1024 / 1024
                    ),
                    413,
                );
            }
            image_names.push(format!("挿絵/{name}"));
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }

    let options = narou_rs::epub_lite::EpubBuildOptions {
        title: record.title.clone(),
        author: record.author.clone(),
        vertical: true,
        cover_from_first_image: !image_names.is_empty(),
        assets_dir: None,
        kindle: false,
        // The dakuten font and its stylesheet are read from `preset/` on
        // disk (`dakuten_font::lite_font_assets`), which this runtime has no
        // filesystem for; the Worker EPUB keeps the engine's own fonts.
        extra_assets: Vec::new(),
    };
    let source = std::sync::Arc::new(narou_rs::epub_lite::LazyImageSource::new(
        text,
        image_names,
    ));
    let build = match narou_rs::epub_lite::build_book_from_source(source.clone(), &options) {
        Ok(build) => std::sync::Arc::new(build),
        Err(error) => return Response::error(&format!("EPUB build failed: {error}"), 500),
    };
    let sink = narou_rs::epub_lite::ChunkSink::new();
    let writer = match build.book.stream_writer(sink.clone()) {
        Ok(writer) => writer,
        Err(error) => return Response::error(&format!("EPUB write failed: {error}"), 500),
    };
    let state = EpubStreamState {
        writer: Some(writer),
        sink,
        build,
        source,
        objects: services.objects.clone(),
        prefix: keys.prefix().as_ref().to_string(),
    };
    let stream = futures::stream::unfold(state, |mut state| async move {
        loop {
            if state.writer.is_none() {
                return None;
            }
            let info = match state.writer.as_ref().and_then(|writer| writer.next_entry()) {
                Some(info) => info,
                None => {
                    let writer = state.writer.take().expect("writer is present");
                    if let Err(error) = writer.finish() {
                        return Some((Err(stream_error(error)), state));
                    }
                    let tail = state.sink.take();
                    return (!tail.is_empty()).then_some((Ok(tail), state));
                }
            };

            let bytes = match info.asset_path.as_deref() {
                Some(path) => {
                    let Some(relative) = path.strip_prefix("image/") else {
                        return Some((
                            Err(worker::Error::RustError(format!(
                                "unexpected EPUB asset path: {path}"
                            ))),
                            state,
                        ));
                    };
                    let key = match narou_rs::platform::ObjectKey::try_new(format!(
                        "{}/{}",
                        state.prefix, relative
                    )) {
                        Ok(key) => key,
                        Err(error) => return Some((Err(stream_error(error)), state)),
                    };
                    match state.objects.read_small(&key).await {
                        Ok(Some(raw)) => {
                            state.source.insert_image(relative, raw.clone());
                            let bytes = state.build.resolve(path).or(Some(raw));
                            // 保持するのは書き出し中の 1 枚分だけにする。
                            state.source.remove_image(relative);
                            bytes
                        }
                        Ok(None) => {
                            return Some((
                                Err(worker::Error::RustError(format!("missing {relative}"))),
                                state,
                            ));
                        }
                        Err(error) => return Some((Err(stream_error(error)), state)),
                    }
                }
                None => None,
            };

            let Some(writer) = state.writer.as_mut() else {
                return None;
            };
            if let Err(error) = writer.write_current(bytes.as_deref()) {
                return Some((Err(stream_error(error)), state));
            }
            let chunk = state.sink.take();
            if !chunk.is_empty() {
                return Some((Ok(chunk), state));
            }
        }
    });

    worker::ResponseBuilder::new()
        .with_header("Content-Type", "application/epub+zip")?
        .with_header(
            "Content-Disposition",
            &format!("attachment; filename=\"novel-{id}.epub\""),
        )?
        .from_stream(stream)
}

fn stream_error(error: impl std::fmt::Display) -> worker::Error {
    worker::Error::RustError(error.to_string())
}

/// 1 エントリずつ書き出しながら応答へ流すための状態。
struct EpubStreamState {
    writer: Option<narou_rs::epub_lite::EpubStreamWriter<narou_rs::epub_lite::ChunkSink>>,
    sink: narou_rs::epub_lite::ChunkSink,
    build: std::sync::Arc<narou_rs::epub_lite::EpubBuild>,
    source: std::sync::Arc<narou_rs::epub_lite::LazyImageSource>,
    objects: std::sync::Arc<dyn narou_rs::platform::ObjectStore>,
    prefix: String,
}

/// 無認証で返す health 応答。認証の要否と設定状況を機械可読で載せる
/// (CLI / Web UI / 監視が、トークン未設定とトークン不一致を区別できる)。
async fn health_payload(env: &Env, status: &str) -> worker::Result<Response> {
    let (required, configured, _) = auth_configuration(env).await;
    Response::from_json(&serde_json::json!({
        "status": status,
        "service": "narou.rs worker",
        "authentication_required": required,
        "authentication_configured": configured,
    }))
}

/// GET /api/sites — bundle 済み + ユーザー定義のサイト定義一覧。
async fn api_sites(req: Request, env: Env) -> Result<Response> {
    if let Some(response) = auth_failure(&req, &env).await {
        return response;
    }
    if req.method() != Method::Get {
        return Response::error("Method Not Allowed", 405);
    }
    let runtime = match WorkerRuntime::build(&env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("service composition failed: {error}");
            return Response::error("Service Unavailable", 503);
        }
    };
    crate::sites::list(&runtime)
        .await
        .map_err(|error| Error::RustError(error.to_string()))
}

/// PUT /api/sites/{name} — 1 件のユーザー定義を置き換える (YAML 本文)。
/// DELETE /api/sites/{name} — ユーザー定義を消して bundle に戻す。
async fn api_site(req: Request, env: Env) -> Result<Response> {
    if let Some(response) = auth_failure(&req, &env).await {
        return response;
    }
    let Some(name) = req
        .path()
        .strip_prefix("/api/sites/")
        .map(str::to_string)
    else {
        return Response::error("Not Found", 404);
    };
    if name.is_empty() {
        return Response::error("Not Found", 404);
    }
    let runtime = match WorkerRuntime::build(&env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("service composition failed: {error}");
            return Response::error("Service Unavailable", 503);
        }
    };
    crate::sites::handle(&runtime, req, Some(&name))
        .await
        .map_err(|error| Error::RustError(error.to_string()))
}

/// GET /api/novels/:id/illustrations/:name — 挿絵 1 枚。
///
/// S3 に置く構成 (`asset_backend = s3`) では **presigned URL へ 302** し、
/// 大きいオブジェクトを Worker で中継しない。D1 に置く構成ではそのまま返す。
async fn api_novel_illustration(req: Request, env: Env, id: i64, name: &str) -> Result<Response> {
    if req.method() != Method::Get {
        return Response::error("Method Not Allowed", 405);
    }
    if let Some(response) = auth_failure(&req, &env).await {
        return response;
    }
    let services = match composition::build_read_services(&env).await {
        Ok(services) => services,
        Err(_) => return Response::error("Service unavailable", 503),
    };
    let record = services
        .app
        .library
        .get(narou_rs::platform::NovelId(id))
        .await
        .map_err(|error| Error::RustError(error.to_string()))?;
    let Some(record) = record else {
        return Response::error("Not Found", 404);
    };
    let keys = match narou_rs::platform::NovelObjectKeys::new(
        &record.sitename,
        &record.file_title,
        record.use_subdirectory,
    ) {
        Ok(keys) => keys,
        Err(error) => return Response::error(&format!("Invalid key: {error}"), 500),
    };
    let key = match keys.illustration(name) {
        Ok(key) => key,
        Err(_) => return Response::error("Not Found", 404),
    };

    if let Some(s3) = services.s3_illustrations.as_ref() {
        // Worker を経由させない。URL の寿命は短くして、共有を避ける。
        let presigned = s3.presign_get_url(&key, 240);
        let url = worker::Url::parse(&presigned)
            .map_err(|error| Error::RustError(error.to_string()))?;
        return Response::redirect(url);
    }

    match services.objects.read_small(&key).await {
        Ok(Some(bytes)) => {
            let content_type =
                narou_rs::platform::content_type_for_key(&key).unwrap_or("application/octet-stream");
            Ok(worker::ResponseBuilder::new()
                .with_header("Content-Type", content_type)?
                .with_header("Cache-Control", "private, max-age=240")?
                .from_bytes(bytes)?)
        }
        Ok(None) => Response::error("Not Found", 404),
        Err(error) => Response::error(&format!("Object store error: {error}"), 500),
    }
}

/// GET /api/login — 保存済みログイン資格情報の一覧 (値は伏せる)。
/// DELETE /api/login — 全削除 (native `login_clear_all`)。
async fn api_login(req: Request, env: Env) -> Result<Response> {
    if let Some(response) = auth_failure(&req, &env).await {
        return response;
    }
    match req.method() {
        Method::Get => {}
        Method::Delete => {}
        _ => return Response::error("Method Not Allowed", 405),
    }
    let runtime = match WorkerRuntime::build(&env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("service composition failed: {error}");
            return Response::error("Service Unavailable", 503);
        }
    };
    if req.method() == Method::Delete {
        return crate::login::clear_all(&runtime)
            .await
            .map_err(|error| Error::RustError(error.to_string()));
    }
    crate::login::status(&runtime)
        .await
        .map_err(|error| Error::RustError(error.to_string()))
}

/// POST /api/login/set | /api/login/add — `{host, cookie, label?}`。
/// `append` が真なら既存の後ろに足す (native `login_set_or_add`)。
async fn api_login_set(req: Request, env: Env, append: bool) -> Result<Response> {
    if let Some(response) = auth_failure(&req, &env).await {
        return response;
    }
    if req.method() != Method::Post {
        return Response::error("Method Not Allowed", 405);
    }
    let runtime = match WorkerRuntime::build(&env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("service composition failed: {error}");
            return Response::error("Service Unavailable", 503);
        }
    };
    crate::login::set_or_add(&runtime, req, append)
        .await
        .map_err(|error| Error::RustError(error.to_string()))
}

/// DELETE /api/login/{host} — 1 ホスト分の資格情報を消す (native
/// `login_clear_host`)。`/{host}/{index}` は 1 件だけ消す (native
/// `login_clear_credential`)。
async fn api_login_host(req: Request, env: Env) -> Result<Response> {
    if let Some(response) = auth_failure(&req, &env).await {
        return response;
    }
    if req.method() != Method::Delete {
        return Response::error("Method Not Allowed", 405);
    }
    let path = req.path();
    let Some(rest) = path.strip_prefix("/api/login/") else {
        return Response::error("Not Found", 404);
    };
    // `{host}` と `{host}/{index}` の 2 形だけがルート。3 段以上は 404。
    let (host, index) = match rest.rsplit_once('/') {
        Some((host, index)) => match index.parse::<usize>() {
            Ok(index) => (host, Some(index)),
            // 最後のセグメントが数値でない = `{host}/{index}` の形に
            // 該当しない (axum の Path 抽出失敗 = 404 と同じ)。
            Err(_) => return Response::error("Not Found", 404),
        },
        None => (rest, None),
    };
    if host.is_empty() || host.contains('/') {
        return Response::error("Not Found", 404);
    }
    let runtime = match WorkerRuntime::build(&env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("service composition failed: {error}");
            return Response::error("Service Unavailable", 503);
        }
    };
    match index {
        Some(index) => crate::login::clear_credential(&runtime, host, index)
            .await
            .map_err(|error| Error::RustError(error.to_string())),
        None => crate::login::clear_host(&runtime, host)
            .await
            .map_err(|error| Error::RustError(error.to_string())),
    }
}

/// POST /api/jobs — plan a request, enqueue each discrete plan separately,
/// and return ids / invalid / duplicates / blocked.
///
/// Unsupported kinds (Send/Mail/Backup) and targetless auto-update
/// are routed to a durable `blocked` ledger state — never a subprocess,
/// never a silent ack.
async fn api_jobs(mut req: Request, env: Env) -> Result<Response> {
    if req.method() != Method::Post {
        return Response::error("Method Not Allowed", 405);
    }
    if let Some(response) = auth_failure(&req, &env).await {
        return response;
    }
    let request: narou_rs::application::JobRequest = match req.json().await {
        Ok(request) => request,
        Err(_) => return Response::error("Invalid JSON body", 400),
    };
    if let Some(reason) = narou_rs::application::validate_request_limits(&request) {
        return Response::error(format!("request exceeds payload limits: {reason}"), 400);
    }
    let runtime = match WorkerRuntime::build(&env).await {
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
    if let Some(response) = auth_failure(&req, &env).await {
        return response;
    }
    let id = req.path().strip_prefix("/api/jobs/").map(str::to_string);
    let Some(id) = id else {
        return Response::error("Not Found", 404);
    };
    let runtime = match WorkerRuntime::build(&env).await {
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

/// POST /api/admin/object-migration — D1 のオブジェクトを S3 へ写す (P0 の移行)。
///
/// body: `{"action": "copy" | "verify" | "status", "limit": 100}`。
/// 1 回の呼び出しは `limit` 件で区切り、進捗は `app_state` に残る。
async fn api_object_migration(mut req: Request, env: Env) -> Result<Response> {
    if req.method() != Method::Post {
        return Response::error("Method Not Allowed", 405);
    }
    if let Some(response) = auth_failure(&req, &env).await {
        return response;
    }

    #[derive(serde::Deserialize, Default)]
    struct Request {
        action: Option<String>,
        limit: Option<usize>,
    }
    let body: Request = req.json().await.unwrap_or_default();
    let action = body.action.unwrap_or_else(|| "status".to_string());

    let db = match env.d1("DB") {
        Ok(db) => std::sync::Arc::new(db),
        Err(error) => {
            console_log!("D1 binding missing: {error}");
            return Response::error("Service unavailable", 503);
        }
    };
    let result = if action == "status" {
        object_migration::status(&db).await
    } else {
        object_migration::run(
            &env,
            &db,
            &action,
            body.limit.unwrap_or(object_migration::DEFAULT_LIMIT),
        )
        .await
    };
    match result {
        Ok(report) => Response::from_json(&report),
        Err(error) => Response::error(format!("object migration failed: {error}"), 500),
    }
}

/// 認証の判定結果。「トークンが要る」と「トークン未設定」を区別する。
///
/// 未設定を単なる 401 にすると、デプロイ直後の「トークンを入れ忘れた」状態が
/// クライアントから見て「トークンが違う」と区別できない。CLI や Web UI が
/// 診断できるよう、機械可読なコードを返す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthState {
    /// 通す (ヘッダが正しい、またはローカル開発用に認証を外している)。
    Allowed,
    /// トークンは設定済みで、ヘッダが無い/違う。
    Required,
    /// 認証が必要なのに `NAROU_ADMIN_TOKEN` が無い (fail-closed)。
    NotConfigured,
}

/// 認証の設定状況 `(認証が必要か, トークンが設定済みか, トークン)`。
async fn auth_configuration(env: &Env) -> (bool, bool, Option<String>) {
    // ローカル開発用の抜け道。既定は "true" (認証する)。
    let required = env
        .var("NAROU_AUTH_REQUIRED")
        .map(|value| value.to_string())
        .unwrap_or_else(|_| "true".to_string());
    if required.eq_ignore_ascii_case("false") {
        return (false, true, None);
    }
    let token = crate::secrets::value(env, "NAROU_ADMIN_TOKEN").await;
    (true, token.is_some(), token)
}

async fn auth_state(req: &Request, env: &Env) -> AuthState {
    let (required, configured, token) = auth_configuration(env).await;
    if !required {
        return AuthState::Allowed;
    }
    if !configured {
        return AuthState::NotConfigured;
    }
    let Some(secret) = token else {
        return AuthState::NotConfigured;
    };
    let expected = format!("Bearer {secret}");
    let Ok(actual) = req.headers().get("authorization") else {
        return AuthState::Required;
    };
    let Some(actual) = actual else {
        return AuthState::Required;
    };
    if actual.as_bytes().ct_eq(expected.as_bytes()).into() {
        AuthState::Allowed
    } else {
        AuthState::Required
    }
}

/// 認証を要求し、失敗していればその応答を返す。
async fn auth_failure(req: &Request, env: &Env) -> Option<worker::Result<Response>> {
    match auth_state(req, env).await {
        AuthState::Allowed => None,
        AuthState::Required => Some(json_error(401, "authentication_required", None)),
        AuthState::NotConfigured => Some(json_error(
            500,
            "authentication_not_configured",
            Some("NAROU_ADMIN_TOKEN is not set for this Worker"),
        )),
    }
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

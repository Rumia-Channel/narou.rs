#![cfg(target_arch = "wasm32")]
// wasm は単一スレッドで、プラットフォーム抽象 (ObjectStore / AssetStore /
// NovelRepository 等) は `PlatformService` の wasm 実装どおり Send + Sync を
// 要求しない。そのため `Arc<dyn ...>` はこのクレートでは常に非 Send/Sync に
// なり、この lint は常に誤検知になる。
#![allow(clippy::arc_with_non_send_sync)]

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
mod db_handle;
mod d1_cookie_store;
mod d1_object_store;
mod d1_repository;
mod executor;
pub mod http;
mod isolate_cache;
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
use narou_rs::application::settings::SettingsStore;
use narou_rs::application::TagAction;
use narou_rs::setting_core::SettingScope;
use std::collections::HashMap;

use crate::composition::{WorkerRuntime, check_ready};
use crate::db_handle::DbHandle;
use crate::d1_repository::D1SettingsStore;

#[event(fetch)]
pub async fn main(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    // native (axum) は GET ルートに HEAD も通す。Worker の各ハンドラは
    // GET/POST 前提で 405 を返すので、HEAD は GET として処理し、応答から
    // 本文だけを落として返す (method とヘッダを引き継いだ複製を作る)。
    if req.method() == Method::Head {
        let get_request = request_as_get(&req)?;
        return route(get_request, env).await.and_then(head_response);
    }
    route(req, env).await
}

/// HEAD を GET として処理するための複製 (本文なし・Content-Length なし)。
fn request_as_get(req: &Request) -> Result<Request> {
    let url = req.url().map_err(|error| Error::RustError(error.to_string()))?;
    let headers = req.headers().clone();
    headers.delete("content-length")?;
    let mut init = RequestInit::new();
    init.with_method(Method::Get);
    init.with_headers(headers);
    Request::new_with_init(url.as_str(), &init)
        .map_err(|error| Error::RustError(error.to_string()))
}

/// HEAD 応答: ステータスとヘッダはそのまま、本文だけ落とす。
fn head_response(response: Response) -> Result<Response> {
    let status = response.status_code();
    let headers = response.headers().clone();
    headers.delete("content-length")?;
    worker::ResponseBuilder::new()
        .with_status(status)
        .with_headers(headers)
        .from_bytes(Vec::new())
}

async fn route(req: Request, env: Env) -> Result<Response> {
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
        // native `src/web/mod.rs` では `{id}` より先に解決される数値以外の
        // `/api/novels/*` 路 (native: `novels_count` / `all_novel_ids`)。
        "/api/novels/count" => api_novels_count(req, env).await,
        "/api/novels/all_ids" => api_novels_all_ids(req, env).await,
        // ブラウザ用の入口 (migration plan §3.2): API/CI は Bearer のまま、
        // ブラウザは `/login` で受け取った HttpOnly Cookie で通す。
        "/login" => auth_login_page(req, env).await,
        "/api/auth/login" => api_auth_login(req, env).await,
        "/api/login" => api_login(req, env).await,
        "/api/sites" => api_sites(req, env).await,
        // Cookie の直接登録 (set/add) は廃止: 取り込みとブラウザ取得だけが
        // 登録経路 (native `src/web/mod.rs` と同じルート構成)。
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
        // native `jobs::api_download_force`: `{"ids": [...]}` を
        // `force: true` の `/api/download` に変換して流し込む。
        "/api/download_force" => api_download_force(req, env).await,
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
        "/api/login/rename" => webui::login_actions::handle(req, env).await,
        "/api/login/order" => webui::login_actions::handle(req, env).await,
        "/api/queue/clear" => webui::queue_actions::handle(req, env).await,
        // native `jobs::queue_cancel`: running 中断 + pending 消去の合成路。
        "/api/queue/cancel" => api_queue_cancel(req, env).await,
        "/api/cancel" => webui::queue_actions::handle(req, env).await,
        "/api/cancel_running_task" => webui::queue_actions::handle(req, env).await,
        "/api/remove_pending_task" => webui::queue_actions::handle(req, env).await,
        "/api/restore_pending_tasks" => webui::queue_actions::handle(req, env).await,
        "/api/defer_restore_pending_tasks" => webui::queue_actions::handle(req, env).await,
        // native `jobs::confirm_running_tasks`: `rerun` で復元/延期に分岐。
        "/api/confirm_running_tasks" => api_confirm_running_tasks(req, env).await,
        "/api/reorder_pending_tasks" => webui::queue_actions::handle(req, env).await,
        "/api/freeze" => webui::row_actions::handle(req, env).await,
        "/api/novels/freeze" => webui::row_actions::handle(req, env).await,
        "/api/novels/unfreeze" => webui::row_actions::handle(req, env).await,
        "/api/novels/remove" => webui::row_actions::handle(req, env).await,
        // native の同義エイリアス (src/web/mod.rs): canonical ルートの
        // ハンドラをそのまま使う (`alias_route` が本文ごと再ディスパッチする)。
        "/api/remove" => {
            alias_route(req, env, "/api/novels/remove", webui::row_actions::handle).await
        }
        "/api/remove_with_file" => api_remove_with_file(req, env).await,
        "/api/freeze_on" => {
            alias_route(req, env, "/api/novels/freeze", webui::row_actions::handle).await
        }
        "/api/freeze_off" => {
            alias_route(req, env, "/api/novels/unfreeze", webui::row_actions::handle).await
        }
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
        // 小説本文の版履歴を要する差分 API: Worker は版履歴を持たないので
        // `native_only.rs` の 501 (`not_supported_on_worker`) で拒否する。
        "/api/diff" => webui::native_only::handle(req, env).await,
        "/api/diff_history" => webui::native_only::handle(req, env).await,
        "/api/diff_show" => webui::native_only::handle(req, env).await,
        "/api/diff_restore" => webui::native_only::handle(req, env).await,
        "/api/diff_merge" => webui::native_only::handle(req, env).await,
        // native `misc.rs` / `jobs.rs` の読取系 (ログ・URL 正規表現・GIF)。
        "/api/log/recent" => api_log_recent(req, env).await,
        "/api/validate_url_regexp_list" => api_validate_url_regexp_list(req, env).await,
        "/api/downloadable.gif" => api_downloadable_gif(req, env).await,
        "/api/get_queue_size" => webui::queue::handle(req, env).await,
        "/api/devices" => webui::settings::handle(req, env).await,
        "/widget/drag_and_drop" => webui::pages::handle(req, env).await,
        "/_rebooting" => webui::pages::handle(req, env).await,
        "/ws" => websocket::handle(req, env).await,
        _ if path.starts_with("/api/settings/") => webui::settings::handle(req, env).await,
        _ if path.starts_with("/novels/") => webui::pages::handle(req, env).await,
        _ if path.starts_with("/api/novels/") => api_novel(req, env).await,
        _ if path.starts_with("/api/login/") => api_login_site(req, env).await,
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

/// GET /api/novels/count — native `src/web/novels.rs::novels_count`。
async fn api_novels_count(req: Request, env: Env) -> Result<Response> {
    if !matches!(req.method(), Method::Get | Method::Head) {
        return Response::error("Method Not Allowed", 405);
    }
    if let Some(response) = auth_failure(&req, &env).await {
        return response;
    }
    let services = match composition::build_services(&env).await {
        Ok(services) => services,
        Err(_) => return Response::error("Service unavailable", 503),
    };
    webui::novels::count(&services).await
}

/// GET /api/novels/all_ids — native `src/web/misc.rs::all_novel_ids`。
async fn api_novels_all_ids(req: Request, env: Env) -> Result<Response> {
    if !matches!(req.method(), Method::Get | Method::Head) {
        return Response::error("Method Not Allowed", 405);
    }
    if let Some(response) = auth_failure(&req, &env).await {
        return response;
    }
    let services = match composition::build_services(&env).await {
        Ok(services) => services,
        Err(_) => return Response::error("Service unavailable", 503),
    };
    webui::novels::all_ids(&services).await
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
        // native の `Path<i64>` は数値でないセグメントを 400 で弾く
        // (404 ではない) ので、ここも同じステータスに合わせる。
        let Ok(id) = rest.parse::<i64>() else {
            return json_error(400, "invalid_path_parameter", None);
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
    // native の `Path<i64>` と同じく、数値でない id は 400 (404 ではない)。
    let Ok(id) = id_str.parse::<i64>() else {
        return json_error(400, "invalid_path_parameter", None);
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
        Err(error) => return Response::error(format!("Repository error: {error}"), 500),
    };
    let keys = match narou_rs::platform::NovelObjectKeys::new(
        &record.sitename,
        &record.file_title,
        record.use_subdirectory,
    ) {
        Ok(keys) => keys,
        Err(error) => return Response::error(format!("Invalid key: {error}"), 500),
    };

    let local_map = load_local_settings_map(&env).await;
    let filename = match epub_download_filename(
        services.objects.clone(),
        &record,
        &keys,
        &local_map,
    )
    .await
    {
        Ok(name) => name,
        Err(error) => return Response::error(format!("Naming error: {error}"), 500),
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
        Err(error) => return Response::error(format!("Object store error: {error}"), 500),
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
                return Response::error(format!("Object store error: {error}"), 500);
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
        // `convert.rotate-image` (D1 ローカル設定)。未指定は INI 準拠
        // (この経路は挿絵を回転しない)。指定時だけ Lite の INI へ
        // `RotateImage` を上書きし、書き出し直前に寸法から回転角を決める。
        rotate_image: local_map
            .get("convert.rotate-image")
            .and_then(setting_text)
            .and_then(|value| narou_rs::converter::settings::ImageRotation::from_setting(&value)),
    };
    let source = std::sync::Arc::new(narou_rs::epub_lite::LazyImageSource::new(
        text,
        image_names,
    ));
    let build = match narou_rs::epub_lite::build_book_from_source(source.clone(), &options) {
        Ok(build) => std::sync::Arc::new(build),
        Err(error) => return Response::error(format!("EPUB build failed: {error}"), 500),
    };
    let sink = narou_rs::epub_lite::ChunkSink::new();
    let writer = match build.book.stream_writer(sink.clone()) {
        Ok(writer) => writer,
        Err(error) => return Response::error(format!("EPUB write failed: {error}"), 500),
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
            state.writer.as_ref()?;
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

            let writer = state.writer.as_mut()?;
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
            &format!("attachment; filename=\"{}\"", sanitize_header_filename(&filename)),
        )?
        .from_stream(stream)
}

/// D1 のローカルスコープ設定を読む (fs が無いので `app_state` 経由)。
async fn load_local_settings_map(env: &Env) -> HashMap<String, serde_yaml::Value> {
    let Ok(db) = env.d1("DB") else {
        return HashMap::new();
    };
    let handle = DbHandle::ui(std::sync::Arc::new(db));
    D1SettingsStore::new(handle)
        .load(SettingScope::Local)
        .await
        .unwrap_or_default()
}

/// 設定マップの値を文字列にする。文字列以外 (bool/数値) も文字列表現で返す
/// (native の `compat::yaml_value_to_string` と同じ扱い)。
fn setting_text(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// `download.epub` の Content-Disposition 名を native の保存済み EPUB と同じ
/// 規則 (`converter::output` の変換出力名に `.epub` を付けたもの) で決める。
///
/// 反映される設定は native の変換時と同じ: `setting.ini` /
/// `default.*` / `force.*` (`NovelSettings::from_sources`) と
/// `convert.filename-to-ncode` / `ebook-filename-length-limit`
/// (`OutputNamingEnv::from_local_map`)。fs が無いので local 設定は D1 の
/// `app_state` スコープ `local` から読む。
async fn epub_download_filename(
    objects: std::sync::Arc<dyn narou_rs::platform::ObjectStore>,
    record: &narou_rs::db::NovelRecord,
    keys: &narou_rs::platform::NovelObjectKeys,
    local_map: &HashMap<String, serde_yaml::Value>,
) -> narou_rs::error::Result<String> {
    let ini = match objects.read_small(&keys.setting()).await? {
        Some(bytes) => {
            narou_rs::converter::ini::IniData::load(&String::from_utf8_lossy(&bytes))
        }
        None => narou_rs::converter::ini::IniData::new(),
    };
    let settings = narou_rs::converter::settings::NovelSettings::from_sources(
        Some(record.id),
        &record.title,
        &record.author,
        &ini,
        local_map,
        false,
        false,
    );
    // 命名には TocObject の title/author/toc_url だけが使われる。
    // `from_sources` が novel_title/novel_author を record で埋めるため、
    // native (`generate_epub_on_demand` が toc.yaml から組み立てる TocObject)
    // と同じ名になる最小の TocObject を record から組み立てる。
    let toc = narou_rs::downloader::TocObject {
        title: record.title.clone(),
        author: record.author.clone(),
        toc_url: record.toc_url.clone(),
        story: None,
        subtitles: Vec::new(),
        novel_type: Some(record.novel_type),
    };
    let naming_env = narou_rs::converter::output::OutputNamingEnv::from_local_map(local_map);
    Ok(narou_rs::converter::output::epub_output_filename(
        &settings,
        &toc,
        Some(record),
        &naming_env,
    ))
}

/// Content-Disposition に埋め込むため、引用符と制御文字を `_` に置き換える。
/// (native `sanitize_content_disposition` と同じ規則)
fn sanitize_header_filename(filename: &str) -> String {
    filename
        .chars()
        .map(|c| {
            if c == '"' || c.is_ascii_control() {
                '_'
            } else {
                c
            }
        })
        .collect()
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
    let runtime = match WorkerRuntime::build_ui(&env).await {
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
    let runtime = match WorkerRuntime::build_ui(&env).await {
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
        Err(error) => return Response::error(format!("Invalid key: {error}"), 500),
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
        Err(error) => Response::error(format!("Object store error: {error}"), 500),
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
    let runtime = match WorkerRuntime::build_ui(&env).await {
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

/// DELETE /api/login/{site} — 1 サイト分のログインを消す (native
/// `login_clear_site`)。`/{site}/{index}` は 1 件だけ消す (native
/// `login_clear_group`)。
async fn api_login_site(req: Request, env: Env) -> Result<Response> {
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
    // `{site}` と `{site}/{index}` の 2 形だけがルート。3 段以上は 404。
    let (site, index) = match rest.rsplit_once('/') {
        Some((site, index)) => match index.parse::<usize>() {
            Ok(index) => (site, Some(index)),
            // 最後のセグメントが数値でない = `{site}/{index}` の形に
            // 該当しない (axum の Path 抽出失敗 = 404 と同じ)。
            Err(_) => return Response::error("Not Found", 404),
        },
        None => (rest, None),
    };
    if site.is_empty() || site.contains('/') {
        return Response::error("Not Found", 404);
    }
    let runtime = match WorkerRuntime::build_ui(&env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("service composition failed: {error}");
            return Response::error("Service Unavailable", 503);
        }
    };
    match index {
        Some(index) => crate::login::clear_group(&runtime, site, index)
            .await
            .map_err(|error| Error::RustError(error.to_string())),
        None => crate::login::clear_site(&runtime, site)
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
    let runtime = match WorkerRuntime::build_ui(&env).await {
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
///
/// `record_rejected` が書く `kind='unknown'` の poison 行も閲覧できるよう
/// `ledger::get_view` を使う (実行経路の `JobQueue::get` は計画が必須で
/// 未知の kind を拒否するが、閲覧で 500 にはしない)。
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
    let runtime = match WorkerRuntime::build_ui(&env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("service composition failed: {error}");
            return Response::error("Service unavailable", 503);
        }
    };
    match runtime.ledger.get_view(&narou_rs::application::JobId(id)).await {
        Ok(Some(view)) => Response::from_json(&view),
        Ok(None) => Response::error("Not Found", 404),
        Err(error) => Response::error(format!("ledger read failed: {error}"), 500),
    }
}

// ---------------------------------------------------------------------------
// native の同義エイリアス (`src/web/mod.rs`): 本文・応答形・ステータスは
// canonical ルートのハンドラがそのまま担う。各 webui モジュールは受取パス
// で振り分けているので、エイリアスは canonical パスへ作り直したリクエスト
// を再ディスパッチして共有する (実装の複製を避けるため)。
// ---------------------------------------------------------------------------

/// canonical `path` 向けに `req` を作り直す。method とヘッダ (認証・
/// Origin 検査) は呼び出し元のまま運び、`body` を本文にする
/// (`None` = 本文なし)。URL は呼び出し元の URL を基に組むので
/// 同一オリジン判定と残りのクエリがそのまま効く。
fn forwarded_request(req: &Request, path: &str, body: Option<String>) -> Result<Request> {
    let mut url = req.url().map_err(|error| Error::RustError(error.to_string()))?;
    url.set_path(path);
    let headers = req.headers().clone();
    // 本文を作り直すので、元の Content-Length は新しい本文と食い違う。
    // そのまま運ぶと workerd が古い長さで本文を切ることがあるため必ず外す
    // (新しい長さは本文から算定される)。
    headers.delete("content-length")?;
    let mut init = RequestInit::new();
    init.with_method(req.method());
    init.with_headers(headers);
    if let Some(body) = body {
        init.with_body(Some(wasm_bindgen::JsValue::from_str(&body)));
    }
    Request::new_with_init(url.as_str(), &init)
        .map_err(|error| Error::RustError(error.to_string()))
}

/// 本文まで canonical と同じエイリアス (`/api/remove` → `/api/novels/remove`
/// など): 呼び出し元の本文をそのまま運ぶ。読めない本文は native と同様に
/// 下段ハンドラの 400 判定へ回る。
async fn alias_route<F, Fut>(req: Request, env: Env, path: &str, handler: F) -> Result<Response>
where
    F: FnOnce(Request, Env) -> Fut,
    Fut: Future<Output = worker::Result<Response>>,
{
    let mut req = req;
    let body = req.text().await.unwrap_or_default();
    let forwarded = forwarded_request(&req, path, Some(body))?;
    handler(forwarded, env).await
}

/// native `src/web/state.rs` の `IdsBody` (`/api/download_force` 本文)。
#[derive(Debug, serde::Deserialize)]
struct IdsBody {
    ids: Vec<serde_json::Value>,
}

/// POST /api/download_force — native `src/web/jobs.rs::api_download_force`:
/// `{"ids": [...]}` を `force: true` の `DownloadBody` に組み替えて
/// canonical の `/api/download` へ流す (計画・キュー投入は下段に任せる)。
async fn api_download_force(mut req: Request, env: Env) -> Result<Response> {
    if req.method() != Method::Post {
        return Response::error("Method Not Allowed", 405);
    }
    // native はミドルウェアで認証してから `Json<IdsBody>` を解くので、こちらも
    // 本文を解く前に認証を通す (下段の `/api/download` も同じ認証を再実行する)。
    if let Some(response) = auth_failure(&req, &env).await {
        return response;
    }
    let body: IdsBody = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };
    // native と同じ id → 文字列変換 (数値はそのまま、それ以外は JSON 表現)。
    let targets = body
        .ids
        .iter()
        .map(|value| match value {
            serde_json::Value::Number(number) => number.to_string(),
            serde_json::Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>();
    let download_body = json!({ "targets": targets, "force": true }).to_string();
    let forwarded = forwarded_request(&req, "/api/download", Some(download_body))?;
    webui::download::handle(forwarded, env).await
}

/// POST /api/remove_with_file — native `src/web/batch.rs::
/// batch_remove_with_file`: 本文の `with_file` を問わず常に `true` で
/// canonical の `/api/novels/remove` へ渡す。
async fn api_remove_with_file(mut req: Request, env: Env) -> Result<Response> {
    if req.method() != Method::Post {
        return Response::error("Method Not Allowed", 405);
    }
    let text = req.text().await.unwrap_or_default();
    let body = match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(mut value) if value.is_object() => {
            value["with_file"] = serde_json::Value::Bool(true);
            value.to_string()
        }
        // オブジェクト以外 (不正 JSON・配列・空) は素通しして、下段の
        // `BatchIdsBody` 解析が native と同じ 400 を返すようにする。
        _ => text,
    };
    let forwarded = forwarded_request(&req, "/api/novels/remove", Some(body))?;
    webui::row_actions::handle(forwarded, env).await
}

/// POST /api/queue/cancel — native `src/web/jobs.rs::queue_cancel`:
/// running を中断して pending を消す (=`/api/cancel` の後に
/// `/api/queue/clear`)。native と同順 (kill → clear) に実行し、
/// どちらも成功して初めて native と同じ `{"success": true,
/// "message": "キャンセルしました"}` を返す。
async fn api_queue_cancel(req: Request, env: Env) -> Result<Response> {
    let mut cancel = webui::queue_actions::handle(
        forwarded_request(&req, "/api/cancel", None)?,
        env.clone(),
    )
    .await?;
    if cancel.status_code() != 200 {
        return Ok(cancel);
    }
    let cancel_value: serde_json::Value = cancel.json().await?;
    if cancel_value.get("success") != Some(&json!(true)) {
        return Response::from_json(&cancel_value);
    }
    let mut clear =
        webui::queue_actions::handle(forwarded_request(&req, "/api/queue/clear", None)?, env)
            .await?;
    if clear.status_code() != 200 {
        return Ok(clear);
    }
    let clear_value: serde_json::Value = clear.json().await?;
    if clear_value.get("success") != Some(&json!(true)) {
        return Response::from_json(&clear_value);
    }
    // native `queue_cancel` の応答 (= cancel の成功応答と同じ形)。
    Response::from_json(&cancel_value)
}

/// native `src/web/state.rs::ConfirmRunningTasksBody`。
#[derive(Debug, serde::Deserialize)]
struct ConfirmRunningTasksBody {
    #[serde(default)]
    rerun: Option<String>,
}

/// POST /api/confirm_running_tasks — native `src/web/jobs.rs::
/// confirm_running_tasks`: `rerun == "true"` なら停止時に running だった
/// ジョブの復元 (`/api/restore_pending_tasks`)、それ以外は延期
/// (`/api/defer_restore_pending_tasks`)。応答形も下段のまま native と一致
/// (`{"status":"ok"[, "count"]}` / 失敗時 `{"error": ...}` を 200 で)。
async fn api_confirm_running_tasks(mut req: Request, env: Env) -> Result<Response> {
    if req.method() != Method::Post {
        return Response::error("Method Not Allowed", 405);
    }
    if let Some(response) = auth_failure(&req, &env).await {
        return response;
    }
    let body: ConfirmRunningTasksBody = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };
    let path = if body.rerun.as_deref() == Some("true") {
        "/api/restore_pending_tasks"
    } else {
        "/api/defer_restore_pending_tasks"
    };
    // 下段は本文を読まないので無本文で再ディスパッチする。
    let forwarded = forwarded_request(&req, path, None)?;
    webui::queue_actions::handle(forwarded, env).await
}

/// GET /api/log/recent — native `src/web/misc.rs::recent_logs`
/// (`{"logs": [...]}`)。worker には PushServer のログバッファが無い
/// (`webui/read_views.rs` の `console_history` と同じ扱い): 保持している
/// ログが無いので空配列を返す = 「直近ログ 0 件」という事実。偽の行は
/// 返さないし、UI のログ欄を 501 で壊さない。
async fn api_log_recent(req: Request, env: Env) -> Result<Response> {
    if !matches!(req.method(), Method::Get | Method::Head) {
        return Response::error("Method Not Allowed", 405);
    }
    if let Some(response) = auth_failure(&req, &env).await {
        return response;
    }
    Response::from_json(&json!({ "logs": [] }))
}

/// GET /api/validate_url_regexp_list — native `src/web/misc.rs::
/// validate_url_regexp_list`: 全サイト定義 (bundle + ユーザー定義) の URL
/// 検証正規表現を 1 つの JSON 配列で返す。native の
/// `site_definitions.url_patterns_for_validation()` と同じ flat_map。
async fn api_validate_url_regexp_list(req: Request, env: Env) -> Result<Response> {
    if !matches!(req.method(), Method::Get | Method::Head) {
        return Response::error("Method Not Allowed", 405);
    }
    if let Some(response) = auth_failure(&req, &env).await {
        return response;
    }
    let runtime = match WorkerRuntime::build_ui(&env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("service composition failed: {error}");
            return Response::error("Service unavailable", 503);
        }
    };
    let settings = match crate::bundled_sites::load_site_settings(&runtime.objects()).await {
        Ok(settings) => settings,
        Err(error) => {
            // native は定義を起動時スナップショットから読むが、worker は
            // ユーザー定義をオブジェクトストアから読む。読めないときに空を
            // 返すと正規表現検証が全滅する (通す誤判定) ので 503。
            console_log!("site definitions unavailable: {error}");
            return Response::error("Service unavailable", 503);
        }
    };
    let patterns = settings
        .iter()
        .flat_map(|setting| setting.url_patterns_for_validation())
        .collect::<Vec<_>>();
    Response::from_json(&json!(patterns))
}

/// native `src/web/jobs.rs` の `TRANSPARENT_GIF` (1x1 透明 GIF)。
const TRANSPARENT_GIF: &[u8] = &[
    71, 73, 70, 56, 57, 97, 1, 0, 1, 0, 128, 0, 0, 0, 0, 0, 255, 255, 255, 33, 249, 4, 1, 0, 0, 0,
    0, 44, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 68, 1, 0, 59,
];

/// GET /api/downloadable.gif — native `src/web/jobs.rs::
/// api_downloadable_gif`。native は `target` の登録状態を計算した値を
/// 破棄して (`let _number`) 常に同じ透明 GIF を返すので、worker もクエリ
/// を問わず同じバイト列を返す (native の画像切替未実装と同じ挙動)。
async fn api_downloadable_gif(req: Request, env: Env) -> Result<Response> {
    if !matches!(req.method(), Method::Get | Method::Head) {
        return Response::error("Method Not Allowed", 405);
    }
    if let Some(response) = auth_failure(&req, &env).await {
        return response;
    }
    let mut response = Response::from_bytes(TRANSPARENT_GIF.to_vec())?;
    response.headers_mut().set("content-type", "image/gif")?;
    response
        .headers_mut()
        .set("cache-control", "no-store")?;
    Ok(response)
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
        // 管理系の移行処理は常に primary へ (進捗を app_state に書き戻す)。
        Ok(db) => crate::db_handle::DbHandle::primary(std::sync::Arc::new(db)),
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

/// ブラウザ用トークン Cookie の名前 (migration plan §3.2)。
const AUTH_COOKIE_NAME: &str = "narou_api_token";

/// Worker 専用のログインページ。assets には置かない (`/login` は assets に
/// 無いので Worker が受ける)。
const LOGIN_PAGE: &str = include_str!("login.html");

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
    if bearer_matches(req, &secret) {
        return AuthState::Allowed;
    }
    // ブラウザは HttpOnly Cookie でトークンを運ぶ (migration plan §3.2)。
    // Cookie は自動で付く資格情報なので、同一オリジン検査を併せて要求する
    // (SameSite=Lax と二重の CSRF 対策)。
    if cookie_matches(req, &secret) && same_origin(req) {
        return AuthState::Allowed;
    }
    AuthState::Required
}

/// `Authorization: Bearer <secret>` の定数時間照合。
fn bearer_matches(req: &Request, secret: &str) -> bool {
    let Ok(Some(actual)) = req.headers().get("authorization") else {
        return false;
    };
    let expected = format!("Bearer {secret}");
    actual.as_bytes().ct_eq(expected.as_bytes()).into()
}

/// トークン Cookie の値を復号し、`secret` と定数時間で照合する。
/// Cookie は base64url で持つため、トークンに `;` などが含まれても壊れない。
fn cookie_matches(req: &Request, secret: &str) -> bool {
    use base64::Engine as _;

    let Some(value) = request_cookie(req, AUTH_COOKIE_NAME) else {
        return false;
    };
    let Ok(decoded) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(value.as_bytes())
    else {
        return false;
    };
    decoded.ct_eq(secret.as_bytes()).into()
}

/// `Cookie` ヘッダから 1 件だけ取り出す。
fn request_cookie(req: &Request, name: &str) -> Option<String> {
    let cookies = req.headers().get("cookie").ok().flatten()?;
    cookies.split(';').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key.trim() == name).then(|| value.trim().to_string())
    })
}

/// Cookie 認証時の同一オリジン検査。`Origin` が無いリクエスト (トップレベル
/// 遷移や非ブラウザ) は通し、ある場合はリクエスト URL の origin と一致を要求する。
fn same_origin(req: &Request) -> bool {
    let Ok(Some(origin)) = req.headers().get("origin") else {
        return true;
    };
    let Ok(url) = req.url() else {
        return false;
    };
    url.origin().ascii_serialization() == origin
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

/// `GET /login` — Worker 専用のログインページ。
///
/// 認証不要 (トークンを配る入口そのもの)。既に認証済み (Cookie / Bearer) なら
/// `?return=` の同一サイト内パスへ戻す。`NAROU_AUTH_REQUIRED=false` の構成でも
/// 認証済み扱いになるので UI へ戻る。
async fn auth_login_page(req: Request, env: Env) -> Result<Response> {
    if req.method() != Method::Get && req.method() != Method::Head {
        return json_error(405, "method_not_allowed", None);
    }
    if matches!(auth_state(&req, &env).await, AuthState::Allowed) {
        let target = login_return_target(&req);
        let origin = req
            .url()
            .map(|url| url.origin().ascii_serialization())
            .unwrap_or_default();
        let url = Url::parse(&format!("{origin}{target}"))
            .map_err(|error| Error::RustError(error.to_string()))?;
        return Response::redirect(url);
    }
    Response::from_html(LOGIN_PAGE)
}

/// `?return=` の戻り先。オープンリダイレクト防止のため、同一サイト内の絶対
/// パス (`/` で始まり `//` と `\` を含まない) だけを許可する。
fn login_return_target(req: &Request) -> String {
    let target = req
        .url()
        .ok()
        .and_then(|url| query_param(&url, "return"));
    match target {
        Some(target)
            if target.starts_with('/')
                && !target.starts_with("//")
                && !target.contains('\\') =>
        {
            target
        }
        _ => "/".to_string(),
    }
}

/// `POST /api/auth/login` — ブラウザにトークン Cookie を配る。
///
/// Bearer を持てないブラウザ UI の入口。本文のトークンを定数時間で照合し、
/// 一致したときだけ HttpOnly / SameSite=Lax の Cookie を設定する。API/CI の
/// Bearer 経路と `NAROU_AUTH_REQUIRED=false` の構成は従来どおり。
async fn api_auth_login(mut req: Request, env: Env) -> Result<Response> {
    use base64::Engine as _;

    if req.method() != Method::Post {
        return json_error(405, "method_not_allowed", None);
    }
    // Cookie を付ける要求なので、他サイトのページから叩かせない。
    if !same_origin(&req) {
        return json_error(
            403,
            "cross_origin_rejected",
            Some("login requires a same-origin request"),
        );
    }
    let (required, configured, token) = auth_configuration(&env).await;
    if !required {
        // 認証を無効にしている構成では何も配らない (そのまま通るため)。
        return Response::from_json(&json!({ "success": true }));
    }
    let Some(secret) = token else {
        return json_error(
            500,
            "authentication_not_configured",
            Some("NAROU_ADMIN_TOKEN is not set for this Worker"),
        );
    };
    if !configured {
        return json_error(
            500,
            "authentication_not_configured",
            Some("NAROU_ADMIN_TOKEN is not set for this Worker"),
        );
    }
    #[derive(serde::Deserialize)]
    struct LoginBody {
        token: String,
    }
    let body: LoginBody = match req.json().await {
        Ok(body) => body,
        Err(_) => {
            return json_error(400, "bad_request", Some("expected {\"token\": \"...\"}"));
        }
    };
    if !bool::from(body.token.as_bytes().ct_eq(secret.as_bytes())) {
        return json_error(
            401,
            "authentication_required",
            Some("管理トークンが正しくありません"),
        );
    }
    let secure = req.url().map(|url| url.scheme() == "https").unwrap_or(true);
    let mut cookie = format!(
        "{AUTH_COOKIE_NAME}={}; Path=/; HttpOnly; SameSite=Lax",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret.as_bytes())
    );
    if secure {
        cookie.push_str("; Secure");
    }
    let response = worker::ResponseBuilder::new()
        .with_header("Set-Cookie", &cookie)?
        .with_header("Cache-Control", "no-store")?
        .from_json(&json!({ "success": true }))?;
    Ok(response)
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

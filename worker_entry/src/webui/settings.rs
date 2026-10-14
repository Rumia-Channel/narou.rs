//! 個別小説の設定 API (native: `src/web/novel_settings.rs`)。
//!
//! - `GET /api/settings/{id}` — native `get_settings` と同じ形
//!   (`{id, title, author, settings[], replace_patterns[]}`)。
//! - `POST /api/settings/{id}` — native `save_settings` と同じ受理形・
//!   エラーメッセージ・保存済み項目の ACK・部分失敗の応答。
//! - `GET /api/devices` — native `list_devices` と同じ形
//!   (`{devices: [{name, available}]}`)。値は共有の
//!   `application::webui::capabilities::worker_devices()` が唯一の定義で、
//!   Worker が配信できる Lite の EPUB だけ `true` (外部プロセス・ローカル FS
//!   前提の出力や、ダウンロード経路の無い text は `false`)。
//!
//! 値の解析・検証は native と同じ共有実装
//! (`narou_rs::application::novel_settings`) を使う。

use narou_rs::application::AppServices;
use narou_rs::application::novel_settings::parse_settings_patch;
use narou_rs::application::webui::capabilities::worker_devices;
use worker::{Env, Method, Request, Response};

use super::{application_error_response, json_error};

/// `/api/settings/{id}` と `/api/devices` の振り分け。
pub async fn handle(mut req: Request, env: Env) -> worker::Result<Response> {
    let path = req.path();
    enum Route {
        GetSettings(i64),
        SaveSettings(i64),
        Devices,
    }
    let route = match (req.method(), path.as_str()) {
        (Method::Get, "/api/devices") => Route::Devices,
        (_, "/api/devices") => return json_error(405, "method_not_allowed", None),
        (method, _) => {
            let Some(id) = path
                .strip_prefix("/api/settings/")
                .and_then(|rest| rest.parse::<i64>().ok())
            else {
                return json_error(404, "not_found", None);
            };
            match method {
                Method::Get => Route::GetSettings(id),
                Method::Post => Route::SaveSettings(id),
                _ => return json_error(405, "method_not_allowed", None),
            }
        }
    };

    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    match route {
        Route::GetSettings(id) => {
            let services = match crate::composition::build_services(&env).await {
                Ok(services) => services,
                Err(_) => return Response::error("Service unavailable", 503),
            };
            get_settings(&services, id).await
        }
        Route::SaveSettings(id) => {
            // 設定保存は setting.ini / replace.txt の読み込み→書き換えなので
            // primary 直行・キャッシュ無しのサービスを使う。
            let services = match crate::composition::build_mutation_services(&env).await {
                Ok(services) => services,
                Err(_) => return Response::error("Service unavailable", 503),
            };
            save_settings(&mut req, &services, id).await
        }
        Route::Devices => list_devices(),
    }
}

// ---------------------------------------------------------------------------
// GET /api/settings/{id} (native: src/web/novel_settings.rs get_settings)
// ---------------------------------------------------------------------------

async fn get_settings(services: &AppServices, id: i64) -> worker::Result<Response> {
    let view = match services.novel_settings.load(id.into()).await {
        Ok(view) => view,
        Err(error) => return application_error_response(&error),
    };
    Response::from_json(&view.to_json_value(narou_rs::application::webui::capabilities::RuntimeKind::Worker))
}

// ---------------------------------------------------------------------------
// POST /api/settings/{id} (native: src/web/novel_settings.rs save_settings)
// ---------------------------------------------------------------------------

async fn save_settings(
    req: &mut Request,
    services: &AppServices,
    id: i64,
) -> worker::Result<Response> {
    // axum `Json` と同じく、本文が壊れていればここで 400。
    let body: serde_json::Value = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };
    if body.get("settings").and_then(serde_json::Value::as_array).is_some_and(|items| {
        items.iter().filter_map(|item| item.get("name").and_then(serde_json::Value::as_str))
            .any(narou_rs::application::webui::capabilities::worker_novel_setting_ineffective)
    }) {
        return json_error(400, "unsupported_setting", Some("Cloudflare Workers 版では調査ログを生成できません"));
    }
    let patch = match parse_settings_patch(&body) {
        Ok(patch) => patch,
        Err(message) => return json_error(400, "bad_request", Some(&message)),
    };

    match services.novel_settings.save(id.into(), &patch).await {
        Ok(result) => Response::from_json(&result.to_json_value()),
        Err(error) => application_error_response(&error),
    }
}

// ---------------------------------------------------------------------------
// GET /api/devices (native: src/web/novel_settings.rs list_devices)
// ---------------------------------------------------------------------------

/// native `OutputManager::available_devices` と同じ形 (`{devices: [{name, available}]}`)。
/// 値は共有の [`worker_devices`] が唯一の定義 — `POST /api/convert` の device
/// override 検証 (`worker_convert_device_supported`) と同じ一覧を使うので、
/// 端末一覧と受理範囲が乖離しない。
///
/// Worker が配信できるのは Lite の EPUB だけ (text は変換の内部形式で
/// ダウンロード経路が無く、mobi/kobo/ibunko/reader/ibooks は外部プロセスや
/// ローカル FS 前提の出力)。表示されていても実際には出力できない端末を
/// `available: true` にしてはいけない。
fn list_devices() -> worker::Result<Response> {
    let devices = worker_devices();
    let list: Vec<serde_json::Value> = devices
        .iter()
        .map(|choice| serde_json::json!({ "name": &choice.name, "available": choice.available }))
        .collect();
    Response::from_json(&serde_json::json!({ "devices": list }))
}

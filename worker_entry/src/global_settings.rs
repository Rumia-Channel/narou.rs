//! `GET/POST /api/global_setting`。
//!
//! 設定ページが読む JSON は native と共有する
//! [`narou_rs::application::settings_view`] が組み立てる（`D1SettingsStore` の上に
//! 載った `SettingsService` を渡すだけ）。native と違い、保存後の副作用
//! （自動更新スケジューラーの再起動や `webui.*` の再読み込み）は無い。Worker の
//! スケジューラーは cron 側で動くため。

use narou_rs::application::settings_view::{self, SAVE_MESSAGE};
use worker::{console_log, Env, Method, Request, Response};

use crate::composition::WorkerRuntime;

/// 設定一覧の取得と保存。
pub async fn api_global_setting(mut req: Request, env: Env) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    let method = req.method();
    if method != Method::Get && method != Method::Post {
        return Response::error("Method Not Allowed", 405);
    }
    let runtime = match WorkerRuntime::build(&env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("service composition failed: {error}");
            return Response::error("Service Unavailable", 503);
        }
    };
    if method == Method::Get {
        return Response::from_json(&settings_view::load_view(&runtime.services.settings).await);
    }
    let body: serde_json::Value = match req.json().await {
        Ok(body) => body,
        Err(_) => return Response::error("Bad Request", 400),
    };
    match settings_view::apply_save(&runtime.services.settings, &body).await {
        Ok(_effects) => Response::from_json(&result_body(true, SAVE_MESSAGE)),
        Err(message) => Response::from_json(&result_body(false, &message)),
    }
}

/// `ApiResponse` と同じ形（native の応答と揃える）。
fn result_body(success: bool, message: &str) -> serde_json::Value {
    serde_json::json!({
        "success": success,
        "message": message,
    })
}

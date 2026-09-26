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
    let runtime = match WorkerRuntime::build_ui(&env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("service composition failed: {error}");
            return Response::error("Service Unavailable", 503);
        }
    };
    if method == Method::Get {
        let mut view = settings_view::load_view(&runtime.services.settings).await;
        mark_worker_ineffective(&mut view);
        return Response::from_json(&view);
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

/// `GET /api/global_setting` の各項目に、Worker では経路が無い (= 変更しても
/// 効かない) 設定へ `worker_ineffective` と `worker_note` を付ける。
///
/// native では `webui_help_override` 相当の表示上書きで調整しているが、
/// Worker には対象の実行経路自体が無い項目が多い (ローカル FS / 外部
/// プロセス / サーバーバインド前提の設定)。native の応答にはこのキーが
/// 乗らないため、フロント側は「フラグが無ければ従来どおり表示」で良い。
fn mark_worker_ineffective(view: &mut serde_json::Value) {
    const NOTE: &str = "この設定は Cloudflare Workers 版では効きません (実行経路が native 専用)";
    /// 前方一致で無効になる prefix。`server-` は `server-max-targets-per-request`
    /// が Worker でも読まれるため `server_max_targets` で除外する。
    const PREFIXES: &[&str] = &[
        "server-",
        "mail.",
        "send.",
        "hotentry",
        "logging.",
        "difftool",
        "download.narou-api.",
        "default_args.",
    ];
    /// 個別名。変換のコピー先・端末依存のもの、サーバー bind、ローカル FS を
    /// 前提とする設定は Worker では読み出す経路自体が無い。
    const NAMES: &[&str] = &[
        "aozoraepub3dir",
        "concurrency",
        "convert.add-dc-subject-to-epub",
        "convert.copy-to",
        "convert.copy-to-grouping",
        "convert.copy-zip-to",
        "convert.dc-subject-exclude-tags",
        "convert.epub-font",
        "convert.filename-to-ncode",
        "convert.inspect",
        "convert.make-zip",
        "convert.multi-device",
        "convert.no-mobi",
        "convert.no-open",
        "convert.no-zip",
        "device",
        "ebook-filename-length-limit",
        "filename-length-limit",
        "folder",
        "folder-length-limit",
        "line-height",
        "narou-compat",
        "normalize-filename",
        "self-update.variant",
        "update.convert-only-new-arrival",
        "update.max-parallel-domains",
    ];
    let Some(items) = view.get_mut("settings").and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    for item in items {
        let Some(name) = item.get("name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let ineffective = PREFIXES.iter().any(|prefix| {
            name.starts_with(prefix)
                && !(name == "server-max-targets-per-request")
        }) || NAMES.contains(&name);
        if ineffective {
            item["worker_ineffective"] = serde_json::json!(true);
            item["worker_note"] = serde_json::json!(NOTE);
        }
    }
}

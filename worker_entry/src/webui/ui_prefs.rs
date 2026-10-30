//! Web UI の環境設定系エンドポイント。
//!
//! - `GET/POST /api/sort_state` — テーブルのソート状態 (native: `web::misc` +
//!   `web::sort_state`)。native は `server_setting.yaml` の `current_sort` キーへ
//!   書く。Worker では設定ストア (D1 `app_state`) の global スコープに同じキー名
//!   `current_sort` で保持する。
//! - `GET /api/webui/config` — Web UI 初期化設定 (native: `web::misc::webui_config`)。
//!   Worker には別ポートの WS サーバーが無いので `ws_port` / `port` には
//!   リクエストのポート (= `/ws` が同居するオリジン) を返す。
//! - `GET /api/feature_tour/pending` — 未読ツアー (native: `web::feature_tour::pending`)。
//!   ツアー定義テーブルと seen/disabled 設定キーは native と同じ。
//!   ストレージ移行プロンプトは `not(native-runtime)` 側の固定値を返す。

use narou_rs::application::webui::capabilities::{ConsoleHistoryCapabilities, WebUiCapabilities};
use narou_rs::application::webui::{CURRENT_SORT_KEY, normalize_current_sort_request};
use narou_rs::setting_core::SettingScope;
use narou_rs::application::webui::feature_tour::{
    DISABLED_KEY, SEEN_VERSION_KEY, compare_versions, current_entries, is_known_tour_version,
    latest_tour_version, pending_entries,
};
use worker::{Env, Method, Request, Response, console_log};

use crate::composition::WorkerRuntime;

use super::{api_response, json_error, load_current_sort_state};

/// 親がこの 1 ハンドラを 3 ルートへ割り当てる。パスとメソッドで振り分ける。
pub async fn handle(mut req: Request, env: Env) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    let path = req.path();
    // 対象パスとメソッドの組を先に検証する (native のルート定義と同じく、
    // パスはあるがメソッドが違う場合は 405)。
    enum Route {
        GetSortState,
        SaveSortState,
        WebuiConfig,
        FeatureTourPending,
        FeatureTourAll,
        FeatureTourSeen,
        FeatureTourConfig,
    }
    let route = match (req.method(), path.as_str()) {
        (Method::Get, "/api/sort_state") => Route::GetSortState,
        (Method::Post, "/api/sort_state") => Route::SaveSortState,
        (Method::Get, "/api/webui/config") => Route::WebuiConfig,
        (Method::Get, "/api/feature_tour/pending") => Route::FeatureTourPending,
        (Method::Get, "/api/feature_tour/all") => Route::FeatureTourAll,
        (Method::Post, "/api/feature_tour/seen") => Route::FeatureTourSeen,
        (Method::Post, "/api/feature_tour/config") => Route::FeatureTourConfig,
        (
            _,
            "/api/sort_state"
            | "/api/webui/config"
            | "/api/feature_tour/pending"
            | "/api/feature_tour/all"
            | "/api/feature_tour/seen"
            | "/api/feature_tour/config",
        ) => {
            return json_error(405, "method_not_allowed", None);
        }
        _ => {
            return json_error(404, "not_found", Some("route is not handled by this Worker"));
        }
    };
    let runtime = match WorkerRuntime::build_ui(&env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("service composition failed: {error}");
            return json_error(503, "service_unavailable", None);
        }
    };
    match route {
        Route::GetSortState => get_sort_state(&runtime).await,
        Route::SaveSortState => save_sort_state(&mut req, &runtime).await,
        Route::WebuiConfig => webui_config(&req, &env).await,
        Route::FeatureTourPending => feature_tour_pending(&runtime).await,
        Route::FeatureTourAll => feature_tour_all(&runtime).await,
        Route::FeatureTourSeen => feature_tour_seen(&mut req, &runtime).await,
        Route::FeatureTourConfig => feature_tour_config(&mut req, &runtime).await,
    }
}

// ---------------------------------------------------------------------------
// GET /api/webui/config (native: src/web/misc.rs:187 webui_config)
// ---------------------------------------------------------------------------

async fn webui_config(req: &Request, env: &Env) -> worker::Result<Response> {
    // `?fresh=1` は isolate キャッシュを迂回して primary から読む (別 isolate が
    // 古い値を持っていても、再読み込み時はここで取り直す)。通常の初期化は
    // キャッシュ経由で軽くする。
    let fresh = req
        .url()
        .ok()
        .and_then(|url| super::query_param(&url, "fresh"))
        .is_some_and(|value| matches!(value.as_str(), "1" | "true"));
    let settings = if fresh {
        super::metadata::settings_for_writes(env)
    } else {
        super::metadata::settings(env)
    };
    let Ok(settings) = settings else {
        return json_error(503, "service_unavailable", None);
    };
    // 依存の無い設定は 1 回の `get_many` にまとめる (スコープごとに 1 回だけ
    // ストアを読む)。
    const CONFIG_NAMES: &[&str] = &[
        "webui.theme",
        "webui.performance-mode",
        "webui.table.reload-timing",
        "webui.debug-mode",
        "tag.show-novel",
        "tag.show-manga",
        "tag.attributes-enabled",
        "concurrency",
    ];
    let loaded: Vec<Option<serde_yaml::Value>> = settings
        .get_many(CONFIG_NAMES)
        .await
        .unwrap_or_default();
    let find = |name: &str| {
        CONFIG_NAMES
            .iter()
            .position(|candidate| *candidate == name)
            .and_then(|index| loaded.get(index).cloned().flatten())
    };
    let string_value = |value: Option<serde_yaml::Value>, default: &str| {
        value
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| default.to_string())
    };
    let theme = string_value(find("webui.theme"), "Cerulean");
    let performance_mode = string_value(find("webui.performance-mode"), "auto");
    let reload_timing = string_value(find("webui.table.reload-timing"), "every");
    let debug_mode = find("webui.debug-mode")
        .and_then(|value| narou_rs::setting_core::setting_bool_value(&value))
        .unwrap_or(false);
    // Worker のジョブは常に並列なので既定は有効 (native の `concurrency=true` と
    // 同じ 2 ペイン表示)。明示的に false を保存したときだけ 1 ペインに戻す。
    let concurrency_enabled = find("concurrency")
        .and_then(|value| narou_rs::setting_core::setting_bool_value(&value))
        .unwrap_or(true);

    // native は HTTP/WS サーバーの待受ポートを返す。Worker では `/ws` も同じ
    // オリジンに同居するので、リクエストの実効ポート (明示ポートまたは
    // スキーム既定値) をそのまま返す。
    let port = req
        .url()
        .ok()
        .and_then(|url| url.port_or_known_default());

    // UI が対応済みの操作だけを出すための能力宣言。Worker は 501 を返す経路が
    // あるので、native と違って全操作が false。コンソール履歴は PushHub DO の
    // 実装値と共有する。
    let capabilities = WebUiCapabilities::worker(ConsoleHistoryCapabilities {
        available: true,
        capacity: crate::push_hub::HISTORY_CAPACITY,
        clear: true,
        max_bytes: crate::push_hub::HISTORY_MAX_BYTES,
    });

    Response::from_json(&serde_json::json!({
        "theme": theme,
        "performance_mode": performance_mode,
        "reload_timing": reload_timing,
        "debug_mode": debug_mode,
        "show_novel_attribute": find("tag.show-novel").and_then(|value| narou_rs::setting_core::setting_bool_value(&value)).unwrap_or(false),
        "show_manga_attribute": find("tag.show-manga").and_then(|value| narou_rs::setting_core::setting_bool_value(&value)).unwrap_or(true),
        // 属性タグ機能全体の ON/OFF。OFF の間は個別表示スイッチも効かない。
        "attribute_tags_enabled": find("tag.attributes-enabled").and_then(|value| narou_rs::setting_core::setting_bool_value(&value)).unwrap_or(false),
        "ws_port": port,
        "port": port,
        "concurrency_enabled": concurrency_enabled,
        // Worker 固有の注記 (native の応答にも同じキーを native 値で入れる):
        // `concurrency` は実行レーンではなくコンソール分離のみ、`convert.keep-txt`
        // の既定は false、`queue.max-retries` は配備上限で clamp される。
        "concurrency_scope": "console",
        "convert_keep_txt_default": crate::convert::KEEP_CONVERTED_TEXT_DEFAULT,
        "queue_max_retries_limit": crate::consumer::CF_CONSUMER_MAX_RETRIES,
        "capabilities": serde_json::to_value(&capabilities)
            .expect("capabilities serialization is infallible"),
    }))
}

// ---------------------------------------------------------------------------
// GET/POST /api/sort_state (native: src/web/misc.rs:452 / :466,
//                             保存形式: src/web/sort_state.rs)
// ---------------------------------------------------------------------------

async fn get_sort_state(runtime: &WorkerRuntime) -> worker::Result<Response> {
    // native は `server_setting` (global) の `current_sort` キーを読む。
    // Worker では global スコープの `current_sort` 行がその値そのもの
    // (共有の `super::load_current_sort_state` がそのまま読む)。
    let sort_state = load_current_sort_state(runtime).await;
    Response::from_json(&sort_state.to_json_value())
}

async fn save_sort_state(
    req: &mut Request,
    runtime: &WorkerRuntime,
) -> worker::Result<Response> {
    let body: serde_json::Value = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };
    let Some(sort_state) = normalize_current_sort_request(&body) else {
        return Response::from_json(&api_response(
            false,
            "valid column and dir are required",
        ));
    };
    match runtime
        .write_services()
        .settings
        .set_raw(
            SettingScope::Global,
            CURRENT_SORT_KEY,
            sort_state.to_yaml_value(),
        )
        .await
    {
        Ok(()) => Response::from_json(&api_response(true, "OK")),
        Err(error) => Response::from_json(&api_response(false, error.to_string())),
    }
}

// ---------------------------------------------------------------------------
// GET /api/feature_tour/pending (native: src/web/feature_tour.rs:144 pending)
// ---------------------------------------------------------------------------

/// native `narou_rs::version::VERSION` は native-runtime 限定。Worker の配布
/// バージョンを使う (`narou_worker` のパッケージバージョンは narou.rs と揃えてある)。
const WORKER_VERSION: &str = env!("CARGO_PKG_VERSION");



async fn feature_tour_pending(runtime: &WorkerRuntime) -> worker::Result<Response> {
    let settings = &runtime.services.settings;
    let seen_version = settings
        .get_raw(SettingScope::Local, SEEN_VERSION_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|value| value.as_str().map(str::to_owned));
    let disabled = settings
        .get_raw(SettingScope::Local, DISABLED_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|value| narou_rs::setting_core::setting_bool_value(&value))
        .unwrap_or(false);
    let entries = pending_entries(seen_version.as_deref(), WORKER_VERSION);
    let latest_pending_version = entries
        .iter()
        .map(|entry| entry.version)
        .max_by(|a, b| compare_versions(a, b))
        .unwrap_or("");

    // native `not(native-runtime)` 側と同じ固定値。Worker の管理データは常に
    // D1 なので移行プロンプトは出さない。
    let storage_migration = serde_json::json!({ "available": false, "mode": "yaml" });
    Response::from_json(&serde_json::json!({
        "success": true,
        "current_version": WORKER_VERSION,
        "seen_version": seen_version,
        "disabled": disabled,
        "latest_pending_version": latest_pending_version,
        "entries": if disabled { Vec::new() } else { entries },
        "storage_migration": storage_migration,
    }))
}

/// `GET /api/feature_tour/all` — 既知のツアー全件（native: `feature_tour::all`）。
async fn feature_tour_all(runtime: &WorkerRuntime) -> worker::Result<Response> {
    let settings = &runtime.services.settings;
    let seen_version = load_local_str(settings, SEEN_VERSION_KEY).await;
    let disabled = load_local_bool(settings, DISABLED_KEY).await;
    Response::from_json(&serde_json::json!({
        "success": true,
        "current_version": WORKER_VERSION,
        "seen_version": seen_version,
        "disabled": disabled,
        "latest_pending_version": latest_tour_version(),
        "entries": current_entries(WORKER_VERSION),
    }))
}

/// `POST /api/feature_tour/seen` — 既読バージョンを記録する。
async fn feature_tour_seen(
    req: &mut Request,
    runtime: &WorkerRuntime,
) -> worker::Result<Response> {
    let body: serde_json::Value = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };
    let requested = body["version"].as_str().unwrap_or("").trim().to_string();
    if requested.is_empty() {
        return Response::from_json(&api_response(false, "version is required"));
    }
    if !is_known_tour_version(&requested) {
        return Response::from_json(&api_response(false, "unknown tour version"));
    }
    let services = runtime.write_services();
    match narou_rs::application::webui::feature_tour::mark_seen(&services.settings, &requested).await {
        Ok(()) => Response::from_json(&api_response(true, "OK")),
        Err(error) => Response::from_json(&api_response(false, error.to_string())),
    }
}

/// `POST /api/feature_tour/config` — ツアー表示の on/off。
async fn feature_tour_config(
    req: &mut Request,
    runtime: &WorkerRuntime,
) -> worker::Result<Response> {
    let body: serde_json::Value = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };
    let disabled = body["disabled"].as_bool().unwrap_or(false);
    match runtime
        .write_services()
        .settings
        .set_raw(
            SettingScope::Local,
            DISABLED_KEY,
            serde_yaml::Value::Bool(disabled),
        )
        .await
    {
        Ok(()) => Response::from_json(&api_response(true, "OK")),
        Err(error) => Response::from_json(&api_response(false, error.to_string())),
    }
}

/// local スコープの生設定（文字列）。
async fn load_local_str(
    settings: &narou_rs::application::SettingsService,
    key: &str,
) -> Option<String> {
    settings
        .get_raw(SettingScope::Local, key)
        .await
        .ok()
        .flatten()
        .and_then(|value| value.as_str().map(str::to_owned))
}

/// local スコープの生設定（真偽値）。
async fn load_local_bool(
    settings: &narou_rs::application::SettingsService,
    key: &str,
) -> bool {
    settings
        .get_raw(SettingScope::Local, key)
        .await
        .ok()
        .flatten()
        .and_then(|value| narou_rs::setting_core::setting_bool_value(&value))
        .unwrap_or(false)
}


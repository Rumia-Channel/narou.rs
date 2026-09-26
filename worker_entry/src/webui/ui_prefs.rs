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

use narou_rs::application::webui::{CURRENT_SORT_KEY, normalize_current_sort_request};
use narou_rs::setting_core::SettingScope;
use serde::Serialize;
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
    let runtime = match WorkerRuntime::build(&env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("service composition failed: {error}");
            return json_error(503, "service_unavailable", None);
        }
    };
    match route {
        Route::GetSortState => get_sort_state(&runtime).await,
        Route::SaveSortState => save_sort_state(&mut req, &runtime).await,
        Route::WebuiConfig => webui_config(&req, &runtime).await,
        Route::FeatureTourPending => feature_tour_pending(&runtime).await,
        Route::FeatureTourAll => feature_tour_all(&runtime).await,
        Route::FeatureTourSeen => feature_tour_seen(&mut req, &runtime).await,
        Route::FeatureTourConfig => feature_tour_config(&mut req, &runtime).await,
    }
}

// ---------------------------------------------------------------------------
// GET /api/webui/config (native: src/web/misc.rs:187 webui_config)
// ---------------------------------------------------------------------------

async fn webui_config(req: &Request, runtime: &WorkerRuntime) -> worker::Result<Response> {
    let settings = &runtime.services.settings;
    let string_value = |value: Option<serde_yaml::Value>, default: &str| {
        value
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| default.to_string())
    };
    let theme = string_value(settings.get("webui.theme").await.ok().flatten(), "Cerulean");
    let performance_mode = string_value(
        settings.get("webui.performance-mode").await.ok().flatten(),
        "auto",
    );
    let reload_timing = string_value(
        settings.get("webui.table.reload-timing").await.ok().flatten(),
        "every",
    );
    let debug_mode = settings
        .get("webui.debug-mode")
        .await
        .ok()
        .flatten()
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let concurrency_enabled = settings
        .get("concurrency")
        .await
        .ok()
        .flatten()
        .and_then(|value| value.as_bool())
        .unwrap_or(false);

    // native は HTTP/WS サーバーの待受ポートを返す。Worker では `/ws` も同じ
    // オリジンに同居するので、リクエストの実効ポート (明示ポートまたは
    // スキーム既定値) をそのまま返す。
    let port = req
        .url()
        .ok()
        .and_then(|url| url.port_or_known_default());

    Response::from_json(&serde_json::json!({
        "theme": theme,
        "performance_mode": performance_mode,
        "reload_timing": reload_timing,
        "debug_mode": debug_mode,
        "ws_port": port,
        "port": port,
        "concurrency_enabled": concurrency_enabled,
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
        .services
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

const SEEN_VERSION_KEY: &str = "webui.feature-tour.seen-version";
const DISABLED_KEY: &str = "webui.feature-tour.disabled";

/// native `narou_rs::version::VERSION` は native-runtime 限定。Worker の配布
/// バージョンを使う (`narou_worker` のパッケージバージョンは narou.rs と揃えてある)。
const WORKER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy, Debug, Serialize)]
pub struct FeatureTourEntry {
    version: &'static str,
    title: &'static str,
    body: &'static str,
    items: &'static [&'static str],
}

/// native `FEATURE_TOURS` と同じテーブル。native 側に項目を足したらこちらも
/// 揃えること (JSON の形はフィールド名に依存する)。
const FEATURE_TOURS: &[FeatureTourEntry] = &[
    FeatureTourEntry {
        version: "0.2.0",
        title: "narou.rb から広がった Web UI",
        body: "Narou.rs の Web UI では、narou.rb 互換の管理データを使いながら、まとめて扱う操作が増えています。",
        items: &[
            "複数作品を選択した update / convert / tag / freeze / remove",
            "キュー管理と進捗表示",
            "タグ指定 update と最新話掲載日の確認",
        ],
    },
    FeatureTourEntry {
        version: "0.2.0",
        title: "シリーズ URL の一括登録",
        body: "作品 URL だけでなく、シリーズやコレクションの URL から個別作品を展開して登録できます。",
        items: &[
            "小説家になろうのシリーズ URL",
            "ノクターンなど R18 系のシリーズ URL",
            "カクヨムのコレクション URL",
        ],
    },
    FeatureTourEntry {
        version: "0.2.2",
        title: "更新後の自動変換とコピー設定",
        body: "更新後の convert と copy-to 周りの挙動を見直し、Web UI と CLI の update からの変換結果が揃うようにしています。",
        items: &[
            "convert.multi-device を update 後の自動変換でも反映",
            "EPUB 変換時の copy-to 出力を優先",
            "text-only 変換時に txt を EPUB 保存先へコピーしない",
        ],
    },
    FeatureTourEntry {
        version: "0.2.3",
        title: "新機能ツアー",
        body: "バージョンごとの追加機能を、必要な分だけ起動時に表示するようになりました。",
        items: &[
            "表示済みのツアー version を local_setting.yaml に保存",
            "次の更新では、未表示のツアーだけを表示",
            "ツアー項目が追加されないバージョンでは何も表示しない",
        ],
    },
    FeatureTourEntry {
        version: "0.3.0",
        title: "ハーメルン R18 作品に対応",
        body: "h.syosetu.org に分離されたハーメルンの R18 作品を、h あり・なしどちらの URL からでも登録・ダウンロードできます。",
        items: &[
            "h.syosetu.org と syosetu.org を同一サイトとして扱い、重複登録を防止",
            "R18 作品の取得時は h ドメインへの転送を自動で追跡",
            "R18 作品は over18(18歳以上確認)設定が有効な場合のみ取得",
        ],
    },
    FeatureTourEntry {
        version: "0.3.0",
        title: "新規タグの既定色設定",
        body: "新しく作られるタグに割り当てる色を webui.new-tag-color 設定で固定できます。",
        items: &[
            "設定画面の WebUI タブから色を選択",
            "default のままなら従来どおりタグの追加順に色を巡回",
            "Web UI と CLI のどちらでタグを追加しても同じ色を適用",
        ],
    },
    FeatureTourEntry {
        version: "0.3.0",
        title: "失敗したジョブの自動リトライ",
        body: "キューのジョブが一時的なエラーで失敗した場合、間隔を広げながら自動で再試行します。",
        items: &[
            "再試行回数は queue.max-retries 設定で調整(既定 3 回)",
            "再試行間隔は queue.retry-backoff 設定で調整(既定 1m,5m,15m)",
            "再試行の予定は queue_retry 通知として Web UI に配信",
        ],
    },
    FeatureTourEntry {
        version: "0.4.0",
        title: "管理データの SQLite 移行とライブラリバックアップ",
        body: "0.4.0 では .narou 配下の管理データを SQLite へ移行できます。破壊的な変更に備え、アップデート後の初回起動時にライブラリ全体のバックアップを提案します。",
        items: &[
            "初回起動時にライブラリ全体 (小説データ + .narou + webnovel 等) のバックアップを提案",
            "narou_rs_backup サブ実行ファイルでいつでもライブラリ全体を zip 化可能",
            "管理方式は Web UI ツアーまたは .narou/storage-backend で SQLite / YAML を選択",
            "narou db verify / export-yaml / vacuum で SQLite 管理データを保守",
            "narou diff --history / --restore で小説本文のバージョン履歴を参照・復元",
        ],
    },
    FeatureTourEntry {
        version: "0.4.3",
        title: "Pixiv の小説・イラスト・漫画に対応",
        body: "Pixiv の URL から作品を登録し、本文やページ画像を取得できるようになりました。",
        items: &[
            "小説・小説シリーズ・イラスト/漫画・漫画シリーズの URL に対応",
            "挿絵や漫画のページ画像を保存し、うごイラはネイティブ版で APNG に変換",
            "R18・ログイン限定作品を取得するには、先にログイン Cookie を登録",
        ],
    },
    FeatureTourEntry {
        version: "0.4.3",
        title: "ログインが必要な作品を取得",
        body: "ログインを求められた作品は、保存済みの Cookie を使って再取得できます。",
        items: &[
            "同梱の narou_rs_login でブラウザの Cookie を取得し、書き出しファイルで別端末へ持ち込めます",
            "環境設定の「ログイン」タブや narou login で管理し、複数のアカウントを試す順序も変更できます",
            "Cookie は暗号化して保存し、ログイン不要な作品には通常送信しません",
        ],
    },
    FeatureTourEntry {
        version: "0.4.3",
        title: "ライブラリの検索が便利に",
        body: "Web UI 右上のフィルター欄で、作品の URL や ID を貼り付けて探せます。",
        items: &[
            "作品 URL、なろうの N コード、数値の作品 ID・登録 ID で検索",
            "従来のタイトル・タグ・作者などの検索も引き続き利用可能",
        ],
    },
    FeatureTourEntry {
        version: "0.4.3",
        title: "更新時の GPL 版・通常版を選択",
        body: "環境設定の Global タブで、今後のセルフアップデートで取得する版を選べます。",
        items: &[
            "GPL 版は AozoraEpub3_Lite を組み込み、通常版は外部 AozoraEpub3 を利用",
            "self-update.variant で GPL 版 / 通常版 / 未設定を選択。CLI の narou setting でも変更可能",
            "未設定なら、現在利用中のビルドと同じ種類の版を取得",
        ],
    },
];

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
        .and_then(|value| value.as_bool())
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
    let settings = &runtime.services.settings;
    let version_to_save = load_local_str(settings, SEEN_VERSION_KEY)
        .await
        .filter(|seen| version_greater(seen, &requested))
        .unwrap_or(requested);
    match settings
        .set_raw(
            SettingScope::Local,
            SEEN_VERSION_KEY,
            serde_yaml::Value::String(version_to_save),
        )
        .await
    {
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
        .services
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
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// 現在のバージョンまでに存在するツアー（native: `current_entries`）。
fn current_entries(current_version: &str) -> Vec<FeatureTourEntry> {
    FEATURE_TOURS
        .iter()
        .copied()
        .filter(|entry| !version_greater(entry.version, current_version))
        .collect()
}

/// 既知のツアーのうち最新のバージョン（native: `latest_tour_version`）。
fn latest_tour_version() -> &'static str {
    FEATURE_TOURS
        .iter()
        .map(|entry| entry.version)
        .max_by(|a, b| compare_versions(a, b))
        .unwrap_or("")
}

/// テーブルに存在するツアーのバージョンか（native: `is_known_tour_version`）。
fn is_known_tour_version(version: &str) -> bool {
    FEATURE_TOURS.iter().any(|entry| entry.version == version)
}

fn pending_entries(seen_version: Option<&str>, current_version: &str) -> Vec<FeatureTourEntry> {
    FEATURE_TOURS
        .iter()
        .copied()
        .filter(|entry| {
            !version_greater(entry.version, current_version)
                && seen_version
                    .map(|seen| version_greater(entry.version, seen))
                    .unwrap_or(true)
        })
        .collect()
}

fn version_greater(left: &str, right: &str) -> bool {
    compare_versions(left, right).is_gt()
}

fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    normalize_version_parts(left).cmp(&normalize_version_parts(right))
}

fn normalize_version_parts(version: &str) -> [u64; 3] {
    let mut parts = [0, 0, 0];
    let normalized = version
        .trim()
        .trim_start_matches('v')
        .split_whitespace()
        .next()
        .unwrap_or("")
        .split(['-', '+'])
        .next()
        .unwrap_or("");
    for (index, part) in normalized.split('.').take(3).enumerate() {
        parts[index] = part.parse::<u64>().unwrap_or(0);
    }
    parts
}

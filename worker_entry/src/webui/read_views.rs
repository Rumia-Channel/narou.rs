//! 読み取り系 + 差分/メモ帳エンドポイント (native `src/web/novels.rs`,
//! `src/web/misc.rs`, `src/web/jobs.rs` からの移植)。
//!
//! - `GET /api/story` — `{"title", "story"}`。TOC オブジェクト
//!   (`services.content.toc`) の `story` を trim して返す。
//! - `GET /api/diff_list?target=…` — 差分キャッシュの HTML 断片 (native
//!   `render_diff_list_html_for_target`)。Worker の差分キャッシュは
//!   オブジェクトストア上の `本文/.cache/` (移行物の `本文/cache/` も対象)。
//! - `POST /api/diff_list` — `{"diffs": [{"id","title","content"}]}`。
//!   `content` は `diff.txt` オブジェクト (= `services.content.diff`)。
//! - `POST /api/diff_clean` — `narou diff --clean` 相当。差分キャッシュ用の
//!   オブジェクトを削除する。Worker には native の `novel_versions` 履歴が
//!   無いのでキャッシュ削除がここでの全量になる。
//! - `GET /api/notepad/read` / `POST /api/notepad/save` —
//!   `app_state('inv','notepad')` の生テキスト (native `StateDb::set_raw` と同じく
//!   `value_yaml` に格納)。`object_id` は本文の SHA-256 hex で競合検知に使う。
//! - `GET /api/history` / `POST /api/clear_history` — Worker には PushServer の
//!   コンソール履歴バッファが無い (websocket.rs 参照)。保持する履歴が無いので
//!   `history` は空を返し、`clear_history` はクリア対象が無い = native の
//!   クリア済み状態と同じ成功応答になる。
//! - `GET`/`POST /api/taginfo.json` — タグ情報配列。native は POST のみだが、
//!   ルート表で GET に割り当てられているため GET (クエリ `ids` 指定) も受理する。
//! - `GET /api/version/current.json` / `GET /api/version/latest.json` —
//!   native `version_json` のキー構造を Worker の実値に置き換えたもの。
//!   バージョン文字列は `env!("CARGO_PKG_VERSION")` (`narou_worker` の
//!   パッケージバージョンは narou.rs と揃えてある)。
//! - `POST /api/inspect` — `調査ログ.txt` は native の `Inspector::save`
//!   (ローカル FS への書き込み) でしか生成されず、出力の送達にも PushServer の
//!   console broadcast が要る。どちらも Worker には無いので 501
//!   `not_supported_on_worker` を返す (成功を偽装しない)。

use std::collections::HashMap;
use std::num::NonZeroUsize;

use narou_rs::application::ApplicationError;
use narou_rs::application::aliases::resolve_alias_target;
use narou_rs::downloader::{Downloader, SectionFile, TargetType};
use narou_rs::platform::{
    NovelId, NovelObjectKeys, ObjectKey, ObjectListRequest, ObjectPrefix,
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use wasm_bindgen::JsValue;
use worker::{console_log, Env, Fetch, Headers, Method, Request, RequestInit, Response};

use crate::composition::WorkerRuntime;

/// native `crate::web::MAX_WEB_TARGETS_PER_REQUEST` —
/// `server-max-targets-per-request` 設定が無い/不正なときのフォールバック上限。
const MAX_WEB_TARGETS_PER_REQUEST: usize = 100_000;
/// native `crate::web::MAX_WEB_TARGET_LENGTH` (`validate_web_target_value`)。
const MAX_WEB_TARGET_LENGTH: usize = 4096;
/// native `crate::application::settings_view::MAX_WEB_TEXT_INPUT_BYTES`。
const MAX_WEB_TEXT_INPUT_BYTES: usize = 1024 * 1024;
/// native `SECTION_SAVE_DIR` (`本文`) — セクション/キャッシュが置かれる層。
const SECTION_SAVE_DIR: &str = "本文";
/// Worker が書く差分キャッシュ (`NovelObjectKeys::cached_section`) と
/// native レイアウト由来の移行物 (`cache/`) の双方を対象にする。
const CACHE_DIR_NAMES: [&str; 2] = [".cache", "cache"];
/// `app_state` の notepad 行 (native `StateDb` の scope/key と同じ)。
/// alias 行の scope/key は `narou_rs::application::aliases` が持つ。
const INVENTORY_SCOPE: &str = "inv";
const NOTEPAD_KEY: &str = "notepad";
/// native `src/web/misc.rs::version_latest` と同じ参照先。
const RELEASES_API_URL: &str =
    "https://api.github.com/repos/Rumia-Channel/narou.rs/releases/latest";
const RELEASES_PAGE_URL: &str =
    "https://github.com/Rumia-Channel/narou.rs/releases/latest";
/// native `narou_rs::version::NAME`。
const APP_NAME: &str = "narou.rs";
/// 差分キャッシュのオブジェクト列挙/削除を一度に扱うページ幅。
const OBJECT_PAGE_LIMIT: usize = 1000;

/// native `src/web/state.rs::TargetsBody` と同じ受理形。
#[derive(Debug, Deserialize)]
struct TargetsBody {
    targets: Vec<serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    sort_state: Option<serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    timestamp: Option<u64>,
}

/// native `DiffCleanBody` と同じ受理形。
#[derive(Debug, Deserialize)]
struct DiffCleanBody {
    target: serde_json::Value,
}

/// native `TagInfoBody` と同じ受理形。
#[derive(Debug, Deserialize)]
struct TagInfoBody {
    ids: Vec<serde_json::Value>,
    #[serde(default)]
    with_exclusion: Option<bool>,
    #[serde(default)]
    #[allow(dead_code)]
    sort_state: Option<serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    timestamp: Option<u64>,
}

/// Entry point; `lib.rs` routes the assigned paths here.
/// パス×メソッドが native のルート定義と一致しない組は 405、未知パスは 404。
pub async fn handle(mut req: Request, env: Env) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    enum Route {
        Story,
        DiffListGet,
        DiffListPost,
        DiffClean,
        Inspect,
        NotepadRead,
        NotepadSave,
        History,
        ClearHistory,
        TagInfoGet,
        TagInfoPost,
        VersionCurrent,
        VersionLatest,
    }
    let path = req.path();
    let route = match (req.method(), path.as_str()) {
        // axum の `get(...)` は HEAD も通す。GET 系は GET/HEAD 双方を受理する。
        (Method::Get | Method::Head, "/api/story") => Route::Story,
        (Method::Get | Method::Head, "/api/diff_list") => Route::DiffListGet,
        (Method::Post, "/api/diff_list") => Route::DiffListPost,
        (Method::Post, "/api/diff_clean") => Route::DiffClean,
        (Method::Post, "/api/inspect") => Route::Inspect,
        (Method::Get | Method::Head, "/api/notepad/read") => Route::NotepadRead,
        (Method::Post, "/api/notepad/save") => Route::NotepadSave,
        (Method::Get | Method::Head, "/api/history") => Route::History,
        (Method::Post, "/api/clear_history") => Route::ClearHistory,
        (Method::Get | Method::Head, "/api/taginfo.json") => Route::TagInfoGet,
        (Method::Post, "/api/taginfo.json") => Route::TagInfoPost,
        (Method::Get | Method::Head, "/api/version/current.json") => Route::VersionCurrent,
        (Method::Get | Method::Head, "/api/version/latest.json") => Route::VersionLatest,
        (
            _,
            "/api/story"
            | "/api/diff_list"
            | "/api/diff_clean"
            | "/api/inspect"
            | "/api/notepad/read"
            | "/api/notepad/save"
            | "/api/history"
            | "/api/clear_history"
            | "/api/taginfo.json"
            | "/api/version/current.json"
            | "/api/version/latest.json",
        ) => return json_error(405, "method_not_allowed", None),
        _ => {
            return json_error(404, "not_found", Some("route is not handled by this Worker"));
        }
    };

    // バージョン系はリポジトリに触れないので runtime 組み立てをスキップする。
    match route {
        Route::VersionCurrent => return version_current(),
        Route::VersionLatest => return version_latest().await,
        _ => {}
    }

    let runtime = match WorkerRuntime::build(&env).await {
        Ok(runtime) => runtime,
        Err(error) => {
            console_log!("service composition failed: {error}");
            return json_error(503, "service_unavailable", None);
        }
    };

    match route {
        Route::Story => get_story(&req, &runtime).await,
        Route::DiffListGet => diff_list_get(&req, &runtime).await,
        Route::DiffListPost => diff_list_post(&mut req, &runtime).await,
        Route::DiffClean => diff_clean(&mut req, &env, &runtime).await,
        Route::Inspect => inspect(&mut req, &runtime).await,
        Route::NotepadRead => notepad_read(&env).await,
        Route::NotepadSave => notepad_save(&mut req, &env).await,
        Route::History => console_history(&req),
        Route::ClearHistory => clear_history(),
        Route::TagInfoGet => taginfo_get(&req, &runtime).await,
        Route::TagInfoPost => taginfo_post(&mut req, &runtime).await,
        Route::VersionCurrent | Route::VersionLatest => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// `lib.rs::json_error` と同じ JSON 形 (`{error: {code, message?}}`)。
/// あちらは private なので形だけ合わせてここに持つ。
fn json_error(status: u16, code: &str, message: Option<&str>) -> worker::Result<Response> {
    let payload = match message {
        Some(message) => json!({ "error": { "code": code, "message": message } }),
        None => json!({ "error": { "code": code } }),
    };
    Response::from_json(&payload).map(|response| response.with_status(status))
}

/// native `map_application_error` (`src/web/novels.rs`) と同じステータス割当。
fn map_application_error(error: ApplicationError) -> worker::Result<Response> {
    let (status, code, message) = match error {
        ApplicationError::InvalidRequest(message) => (400, "bad_request", message),
        ApplicationError::NotFound(message) => (404, "not_found", message),
        ApplicationError::Platform(message) => (500, "internal_error", message),
    };
    json_error(status, code, Some(&message))
}

/// native `ApiResponse` と同じ形 (`{success, message}`)。
fn api_response(success: bool, message: impl Into<String>) -> serde_json::Value {
    json!({
        "success": success,
        "message": message.into(),
    })
}

/// native `ApiResponse` を JSON ボディとして返す (ステータスは native 同様 200)。
fn api_json_response(success: bool, message: impl Into<String>) -> worker::Result<Response> {
    Response::from_json(&api_response(success, message))
}

fn query_param(url: &worker::Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

/// native `src/web/misc.rs::html_escape` / `tag_color_class` の写し。
fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn tag_color_class(color: &str) -> &'static str {
    match color {
        "green" => "tag-green",
        "yellow" => "tag-yellow",
        "blue" => "tag-blue",
        "magenta" => "tag-magenta",
        "cyan" => "tag-cyan",
        "red" => "tag-red",
        "white" => "tag-white",
        _ => "tag-default",
    }
}

/// `webui/queue.rs` と同じく `webui.new-tag-color` 設定を読み、
/// 小文字化 + 有効色チェックを通す (native `configured_tag_color`)。
async fn configured_tag_color(runtime: &WorkerRuntime) -> Option<String> {
    runtime
        .services
        .settings
        .get(narou_rs::application::tag_colors::NEW_TAG_COLOR_SETTING)
        .await
        .ok()
        .flatten()
        .and_then(|value| value.as_str().map(str::to_owned))
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| narou_rs::application::tag_colors::is_valid_tag_color(value))
}

/// native `validate_web_target_value` (`src/web/mod.rs`) の写し。
fn validate_web_target_value(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("target is required".to_string());
    }
    if trimmed.len() > MAX_WEB_TARGET_LENGTH {
        return Err("target is too long".to_string());
    }
    if trimmed.starts_with('-') {
        return Err("invalid target".to_string());
    }
    if trimmed.chars().any(|ch| ch.is_control()) {
        return Err("target contains invalid characters".to_string());
    }
    Ok(trimmed.to_string())
}

/// native `targets_to_strings` (`src/web/jobs.rs`) の写し。
fn targets_to_strings(targets: &[serde_json::Value]) -> Vec<String> {
    targets
        .iter()
        .map(|v| match v {
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect()
}

/// native `max_web_targets_per_request` (`src/web/mod.rs`) 相当。
async fn max_web_targets(runtime: &WorkerRuntime) -> usize {
    runtime
        .services
        .settings
        .web_target_limit(MAX_WEB_TARGETS_PER_REQUEST)
        .await
}

// ---------------------------------------------------------------------------
// GET /api/story (native: src/web/novels.rs:201 get_story)
// ---------------------------------------------------------------------------

async fn get_story(req: &Request, runtime: &WorkerRuntime) -> worker::Result<Response> {
    let url = req.url()?;
    let id_str = match query_param(&url, "id") {
        Some(id) => id,
        None => return json_error(400, "bad_request", Some("id is required")),
    };
    let id: i64 = match id_str.parse() {
        Ok(id) => id,
        Err(_) => return json_error(400, "bad_request", Some("invalid id")),
    };

    let record = match runtime.services.library.get(NovelId(id)).await {
        Ok(record) => record,
        Err(error) => return map_application_error(error),
    };
    let Some(record) = record else {
        return json_error(404, "not_found", Some(&format!("ID: {}", id)));
    };

    let toc = match runtime.services.content.toc(NovelId(id)).await {
        Ok(bytes) => bytes
            .and_then(|bytes| serde_yaml::from_slice::<narou_rs::downloader::TocObject>(&bytes).ok()),
        Err(error) => return map_application_error(error),
    };
    let (title, story) = match toc {
        Some(t) => {
            let story = t.story.unwrap_or_default().trim().to_string();
            (t.title, story)
        }
        None => (record.title, String::new()),
    };

    Response::from_json(&json!({ "title": title, "story": story }))
}

// ---------------------------------------------------------------------------
// diff targets / object store helpers
// ---------------------------------------------------------------------------

/// native `resolve_existing_id_for_target_with_library` (`src/web/jobs.rs`) 相当:
/// `get_target_type` で id / URL / ncode / タイトルの順に既存 ID を引く。
/// URL 解決は `services.site_definitions.resolve_toc_url` を優先し、
/// Worker の `EmptySiteDefinitionProvider` ではヒットしないときだけ
/// 保存済み `toc_url` 完全一致へフォールバックする。
async fn resolve_existing_id(runtime: &WorkerRuntime, target: &str) -> Option<i64> {
    match Downloader::get_target_type(target) {
        TargetType::Id => runtime
            .services
            .library
            .get(target.parse::<i64>().ok()?.into())
            .await
            .ok()
            .flatten()
            .map(|record| record.id),
        TargetType::Url => {
            let toc_url = runtime
                .services
                .site_definitions
                .resolve_toc_url(target)
                .unwrap_or_else(|| target.to_string());
            runtime
                .services
                .library
                .find_by_toc_url(&toc_url)
                .await
                .ok()
                .flatten()
                .map(|record| record.id)
        }
        TargetType::Ncode => runtime
            .services
            .library
            .find_by_ncode(target)
            .await
            .ok()
            .flatten()
            .map(|record| record.id),
        _ => runtime
            .services
            .library
            .find_by_title(target)
            .await
            .ok()
            .flatten()
            .map(|record| record.id),
    }
}

/// `narou diff --clean <target>` がサブプロセス内で通る解決順
/// (`commands::resolve_target_to_id`): エイリアス → ID → URL → ncode → タイトル
/// (Other はタイトル→ncode の順でフォールバック)。
async fn resolve_diff_target_id(
    runtime: &WorkerRuntime,
    env: &Env,
    target: &str,
) -> Option<i64> {
    let aliases = super::download::load_aliases(env).await;
    let effective = resolve_alias_target(&aliases, target);
    let effective = effective.trim();
    if effective.is_empty() {
        return None;
    }
    if let Ok(id) = effective.parse::<i64>() {
        return runtime
            .services
            .library
            .get(id.into())
            .await
            .ok()
            .flatten()
            .map(|record| record.id);
    }
    match Downloader::get_target_type(effective) {
        TargetType::Url => {
            let toc_url = runtime
                .services
                .site_definitions
                .resolve_toc_url(effective)
                .unwrap_or_else(|| effective.to_string());
            runtime
                .services
                .library
                .find_by_toc_url(&toc_url)
                .await
                .ok()
                .flatten()
                .map(|record| record.id)
        }
        TargetType::Ncode => runtime
            .services
            .library
            .find_by_ncode(effective)
            .await
            .ok()
            .flatten()
            .map(|record| record.id),
        TargetType::Id => None,
        _ => {
            let by_title = runtime
                .services
                .library
                .find_by_title(effective)
                .await
                .ok()
                .flatten()
                .map(|record| record.id);
            match by_title {
                Some(id) => Some(id),
                None => runtime
                    .services
                    .library
                    .find_by_ncode(effective)
                    .await
                    .ok()
                    .flatten()
                    .map(|record| record.id),
            }
        }
    }
}

/// `<novel_prefix>/本文/<cache_dir>/<version>/<file>.yaml` という形の
/// キャッシュキーだけを `(version, file_name)` に展開する。
/// `novel_prefix` は `NovelObjectKeys::prefix()` (= `novels/<site>/<title>`)。
fn cached_section_key<'a>(novel_prefix: &str, key: &'a str) -> Option<(&'a str, &'a str)> {
    let rest = key
        .strip_prefix(novel_prefix)?
        .strip_prefix('/')?
        .strip_prefix(SECTION_SAVE_DIR)?
        .strip_prefix('/')?;
    let (cache_dir, rest) = rest.split_once('/')?;
    if !CACHE_DIR_NAMES.contains(&cache_dir) {
        return None;
    }
    let (version, file_name) = rest.split_once('/')?;
    if file_name.is_empty() || !file_name.ends_with(".yaml") {
        return None;
    }
    Some((version, file_name))
}

/// `<novel_prefix>/本文/` 配下の差分キャッシュキーだけを列挙する。
/// `ObjectStore::list_page` はカーソル分割だが、D1 は全件を返す構造なので
/// まとめて取得しきる。
async fn list_cached_section_keys(
    runtime: &WorkerRuntime,
    novel_prefix: &str,
) -> Result<Vec<String>, String> {
    let sections_prefix = format!("{novel_prefix}/{SECTION_SAVE_DIR}/");
    let prefix = ObjectPrefix::new(sections_prefix).map_err(|error| error.to_string())?;
    let mut keys = Vec::new();
    let mut request =
        ObjectListRequest::new(prefix, NonZeroUsize::new(OBJECT_PAGE_LIMIT).unwrap());
    loop {
        let page = runtime
            .objects()
            .list_page(&request)
            .await
            .map_err(|error| error.to_string())?;
        for meta in &page.objects {
            if cached_section_key(novel_prefix, meta.key.as_ref()).is_some() {
                keys.push(meta.key.as_ref().to_string());
            }
        }
        let Some(cursor) = page.next_cursor else {
            break;
        };
        request = request.after(cursor);
    }
    Ok(keys)
}

// ---------------------------------------------------------------------------
// GET /api/diff_list (native: src/web/jobs.rs:1479 api_diff_list_get +
//                    :634 render_diff_list_html_for_target)
// ---------------------------------------------------------------------------

async fn diff_list_get(req: &Request, runtime: &WorkerRuntime) -> worker::Result<Response> {
    let url = req.url()?;
    let html = match query_param(&url, "target").as_deref() {
        Some(target) => render_diff_list_html(runtime, target).await,
        None => String::new(),
    };
    Response::from_html(html)
}

/// native `render_diff_list_html_for_target` の写し。解決失敗・記録なし・
/// キャッシュ列挙失敗・キャッシュ空は全部空文字列を返す (= native 同様)。
async fn render_diff_list_html(runtime: &WorkerRuntime, target: &str) -> String {
    let Some(id) = resolve_existing_id(runtime, target).await else {
        return String::new();
    };
    let Ok(Some(record)) = runtime.services.library.get(id.into()).await else {
        return String::new();
    };
    let Ok(object_keys) = NovelObjectKeys::new(
        &record.sitename,
        &record.file_title,
        record.use_subdirectory,
    ) else {
        return String::new();
    };
    let prefix = object_keys.prefix().as_ref().to_string();
    let Ok(mut keys) = list_cached_section_keys(runtime, &prefix).await else {
        return String::new();
    };
    if keys.is_empty() {
        return String::new();
    }

    // (version, [(index, file_name, key)]) に畳み込み、version 名の降順に並べる
    // (native `read_sorted_cache_dirs` の降順と同じ)。
    let mut versions: HashMap<String, Vec<(usize, String, String)>> = HashMap::new();
    for key in keys.drain(..) {
        let Some((version, file_name)) = cached_section_key(&prefix, &key) else {
            continue;
        };
        let index = file_name
            .split_once(' ')
            .and_then(|(index, _)| index.parse::<usize>().ok())
            .unwrap_or(0);
        versions
            .entry(version.to_string())
            .or_default()
            .push((index, file_name.to_string(), key));
    }
    let mut ordered: Vec<(String, Vec<(usize, String, String)>)> =
        versions.into_iter().collect();
    ordered.sort_by(|a, b| b.0.cmp(&a.0));

    let mut html = String::new();
    for (number, (version, mut sections)) in ordered.into_iter().enumerate() {
        html.push_str("<div class=\"diff-list-group\">");
        html.push_str(&format!(
            "<div class=\"diff-list-version\">{}&nbsp;&nbsp;-{}</div>",
            html_escape(&version),
            number + 1
        ));
        sections.sort_by_key(|(index, _, _)| *index);
        if sections.is_empty() {
            html.push_str("<div class=\"diff-list-entry\">(最新話のみのアップデート)</div></div>");
            continue;
        }
        for (_index, _file_name, key) in sections {
            let Ok(object_key) = ObjectKey::try_new(key.as_str()) else {
                continue;
            };
            let Ok(Some(bytes)) = runtime.objects().read_small(&object_key).await else {
                continue;
            };
            let Ok(section) = serde_yaml::from_slice::<SectionFile>(&bytes) else {
                continue;
            };
            html.push_str(&format!(
                "<div class=\"diff-list-entry\">第{}部分　{}</div>",
                html_escape(&section.index),
                html_escape(section.subtitle.trim_end())
            ));
        }
        html.push_str("</div>");
    }
    html
}

// ---------------------------------------------------------------------------
// POST /api/diff_list (native: src/web/jobs.rs:1439 api_diff_list)
// ---------------------------------------------------------------------------

async fn diff_list_post(req: &mut Request, runtime: &WorkerRuntime) -> worker::Result<Response> {
    let body: TargetsBody = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };
    let targets = targets_to_strings(&body.targets);
    if targets.len() > max_web_targets(runtime).await {
        return Response::from_json(&json!({ "error": "too many targets" }));
    }
    let mut diffs = Vec::new();
    for target in &targets {
        let id: i64 = match target.parse() {
            Ok(id) => id,
            Err(_) => continue,
        };
        let record = runtime.services.library.get(id.into()).await.ok().flatten();
        let Some(record) = record else {
            diffs.push(json!({
                "id": id,
                "title": format!("ID: {}", id),
                "content": "Novel not found",
            }));
            continue;
        };
        let content = match runtime.services.content.diff(id.into()).await {
            Ok(Some(bytes)) => {
                String::from_utf8(bytes).unwrap_or_else(|_| "読み取りエラー".to_string())
            }
            Ok(None) => "No diff".to_string(),
            Err(_) => "読み取りエラー".to_string(),
        };
        diffs.push(json!({
            "id": id,
            "title": record.title,
            "content": content,
        }));
    }
    Response::from_json(&json!({ "diffs": diffs }))
}

// ---------------------------------------------------------------------------
// POST /api/diff_clean (native: src/web/jobs.rs:1591 api_diff_clean)
// ---------------------------------------------------------------------------

async fn diff_clean(
    req: &mut Request,
    env: &Env,
    runtime: &WorkerRuntime,
) -> worker::Result<Response> {
    let body: DiffCleanBody = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };
    let target = match &body.target {
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let target = match validate_web_target_value(&target) {
        Ok(target) => target,
        Err(message) => return api_json_response(false, message),
    };

    let Some(id) = resolve_diff_target_id(runtime, env, &target).await else {
        return api_json_response(false, "差分の削除に失敗しました");
    };
    let Ok(Some(record)) = runtime.services.library.get(id.into()).await else {
        return api_json_response(false, "差分の削除に失敗しました");
    };
    let Ok(keys) = NovelObjectKeys::new(
        &record.sitename,
        &record.file_title,
        record.use_subdirectory,
    ) else {
        return api_json_response(false, "差分の削除に失敗しました");
    };
    let prefix = keys.prefix().as_ref().to_string();
    let cache_keys = match list_cached_section_keys(runtime, &prefix).await {
        Ok(keys) => keys,
        Err(_) => return api_json_response(false, "差分の削除に失敗しました"),
    };
    for key in cache_keys {
        let Ok(object_key) = ObjectKey::try_new(key.as_str()) else {
            continue;
        };
        if runtime.objects().delete(&object_key).await.is_err() {
            return api_json_response(false, "差分の削除に失敗しました");
        }
    }
    api_json_response(true, format!("Diff cleaned for {}", target))
}

// ---------------------------------------------------------------------------
// POST /api/inspect (native: src/web/jobs.rs:1131 api_inspect)
// ---------------------------------------------------------------------------

async fn inspect(req: &mut Request, runtime: &WorkerRuntime) -> worker::Result<Response> {
    let body: TargetsBody = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };
    let raw_targets = targets_to_strings(&body.targets);
    if raw_targets.len() > max_web_targets(runtime).await {
        return api_json_response(false, "too many targets");
    }
    for target in &raw_targets {
        if let Err(message) = validate_web_target_value(target) {
            return api_json_response(false, message);
        }
    }
    // `Inspector::save` (調査ログ.txt の生成) と出力の console broadcast は
    // native 専用。Worker にはどちらの経路も無いので成功を偽装しない。
    json_error(
        501,
        "not_supported_on_worker",
        Some("調査ログの取得はこの Worker 環境では利用できません"),
    )
}

// ---------------------------------------------------------------------------
// GET /api/notepad/read + POST /api/notepad/save
// (native: src/web/misc.rs:314 / :319)
// ---------------------------------------------------------------------------

/// `app_state('inv','notepad')` の行。`value_yaml` を正とし、移行前の行
/// (`value_json` のみ) も読めるようにする (`d1_repository` と同じ規則)。
#[derive(Debug, Deserialize)]
struct NotepadRow {
    #[serde(default)]
    value_yaml: Option<String>,
    #[serde(default)]
    value_json: Option<String>,
}

/// native `read_notepad` 相当: `value_yaml` の生テキストを返す。行が無い・
/// 読めない場合は空文字列 (native の `unwrap_or_default` と同じ)。
async fn read_notepad(env: &Env) -> String {
    let Ok(db) = env.d1("DB") else {
        return String::new();
    };
    let statement = match db
        .prepare("SELECT value_yaml, value_json FROM app_state WHERE scope = ? AND key = ?")
        .bind(&[
            JsValue::from_str(INVENTORY_SCOPE),
            JsValue::from_str(NOTEPAD_KEY),
        ]) {
        Ok(statement) => statement,
        Err(_) => return String::new(),
    };
    let row: Option<NotepadRow> = statement.first::<NotepadRow>(None).await.unwrap_or_default();
    let Some(row) = row else {
        return String::new();
    };
    match row.value_yaml.as_deref() {
        Some(yaml) if !yaml.trim().is_empty() && yaml.trim() != "{}" => yaml.to_string(),
        _ => row
            .value_json
            .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default(),
    }
}

/// native `set_raw_db` と同じ SQL: `value_yaml` に生テキストを置く
/// (`value_json` は `set_raw_db` 同様 `'{}'`)。
async fn write_notepad(env: &Env, content: &str) -> Result<(), String> {
    let db = env.d1("DB").map_err(|error| error.to_string())?;
    let statement = db
        .prepare(
            "INSERT INTO app_state (scope, key, value_json, value_yaml) VALUES (?, ?, '{}', ?)
             ON CONFLICT(scope, key) DO UPDATE SET value_yaml = excluded.value_yaml",
        )
        .bind(&[
            JsValue::from_str(INVENTORY_SCOPE),
            JsValue::from_str(NOTEPAD_KEY),
            JsValue::from_str(content),
        ])
        .map_err(|error| error.to_string())?;
    let result = statement.run().await.map_err(|error| error.to_string())?;
    if result.success() {
        Ok(())
    } else {
        Err(result
            .error()
            .unwrap_or_else(|| "D1 write failed".to_string()))
    }
}

/// native `notepad_object_id` の写し: 本文の SHA-256 hex。
fn notepad_object_id(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hex::encode(hasher.finalize())
}

/// native `notepad_response_value` と同じ形。
fn notepad_response_value(content: &str) -> serde_json::Value {
    json!({
        "content": content,
        "text": content,
        "object_id": notepad_object_id(content),
    })
}

async fn notepad_read(env: &Env) -> worker::Result<Response> {
    let content = read_notepad(env).await;
    Response::from_json(&notepad_response_value(&content))
}

async fn notepad_save(req: &mut Request, env: &Env) -> worker::Result<Response> {
    let body: serde_json::Value = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };
    let content = body["content"]
        .as_str()
        .or_else(|| body["text"].as_str())
        .unwrap_or("");
    if let Err(message) = narou_rs::application::settings_view::validate_web_text_size(
        content,
        MAX_WEB_TEXT_INPUT_BYTES,
        "notepad content",
    ) {
        return Response::from_json(&api_response(false, message));
    }
    let current_content = read_notepad(env).await;
    let current_object_id = notepad_object_id(&current_content);
    let request_object_id = body["object_id"].as_str().unwrap_or("");

    if request_object_id != current_object_id {
        return Response::from_json(&json!({
            "success": false,
            "conflict": true,
            "message": "他の画面でメモ帳が更新されたため再読み込みしました。内容を確認してからもう一度保存してください",
            "content": current_content,
            "text": current_content,
            "object_id": current_object_id,
        }));
    }

    match write_notepad(env, content).await {
        Ok(()) => {
            // native はここで `notepad.change` を PushServer へ broadcast するが、
            // Worker には同等の経路が無い (websocket.rs 参照)。応答 JSON は同一。
            Response::from_json(&json!({
                "success": true,
                "message": "Saved",
                "content": content,
                "text": content,
                "object_id": notepad_object_id(content),
            }))
        }
        Err(message) => Response::from_json(&api_response(false, message)),
    }
}

// ---------------------------------------------------------------------------
// GET /api/history + POST /api/clear_history
// (native: src/web/misc.rs:429 console_history / :444 clear_history)
// ---------------------------------------------------------------------------

/// Worker には `PushServer` のコンソール履歴が無い (`websocket.rs` は接続を
/// 受理するだけで履歴を保持しない)。`stream`/`format` パラメータは受理し、
/// native の履歴ゼロ状態と同じ形を返す。
fn console_history(req: &Request) -> worker::Result<Response> {
    let url = req.url()?;
    if query_param(&url, "format").as_deref() == Some("json") {
        return Response::from_json(&json!({ "history": "" }));
    }
    Response::ok("").and_then(|response| {
        let headers = Headers::new();
        headers.set("Content-Type", "text/plain; charset=utf-8")?;
        Ok(response.with_headers(headers))
    })
}

/// Worker の履歴は常に空なので、native のクリア後と同じ成功応答になる。
fn clear_history() -> worker::Result<Response> {
    api_json_response(true, "History cleared")
}

// ---------------------------------------------------------------------------
// GET+POST /api/taginfo.json (native: src/web/jobs.rs:2216 api_taginfo)
// ---------------------------------------------------------------------------

async fn taginfo(req_ids: Vec<serde_json::Value>, with_exclusion: bool, runtime: &WorkerRuntime) -> worker::Result<Response> {
    let selected_ids: Vec<i64> = req_ids
        .iter()
        .filter_map(|value| match value {
            serde_json::Value::Number(n) => n.as_i64(),
            serde_json::Value::String(s) => s.parse::<i64>().ok(),
            _ => None,
        })
        .collect();
    let records = runtime.services.library.records().await.unwrap_or_default();
    let new_tag_color = configured_tag_color(runtime).await;
    let mut tag_index: HashMap<String, Vec<i64>> = HashMap::new();
    let mut record_tags: HashMap<i64, Vec<String>> = HashMap::new();
    for record in &records {
        record_tags.insert(record.id, record.tags.clone());
        for tag in &record.tags {
            tag_index.entry(tag.clone()).or_default().push(record.id);
        }
    }
    let tag_names = tag_index.keys().cloned().collect::<Vec<_>>();
    let tag_colors = runtime
        .services
        .tag_colors
        .for_tags(tag_names, new_tag_color.as_deref())
        .await
        .unwrap_or_default();
    let mut selected_counts: HashMap<String, usize> = HashMap::new();
    for id in selected_ids.iter().copied() {
        if let Some(tags) = record_tags.get(&id) {
            for tag in tags {
                *selected_counts.entry(tag.clone()).or_insert(0) += 1;
            }
        }
    }

    let mut sorted_tags: Vec<(&String, &Vec<i64>)> = tag_index.iter().collect();
    sorted_tags.sort_by(|a, b| a.0.cmp(b.0));
    let mut tag_info = Vec::with_capacity(sorted_tags.len());
    for (tag, tag_ids) in sorted_tags {
        let color = tag_colors.get(tag).map(|c| c.as_str()).unwrap_or("");
        let class = tag_color_class(color);
        let escaped_tag = html_escape(tag);
        let html = format!(
            "<span class=\"tag-label {}\">{}</span>",
            class, escaped_tag
        );
        let mut entry = json!({
            "tag": tag,
            "count": selected_counts.get(tag.as_str()).copied().unwrap_or(0),
            "total_count": tag_ids.len(),
            "html": html,
        });
        if with_exclusion {
            entry["exclusion_html"] = json!(format!(
                "<span class=\"tag-label {} tag-exclusion\">{}</span>",
                class, escaped_tag
            ));
        }
        tag_info.push(entry);
    }
    Response::from_json(&tag_info)
}

/// GET 分派: `ids` クエリパラメータを `TagInfoBody.ids` と同じ値として扱う。
/// native は GET を受理しないが、ルート表で GET に割り当てられているため
/// POST と同じ応答を返す。
async fn taginfo_get(req: &Request, runtime: &WorkerRuntime) -> worker::Result<Response> {
    let url = req.url()?;
    let ids = url
        .query_pairs()
        .filter(|(key, _)| key == "ids")
        .map(|(_, value)| serde_json::Value::String(value.into_owned()))
        .collect();
    let with_exclusion = matches!(
        query_param(&url, "with_exclusion").as_deref(),
        Some("true") | Some("1")
    );
    taginfo(ids, with_exclusion, runtime).await
}

async fn taginfo_post(req: &mut Request, runtime: &WorkerRuntime) -> worker::Result<Response> {
    let body: TagInfoBody = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };
    taginfo(body.ids, body.with_exclusion.unwrap_or(false), runtime).await
}

// ---------------------------------------------------------------------------
// GET /api/version/current.json / /api/version/latest.json
// (native: src/web/misc.rs:54 version_current / :58 version_latest)
// ---------------------------------------------------------------------------

/// Worker のバージョン文字列。`narou_worker` のパッケージバージョンは
/// narou.rs と揃えてある。
const WORKER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// native `version_json` のキー構造を Worker 値に置き換えたもの。
/// Worker には self-update の経路もコンテナ判定の手掛かりも無いので、
/// 「ランタイム管理されている = 自動更新不可」を `container` / `self_update_*`
/// で表し、`develop` / `local_build` は偽装しない (false 固定)。
fn version_current() -> worker::Result<Response> {
    Response::from_json(&json!({
        "version": WORKER_VERSION,
        "name": APP_NAME,
        "develop": false,
        "local_build": false,
        "container": true,
        "self_update_supported": false,
        "build_variant": "gpl",
    }))
}

/// native `version_is_newer` の写し (worker 側は version core のみ比較)。
fn version_is_newer(latest: &str, current: &str) -> bool {
    if latest.is_empty() {
        return false;
    }
    match narou_rs::application::version_compare::version_compare(latest, current) {
        Some(ord) => ord == std::cmp::Ordering::Greater,
        None => latest != current,
    }
}

/// GitHub Releases を参照して最新バージョンを返す。ネットワーク失敗も
/// `{success: false, current_version, message, url}` 形で native と同じ。
async fn version_latest() -> worker::Result<Response> {
    let headers = Headers::new();
    headers.set("User-Agent", APP_NAME)?;
    let mut init = RequestInit::new();
    init.with_method(Method::Get);
    init.with_headers(headers);
    let request = Request::new_with_init(RELEASES_API_URL, &init)
        .map_err(|error| worker::Error::RustError(error.to_string()))?;
    let response = Fetch::Request(request).send().await;
    let url_fallback = RELEASES_PAGE_URL;

    match response {
        Ok(mut resp) if (200..=299).contains(&resp.status_code()) => {
            let json_text = resp.text().await.unwrap_or_default();
            let json: serde_json::Value = serde_json::from_str(&json_text).unwrap_or_default();
            let latest = json["tag_name"]
                .as_str()
                .or_else(|| json["name"].as_str())
                .map(narou_rs::application::version_compare::version_core)
                .unwrap_or_default();
            let current_plain = narou_rs::application::version_compare::version_core(WORKER_VERSION);
            Response::from_json(&json!({
                "success": true,
                "current_version": WORKER_VERSION,
                "latest_version": latest,
                "update_available": version_is_newer(&latest, &current_plain),
                "develop": false,
                "local_build": false,
                "container": true,
                "self_update_supported": false,
                "self_update_unavailable_reason": "Cloudflare Workers 環境では自動アップデートできません。wrangler deploy で更新してください",
                "build_variant": "gpl",
                "variant_choice_required": false,
                "url": json["html_url"].as_str().unwrap_or(url_fallback),
            }))
        }
        Ok(resp) => Response::from_json(&json!({
            "success": false,
            "current_version": WORKER_VERSION,
            "message": format!("latest version request failed: {}", resp.status_code()),
            "url": url_fallback,
        })),
        Err(error) => Response::from_json(&json!({
            "success": false,
            "current_version": WORKER_VERSION,
            "message": error.to_string(),
            "url": url_fallback,
        })),
    }
}

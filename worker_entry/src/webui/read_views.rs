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
//! - `GET /api/history` / `POST /api/clear_history` — PushHub DO
//!   (`push_hub.rs`) が永続化するコンソール履歴を読む / 消す。native
//!   `console_history` / `clear_history` と同じ受理形 (`stream` 既定
//!   `stdout`、`format=json`) で、DO が読めないときは空履歴に潰さず 502。
//! - `GET`/`POST /api/taginfo.json` — タグ情報配列。native は POST のみだが、
//!   ルート表で GET に割り当てられているため GET (クエリ `ids` 指定) も受理する。
//! - `GET /api/version/current.json` / `GET /api/version/latest.json` —
//!   キー集合は共有の `VersionInfoPayload`、値は Worker の実値。
//!   バージョン文字列は `env!("CARGO_PKG_VERSION")` (`narou_worker` の
//!   パッケージバージョンは narou.rs と揃えてある)。
//! - `POST /api/inspect` — `調査ログ.txt` は native の `Inspector::save`
//!   (ローカル FS への書き込み) でしか生成されず、出力の送達にも PushServer の
//!   console broadcast が要る。どちらも Worker には無いので 501
//!   `not_supported_on_worker` を返す (成功を偽装しない)。

use std::collections::HashMap;

use narou_rs::application::ApplicationError;
use narou_rs::application::aliases::resolve_alias_target;
use narou_rs::platform::{NovelId, NovelObjectKeys};
use serde::Deserialize;
use serde_json::json;
use wasm_bindgen::JsValue;
use worker::{console_log, Env, Fetch, Headers, Method, Request, RequestInit, Response};

use crate::composition::WorkerRuntime;

use narou_rs::application::settings_view::MAX_WEB_TEXT_INPUT_BYTES;
use narou_rs::application::web_payloads::{VersionInfoPayload, version_update_available};
use narou_rs::application::webui::diff::{
    DIFF_CLEAN_FAILED, diff_cleaned_message, diff_content_label, diff_list_entry,
    diff_missing_entry, plan_diff_cache_clean, read_diff_cache_catalog, render_diff_list_html,
};
use narou_rs::application::webui::mutations::MutationNotice;
use narou_rs::application::webui::notepad::{
    notepad_conflict_payload, notepad_object_id, notepad_response_value, notepad_saved_payload,
};
use narou_rs::application::webui::tag_views::{tag_names, taginfo_entries};
use narou_rs::application::webui::{targets_to_strings, validate_web_target_value};

use super::{api_response, configured_tag_color, json_error, max_web_targets, query_param};

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

    // バージョン系と履歴系はリポジトリ/D1 サービスに触れないので runtime
    // 組み立てをスキップする (履歴は PushHub DO だけを読む)。
    match route {
        Route::VersionCurrent => return version_current(),
        Route::VersionLatest => return version_latest().await,
        Route::History => return console_history(&req, &env).await,
        Route::ClearHistory => return clear_history(&env).await,
        _ => {}
    }

    let runtime = match WorkerRuntime::build_ui(&env).await {
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
        Route::TagInfoGet => taginfo_get(&req, &runtime).await,
        Route::TagInfoPost => taginfo_post(&mut req, &runtime).await,
        Route::History | Route::ClearHistory => unreachable!(),
        Route::VersionCurrent | Route::VersionLatest => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// native `map_application_error` (`src/web/novels.rs`) と同じステータス割当。
fn map_application_error(error: ApplicationError) -> worker::Result<Response> {
    let (status, code, message) = match error {
        ApplicationError::InvalidRequest(message) => (400, "bad_request", message),
        ApplicationError::Conflict(message) => (409, "conflict", message),
        ApplicationError::NotFound(message) => (404, "not_found", message),
        ApplicationError::Platform(message) => (500, "internal_error", message),
    };
    json_error(status, code, Some(&message))
}

/// native `ApiResponse` を JSON ボディとして返す (ステータスは native 同様 200)。
fn api_json_response(success: bool, message: impl Into<String>) -> worker::Result<Response> {
    Response::from_json(&api_response(success, message))
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
/// id / URL / ncode / タイトルの順に既存 ID を引く。URL 解決は
/// `services.site_definitions.resolve_toc_url` を優先し、ヒットしないときだけ
/// 保存済み `toc_url` 完全一致へフォールバックする — 規則は
/// `TargetLookupPlan::READ_WITH_RAW_FALLBACK` が唯一の定義。
async fn resolve_existing_id(runtime: &WorkerRuntime, target: &str) -> Option<i64> {
    let library = &runtime.services.library;
    library
        .resolve_target(
            narou_rs::application::target_lookup::TargetLookupPlan::READ_WITH_RAW_FALLBACK,
            &HashMap::new(),
            narou_rs::application::target_lookup::UrlSource::Provider(
                &*runtime.services.site_definitions,
            ),
            target,
        )
        .await
        .ok()
        .flatten()
        .map(|record| record.id)
}

/// `narou diff --clean <target>` がサブプロセス内で通る解決順
/// (`commands::resolve_target_to_id`): エイリアス → ID → URL → ncode → タイトル
/// (Other はタイトル→ncode の順でフォールバック)。alias と trim は native の
/// 前段と同じ順でここで適用し、探索は `TargetLookupPlan::ALIAS_APPLIED` が
/// 唯一の定義。
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
    // diff_clean の削除対象を stale なキャッシュ/replica で確定しない
    // (write_services = primary 直行)。
    let services = runtime.write_services();
    services
        .library
        .resolve_target(
            narou_rs::application::target_lookup::TargetLookupPlan::ALIAS_APPLIED,
            &aliases,
            narou_rs::application::target_lookup::UrlSource::Provider(&*services.site_definitions),
            effective,
        )
        .await
        .ok()
        .flatten()
        .map(|record| record.id)
}


// ---------------------------------------------------------------------------
// GET /api/diff_list (native: src/web/jobs.rs:1479 api_diff_list_get +
//                    :634 render_diff_list_html_for_target)
// ---------------------------------------------------------------------------

async fn diff_list_get(req: &Request, runtime: &WorkerRuntime) -> worker::Result<Response> {
    let url = req.url()?;
    let html = match query_param(&url, "target").as_deref() {
        Some(target) => diff_list_html_for_target(runtime, target).await,
        None => String::new(),
    };
    Response::from_html(html)
}

/// 解決失敗・記録なし・列挙失敗は native と同じ空文字列。保存済みの
/// バージョンの分類と本文デコードは共通のカタログに任せる。
async fn diff_list_html_for_target(runtime: &WorkerRuntime, target: &str) -> String {
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
    let Ok(versions) =
        read_diff_cache_catalog(runtime.objects().as_ref(), object_keys.prefix(), &[]).await
    else {
        return String::new();
    };
    render_diff_list_html(&versions)
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
            diffs.push(diff_missing_entry(id));
            continue;
        };
        let content = diff_content_label(runtime.services.content.diff(id.into()).await);
        diffs.push(diff_list_entry(id, &record.title, &content));
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
        return api_json_response(false, DIFF_CLEAN_FAILED);
    };
    // 削除対象の確定と削除そのものを stale なスナップショットへ依存させない
    // (読み取りも primary + キャッシュ無しの更新系で行う)。
    let Ok(Some(record)) = runtime.write_services().library.get(id.into()).await else {
        return api_json_response(false, DIFF_CLEAN_FAILED);
    };
    let Ok(keys) = NovelObjectKeys::new(
        &record.sitename,
        &record.file_title,
        record.use_subdirectory,
    ) else {
        return api_json_response(false, DIFF_CLEAN_FAILED);
    };
    let objects = runtime.write_objects();
    let cache_keys = match plan_diff_cache_clean(objects.as_ref(), keys.prefix()).await {
        Ok(keys) => keys,
        Err(_) => return api_json_response(false, DIFF_CLEAN_FAILED),
    };
    for key in cache_keys {
        if objects.delete(&key).await.is_err() {
            return api_json_response(false, DIFF_CLEAN_FAILED);
        }
    }
    api_json_response(true, diff_cleaned_message(&target))
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
#[derive(Debug, Clone, Deserialize)]
struct NotepadRow {
    #[serde(default)]
    value_yaml: Option<String>,
    #[serde(default)]
    value_json: Option<String>,
}

fn notepad_content(row: NotepadRow) -> String {
    match narou_rs::application::webui::notepad::notepad_content_from_columns(
        row.value_yaml.as_deref(), row.value_json.as_deref(),
    ) {
        std::borrow::Cow::Borrowed(_) => row.value_yaml.unwrap_or_default(),
        std::borrow::Cow::Owned(content) => content,
    }
}

/// `app_state('inv','notepad')` の生行を読む。失敗は呼び出し側で扱う
/// (GET は空文字へ畳み、POST は保存を止めてエラー応答にする)。
async fn read_notepad_row(db: &crate::db_handle::DbHandle) -> Result<Option<NotepadRow>, String> {
    let statement = db
        .prepare("SELECT value_yaml, value_json FROM app_state WHERE scope = ? AND key = ?")
        .bind(&[
            JsValue::from_str(INVENTORY_SCOPE),
            JsValue::from_str(NOTEPAD_KEY),
        ])
        .map_err(|error| error.to_string())?;
    statement
        .first::<NotepadRow>(None)
        .await
        .map_err(|error| error.to_string())
}

/// native `read_notepad` 相当: `value_yaml` の生テキストを返す。行が無い・
/// 読めない場合は空文字列 (native の `unwrap_or_default` と同じ)。
async fn read_notepad(env: &Env) -> String {
    let Ok(db) = env.d1("DB") else {
        return String::new();
    };
    let db = crate::db_handle::DbHandle::ui(std::sync::Arc::new(db));
    read_notepad_row(&db)
        .await
        .ok()
        .flatten()
        .map(notepad_content)
        .unwrap_or_default()
}

/// `write_notepad` の結果。`Conflict` は競合ガード (0 行適用) を表し、
/// D1 エラーではない。
enum NotepadSave {
    Saved,
    Conflict,
    Failed(String),
}

/// native `set_raw_db` と同じ更新内容を、条件付きの UPSERT として primary
/// へ書く。ガードは「読んだ時点の生カラム値がそのまま残っている」ことで、
/// `IS` 比較 (NULL 同値) を使う。読み取り→書き込みを 1 文に畳むことで、
/// 別 isolate の保存と競合したときに確実に負け側が 0 行適用になる
/// (行が無いときは INSERT、競合行が生えていたら DO UPDATE の WHERE が
/// 外れて 0 行 = Conflict)。
async fn write_notepad(
    db: &crate::db_handle::DbHandle,
    expected: &Option<NotepadRow>,
    content: &str,
) -> NotepadSave {
    let expected_yaml = expected
        .as_ref()
        .and_then(|row| row.value_yaml.as_deref())
        .map(JsValue::from_str)
        .unwrap_or(JsValue::NULL);
    let expected_json = expected
        .as_ref()
        .and_then(|row| row.value_json.as_deref())
        .map(JsValue::from_str)
        .unwrap_or(JsValue::NULL);
    let statement = match db
        .prepare(
            "INSERT INTO app_state (scope, key, value_json, value_yaml) VALUES (?, ?, '{}', ?)
             ON CONFLICT(scope, key) DO UPDATE SET
               value_json = excluded.value_json, value_yaml = excluded.value_yaml
             WHERE app_state.value_yaml IS ? AND app_state.value_json IS ?",
        )
        .bind(&[
            JsValue::from_str(INVENTORY_SCOPE),
            JsValue::from_str(NOTEPAD_KEY),
            JsValue::from_str(content),
            expected_yaml,
            expected_json,
        ]) {
        Ok(statement) => statement,
        Err(error) => return NotepadSave::Failed(error.to_string()),
    };
    let result = match statement.run().await {
        Ok(result) => result,
        Err(error) => return NotepadSave::Failed(error.to_string()),
    };
    if !result.success() {
        return NotepadSave::Failed(
            result
                .error()
                .unwrap_or_else(|| "D1 write failed".to_string()),
        );
    }
    let changed = result
        .meta()
        .ok()
        .flatten()
        .and_then(|meta| meta.changes)
        .unwrap_or(0);
    if changed >= 1 {
        NotepadSave::Saved
    } else {
        NotepadSave::Conflict
    }
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
    // 楽観ロックの比較は常に primary で行う — セッション (replica) に
    // 読ませると他 isolate の直前の保存を見落として黙って上書きする。
    // 読み取りエラーは「行が無い」と区別して保存を止める (native の
    // `unwrap_or_default` と違い、失敗を成功に見せない)。
    let db = crate::db_handle::DbHandle::primary(std::sync::Arc::new(match env.d1("DB") {
        Ok(db) => db,
        Err(error) => {
            return Response::from_json(&api_response(false, error.to_string()));
        }
    }));
    let current_row = match read_notepad_row(&db).await {
        Ok(row) => row,
        Err(message) => return Response::from_json(&api_response(false, message)),
    };
    let current_content = current_row.as_ref()
        .map(|row| narou_rs::application::webui::notepad::notepad_content_from_columns(
            row.value_yaml.as_deref(), row.value_json.as_deref(),
        )).unwrap_or_default();
    let current_object_id = notepad_object_id(&current_content);
    let request_object_id = body["object_id"].as_str().unwrap_or("");

    if request_object_id != current_object_id {
        return Response::from_json(&notepad_conflict_payload(&current_content));
    }

    // CAS: 「読んだ行のままである」ことを条件に 1 文で書き換える。別
    // isolate の保存が間に入った場合は 0 行適用で負け側になる。
    match write_notepad(&db, &current_row, content).await {
        NotepadSave::Saved => {
            // native `notepad_save` と同じく、他のクライアントのメモ帳表示を
            // 保存後の内容へ更新させる。
            crate::push_hub::broadcast_mutations(env, MutationNotice::Notepad { content }).await;
            Response::from_json(&notepad_saved_payload(content))
        }
        NotepadSave::Conflict => {
            // 書き込み競合 — 勝った側の内容を読み返して競合応答に載せる。
            let conflict_row = read_notepad_row(&db).await.ok().flatten();
            let conflict_content = conflict_row.map(notepad_content).unwrap_or_default();
            Response::from_json(&notepad_conflict_payload(&conflict_content))
        }
        NotepadSave::Failed(message) => Response::from_json(&api_response(false, message)),
    }
}

// ---------------------------------------------------------------------------
// GET /api/history + POST /api/clear_history
// (native: src/web/misc.rs:429 console_history / :444 clear_history)
// ---------------------------------------------------------------------------

/// PushHub DO の履歴を native `console_history` と同じ形で返す。
///
/// `stream` (既定 `stdout`) と `format=json` は native と同じ解釈。Worker の
/// 履歴は DO (`push_hub.rs`) が永続化しており、上限 (`HISTORY_CAPACITY` /
/// `HISTORY_MAX_BYTES`) を超えた分は古い方から落ちる — 能力の上限は
/// `/api/webui/config` の `capabilities.console_history` が伝える。
/// DO が読めないときは空履歴に潰さず 502 を返す。
async fn console_history(req: &Request, env: &Env) -> worker::Result<Response> {
    let url = req.url()?;
    let stream = query_param(&url, "stream").unwrap_or_else(|| "stdout".to_string());
    let history = match crate::push_hub::history_text(env, &stream).await {
        Ok(history) => history,
        Err(error) => {
            console_log!("push hub history failed: {error}");
            return json_error(
                502,
                "push_hub_unavailable",
                Some("履歴を取得できませんでした"),
            );
        }
    };
    if query_param(&url, "format").as_deref() == Some("json") {
        return Response::from_json(&json!({ "history": history }));
    }
    Response::ok(history).and_then(|response| {
        let headers = Headers::new();
        headers.set("Content-Type", "text/plain; charset=utf-8")?;
        Ok(response.with_headers(headers))
    })
}

/// PushHub DO の履歴 (永続チャンク + 再生バッファ) を実際に消す。
/// native と同じ成功応答を返し、消去に失敗したときは成功を偽らない。
async fn clear_history(env: &Env) -> worker::Result<Response> {
    match crate::push_hub::clear_remote_history(env).await {
        Ok(()) => api_json_response(true, "History cleared"),
        Err(error) => {
            console_log!("push hub clear failed: {error}");
            json_error(
                502,
                "push_hub_unavailable",
                Some("履歴を消去できませんでした"),
            )
        }
    }
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
    let mut records = runtime.services.library.records().await.unwrap_or_default();
    narou_rs::application::attribute_tags::project(
        &mut records,
        narou_rs::application::attribute_tags::enabled(&runtime.services.settings).await,
    );
    let new_tag_color = configured_tag_color(runtime).await;
    let colors = runtime
        .services
        .tag_colors
        .for_tags(tag_names(&records), new_tag_color.as_deref())
        .await
        .unwrap_or_default();
    Response::from_json(&taginfo_entries(
        &records,
        &selected_ids,
        with_exclusion,
        &colors,
    ))
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

/// Worker は常に自動更新不可 (native `version::self_update_unavailable_reason`
/// に相当する固定文言)。
const SELF_UPDATE_UNAVAILABLE_REASON: &str =
    "Cloudflare Workers 環境では自動アップデートできません。wrangler deploy で更新してください";

/// Worker のバージョン事実 (キー集合は共有の [`VersionInfoPayload`])。
/// Worker には self-update の経路もコンテナ判定の手掛かりも無いので、
/// 「ランタイム管理されている = 自動更新不可」を `container` / `self_update_*`
/// で表し、`develop` / `local_build` は偽装しない (false 固定)。
fn worker_version_info() -> VersionInfoPayload {
    VersionInfoPayload {
        version: WORKER_VERSION.to_string(),
        name: APP_NAME.to_string(),
        develop: false,
        local_build: false,
        container: true,
        self_update_supported: false,
        build_variant: "gpl",
    }
}

fn version_current() -> worker::Result<Response> {
    Response::from_json(&worker_version_info().current())
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
    let info = worker_version_info();

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
            let update_available = version_update_available(&latest, &current_plain);
            Response::from_json(&info.latest(
                &latest,
                update_available,
                json["html_url"].as_str().unwrap_or(url_fallback),
                Some(SELF_UPDATE_UNAVAILABLE_REASON),
                false,
            ))
        }
        Ok(resp) => Response::from_json(&info.latest_failure(
            &format!("latest version request failed: {}", resp.status_code()),
            url_fallback,
        )),
        Err(error) => Response::from_json(&info.latest_failure(&error.to_string(), url_fallback)),
    }
}

#[cfg(test)]
mod tests {
    use super::{NotepadRow, notepad_content};

    #[test]
    fn cleared_canonical_notepad_value_cannot_fall_back_to_stale_json() {
        let legacy = notepad_content(NotepadRow {
            value_yaml: Some("{}".to_string()),
            value_json: Some(r#""old note""#.to_string()),
        });
        assert_eq!(legacy, "old note");

        let cleared = notepad_content(NotepadRow {
            value_yaml: Some(String::new()),
            value_json: Some("{}".to_string()),
        });
        assert_eq!(cleared, "");
    }
}

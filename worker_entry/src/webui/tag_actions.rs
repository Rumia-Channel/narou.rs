//! Web UI のタグ操作系エンドポイント。
//!
//! JSON parity with the native handlers:
//! - `src/web/tags.rs` `edit_tag` — `POST /api/edit_tag` (選択中の小説へ一括
//!   タグ付け/タグ外し。states: `{ "タグ名": 0|1|2 }` で 0=削除・1=維持・2=追加)
//! - `src/web/misc.rs` `tag_change_color` — `POST /api/tag/change_color`
//!   (タグ色の保存/解除。保存先は D1 `tag_colors` = `D1TagColorStore`)
//!
//! `edit_tag` の対象 ID 並び替えは native と同じく「サーバーが保持する現在の
//! ソート状態」を使う。native は `server_setting.yaml` の `current_sort` キーを
//! 読む (`web::sort_state::load_current_sort_state`); Worker では
//! `webui::ui_prefs` と同じく global スコープの `current_sort` 行 (D1
//! `app_state`) がその値そのもの。リクエストが送ってくる `sort_state` /
//! `timestamp` は native と同じく参照しない (サーバー保存値が常に正)。
//!
//! native の `apply_tag_change` は `push_server.broadcast_event` で
//! `table.reload` / `tag.updateCanvas` を配信するが、Worker には共有
//! ブロードキャスト層が無い (`websocket.rs` 参照) ので省略する。応答 JSON の
//! 形・キー名・ステータスコードは native と一致させる。

use std::collections::{HashMap, HashSet};

use narou_rs::application::web_payloads::ApiResponse;
use narou_rs::application::{ApplicationError, TagAction, TagChangeRequest};
use narou_rs::db::{NovelRecord, compare_records_by_key, sort_keys};
use narou_rs::platform::NovelId;
use narou_rs::setting_core::SettingScope;
use serde::Deserialize;
use serde_json::json;
use worker::{Method, Request, Response, console_log};

use crate::composition::WorkerRuntime;

/// native `crate::web::MAX_WEB_TARGETS_PER_REQUEST` と同じ件数上限。
const MAX_WEB_TARGETS_PER_REQUEST: usize = 100_000;
/// native `crate::web::MAX_WEB_TAGS_PER_REQUEST` と同じ件数上限。
const MAX_WEB_TAGS_PER_REQUEST: usize = 128;
/// native `crate::web::MAX_WEB_TAG_LENGTH` と同じバイト上限。
const MAX_WEB_TAG_LENGTH: usize = 255;

/// native の `server_setting` 内キー名 (`webui::ui_prefs` と同じ行を読む)。
const CURRENT_SORT_KEY: &str = "current_sort";
/// native `DEFAULT_CURRENT_SORT_COLUMN` / `DEFAULT_CURRENT_SORT_DIR` と同じ既定値。
const DEFAULT_CURRENT_SORT_COLUMN: usize = 2;
const DEFAULT_CURRENT_SORT_DIR: &str = "desc";

/// 親がこのハンドラへ割り当てるルート: `POST /api/edit_tag`,
/// `POST /api/tag/change_color`。
pub async fn handle(mut req: Request, env: worker::Env) -> worker::Result<Response> {
    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    enum Route {
        EditTag,
        ChangeColor,
    }
    let route = match (req.method(), req.path().as_str()) {
        (Method::Post, "/api/edit_tag") => Route::EditTag,
        (Method::Post, "/api/tag/change_color") => Route::ChangeColor,
        (_, "/api/edit_tag") | (_, "/api/tag/change_color") => {
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

    // native の axum `Json` 抽出と同じく、不正な JSON 本文は 400。
    let body: serde_json::Value = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "invalid_request", Some("invalid JSON body")),
    };

    match route {
        Route::EditTag => edit_tag(&runtime, body).await,
        Route::ChangeColor => tag_change_color(&runtime, body).await,
    }
}

/// `lib.rs::json_error` と同じ JSON 形 (`{error: {code, message?}}`)。
/// あちらは private なので形だけ合わせてここに持つ。
fn json_error(status: u16, code: &str, message: Option<&str>) -> worker::Result<Response> {
    let payload = match message {
        Some(message) => json!({ "error": { "code": code, "message": message } }),
        None => json!({ "error": { "code": code } }),
    };
    Response::from_json(&payload).map(|response| response.with_status(status))
}

/// native `edit_tag` の失敗応答と同じ形 (`{success: false, error}`)。native
/// は 4xx/5xx のステータスでも本文で `error` を返すので、こちらも常に 200 で
/// この形を返す。
fn fail_response(message: impl Into<String>) -> serde_json::Value {
    json!({ "success": false, "error": message.into() })
}

// ---------------------------------------------------------------------------
// POST /api/edit_tag (native: src/web/tags.rs:186 edit_tag)
// ---------------------------------------------------------------------------

/// native `EditTagBody` (`src/web/state.rs:90`) と同じ受理形。
#[derive(Debug, Deserialize)]
struct EditTagBody {
    ids: Vec<serde_json::Value>,
    states: HashMap<String, serde_json::Value>,
    /// native はリクエストのソート状態を参照しない (`request_sort_state` は
    /// 常に `None`) ので受け取るだけで捨てる。
    #[serde(default)]
    #[allow(dead_code)]
    sort_state: Option<serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    timestamp: Option<u64>,
}

/// native `apply_tag_change` の写し (push 通知を除く)。エラーは呼び出し側で
/// `{"success": false, "error": message}` に写すためメッセージだけ返す。
async fn apply_tag_change(
    runtime: &WorkerRuntime,
    ids: &[i64],
    action: TagAction,
    tags: Vec<String>,
) -> Result<narou_rs::application::TagChangeResult, String> {
    runtime
        .services
        .novel_actions
        .change_tags(&TagChangeRequest {
            ids: ids.iter().copied().map(NovelId::from).collect(),
            action,
            tags,
        })
        .await
        .map_err(|error| match error {
            ApplicationError::InvalidRequest(message)
            | ApplicationError::NotFound(message)
            | ApplicationError::Platform(message) => message,
        })
}

/// native `ensure_all_ids_found` の写し: 存在しない ID が 1 件でもあれば
/// 先頭のものを `ID: <n>` 形式で報告する。
fn ensure_all_ids_found(
    result: &narou_rs::application::TagChangeResult,
) -> Result<(), String> {
    if result.missing.is_empty() {
        Ok(())
    } else {
        Err(format!("ID: {}", result.missing[0].0))
    }
}

/// native `sort_ids_for_request` の写し。native はサーバー保存のソート状態で
/// 並び替えるので、こちらも `current_sort` 設定 (D1 `app_state` global) を読む。
async fn sort_ids_for_request(runtime: &WorkerRuntime, ids: &[i64]) -> Vec<i64> {
    let records = runtime
        .services
        .library
        .records()
        .await
        .unwrap_or_default();
    sort_ids_from_records(ids, &records, current_sort_state(runtime).await)
}

/// native `sort_ids_from_records` の写し。リクエスト指定のソートは native と
/// 同じく採用せず、常にサーバー保存値 (`current_sort`) で並び替える。
/// レコードが存在しない ID は落ちる (native も `HashSet` で絞り込む)。
fn sort_ids_from_records(
    ids: &[i64],
    records: &[NovelRecord],
    sort_state: CurrentSortState,
) -> Vec<i64> {
    let selected = ids.iter().copied().collect::<HashSet<_>>();
    let mut records = records
        .iter()
        .filter(|record| selected.contains(&record.id))
        .cloned()
        .collect::<Vec<_>>();
    sort_records(&mut records, &sort_state);
    records.into_iter().map(|record| record.id).collect()
}

/// native `sort_records` の写し: サーバー保存の列・方向でソートし、同値は
/// id で安定させる。未知キーは id 比較へフォールバックする。
fn sort_records(records: &mut [NovelRecord], sort_state: &CurrentSortState) {
    let sort_key = sort_keys().get(sort_state.column).copied().unwrap_or("id");
    let reverse = sort_state.dir == "desc";
    records.sort_by(|a, b| {
        let ordering = compare_records_by_key(a, b, sort_key).then_with(|| a.id.cmp(&b.id));
        if reverse { ordering.reverse() } else { ordering }
    });
}

/// native `load_current_sort_state` の Worker 版。読み取り不能・未設定・
/// 形式不正は既定値 (column=2, dir="desc") に落ちる点まで同じ。
async fn current_sort_state(runtime: &WorkerRuntime) -> CurrentSortState {
    runtime
        .services
        .settings
        .get_raw(SettingScope::Global, CURRENT_SORT_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|value| normalize_current_sort_value(&value))
        .unwrap_or_else(default_current_sort_state)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CurrentSortState {
    column: usize,
    dir: String,
}

fn default_current_sort_state() -> CurrentSortState {
    CurrentSortState {
        column: DEFAULT_CURRENT_SORT_COLUMN,
        dir: DEFAULT_CURRENT_SORT_DIR.to_string(),
    }
}

/// native `normalize_current_sort_value` と同じ受理形式: `column`/`dir` キー
/// (Ruby シンボル由来の `:column`/`:dir` も許容)、列は番号または数字文字列、
/// 方向は `asc`/`desc` (先頭 `:` は剥がす)。`webui::ui_prefs` と同じ変換。
fn normalize_current_sort_value(sort_state: &serde_yaml::Value) -> Option<CurrentSortState> {
    let sort_state = sort_state.as_mapping()?;
    let column = sort_state
        .get(serde_yaml::Value::String("column".to_string()))
        .or_else(|| sort_state.get(serde_yaml::Value::String(":column".to_string())))
        .and_then(normalize_sort_column)?;
    let dir = sort_state
        .get(serde_yaml::Value::String("dir".to_string()))
        .or_else(|| sort_state.get(serde_yaml::Value::String(":dir".to_string())))
        .and_then(normalize_sort_dir)?;
    Some(CurrentSortState { column, dir })
}

fn normalize_sort_column(value: &serde_yaml::Value) -> Option<usize> {
    let column = match value {
        serde_yaml::Value::Number(number) => number.as_u64().map(|value| value as usize)?,
        serde_yaml::Value::String(text) if text.chars().all(|ch| ch.is_ascii_digit()) => {
            text.parse::<usize>().ok()?
        }
        _ => return None,
    };
    sort_keys().get(column).map(|_| column)
}

fn normalize_sort_dir(value: &serde_yaml::Value) -> Option<String> {
    let text = match value {
        serde_yaml::Value::String(text) => text.as_str(),
        _ => return None,
    };
    let text = text.trim_start_matches(':');
    match text {
        "asc" | "desc" => Some(text.to_string()),
        _ => None,
    }
}

/// native `normalize_web_tag_name` の写し: トリム後 `tag:` プレフィックスを
/// 剥がして `validate_web_tag_name` と同じ検証を通す。
fn normalize_web_tag_name(tag: &str) -> Result<String, String> {
    let trimmed = tag.trim();
    let stripped = trimmed.strip_prefix("tag:").unwrap_or(trimmed);
    validate_web_tag_name(stripped)
}

/// native `validate_web_tag_name` (`src/web/mod.rs:200`) の写し。
/// エラーメッセージも一致させる。
fn validate_web_tag_name(tag: &str) -> Result<String, String> {
    let trimmed = tag.trim();
    if trimmed.is_empty() {
        return Err("tag is required".to_string());
    }
    if trimmed.starts_with('-') {
        return Err("tag contains invalid characters".to_string());
    }
    if trimmed.len() > MAX_WEB_TAG_LENGTH {
        return Err("tag is too long".to_string());
    }
    if trimmed.chars().any(|ch| ch.is_control()) {
        return Err("tag contains invalid characters".to_string());
    }
    Ok(trimmed.to_string())
}

/// native `edit_tag` (`src/web/tags.rs:186`) の写し。
///
/// `states` は `{ "タグ名": 0|1|2 }` (0=削除・1=維持・2=追加)。削除を先に、
/// 追加を後に適用する順序も native と同じ。応答は常に
/// `{"success": bool, "error"?: string}` (失敗時も HTTP 200)。
async fn edit_tag(runtime: &WorkerRuntime, body: serde_json::Value) -> worker::Result<Response> {
    let body: EditTagBody = match serde_json::from_value(body) {
        Ok(body) => body,
        Err(_) => return json_error(400, "invalid_request", Some("invalid request body")),
    };
    let max_targets = runtime
        .services
        .settings
        .web_target_limit(MAX_WEB_TARGETS_PER_REQUEST)
        .await;
    if body.ids.len() > max_targets {
        return Response::from_json(&fail_response("too many ids"));
    }
    if body.states.len() > MAX_WEB_TAGS_PER_REQUEST {
        return Response::from_json(&fail_response("too many tags"));
    }
    let ids: Vec<i64> = body
        .ids
        .iter()
        .filter_map(|v| match v {
            serde_json::Value::Number(n) => n.as_i64(),
            serde_json::Value::String(s) => s.parse::<i64>().ok(),
            _ => None,
        })
        .collect();
    let ids = sort_ids_for_request(runtime, &ids).await;

    if ids.is_empty() {
        return Response::from_json(&fail_response("No valid IDs"));
    }

    let mut tags_to_add: Vec<String> = Vec::new();
    let mut tags_to_delete: Vec<String> = Vec::new();

    for (tag, state_val) in &body.states {
        let tag = match normalize_web_tag_name(tag) {
            Ok(tag) => tag,
            Err(error) => {
                return Response::from_json(&fail_response(error));
            }
        };
        let s = match state_val {
            serde_json::Value::Number(n) => n.as_i64().unwrap_or(1),
            serde_json::Value::String(s) => s.parse::<i64>().unwrap_or(1),
            _ => 1,
        };
        match s {
            0 => tags_to_delete.push(tag),
            2 => tags_to_add.push(tag),
            _ => {}
        }
    }

    if !tags_to_delete.is_empty() {
        if let Err(error) = apply_tag_change(runtime, &ids, TagAction::Remove, tags_to_delete)
            .await
            .and_then(|result| ensure_all_ids_found(&result).map(|()| result))
        {
            return Response::from_json(&fail_response(error));
        }
    }
    if !tags_to_add.is_empty() {
        if let Err(error) = apply_tag_change(runtime, &ids, TagAction::Add, tags_to_add)
            .await
            .and_then(|result| ensure_all_ids_found(&result).map(|()| result))
        {
            return Response::from_json(&fail_response(error));
        }
    }

    Response::from_json(&json!({ "success": true }))
}

// ---------------------------------------------------------------------------
// POST /api/tag/change_color (native: src/web/misc.rs:264 tag_change_color)
// ---------------------------------------------------------------------------

/// native `tag_change_color` の写し。`tag` はタグ名 (native と同じ検証)、
/// `color` は有効色名か空文字 (空文字 = 色の解除)。応答は native
/// `ApiResponse` と同じ `{success, message}` (常に HTTP 200)。
async fn tag_change_color(
    runtime: &WorkerRuntime,
    body: serde_json::Value,
) -> worker::Result<Response> {
    let tag = match validate_web_tag_name(body["tag"].as_str().unwrap_or("")) {
        Ok(tag) => tag,
        Err(message) => {
            return Response::from_json(&ApiResponse {
                success: false,
                message,
            });
        }
    };
    let color = body["color"].as_str().unwrap_or("");
    if !color.is_empty() && !narou_rs::application::tag_colors::is_valid_tag_color(color) {
        return Response::from_json(&ApiResponse {
            success: false,
            message: format!("{}という色は存在しません", color),
        });
    }
    match runtime
        .services
        .tag_colors
        .set(&tag, (!color.is_empty()).then_some(color))
        .await
    {
        Ok(()) => Response::from_json(&ApiResponse {
            success: true,
            message: "OK".to_string(),
        }),
        Err(error) => Response::from_json(&ApiResponse {
            success: false,
            message: error.to_string(),
        }),
    }
}

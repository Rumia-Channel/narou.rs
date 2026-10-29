use axum::{
    extract::{Query, State},
    http::header,
    response::{Html, IntoResponse, Json, Response},
};
use reqwest::header::USER_AGENT;
use serde::Deserialize;
use crate::application::web_payloads::version_update_available;
use crate::application::webui::mutations::{MutationNotice, mutation_events};
use crate::application::webui::notepad::{
    notepad_conflict_payload, notepad_object_id, notepad_response_value, notepad_saved_payload,
};
use crate::application::webui::tag_order::{
    TAG_SORT_ORDER_SETTING, TagSortOrder, sort_tags,
};
use crate::application::webui::tag_views::{tag_count_ordering, tag_list_html};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::db::inventory::{Inventory, InventoryScope};
use crate::version;

use super::AppState;
use super::sort_state::{
    current_sort_from_server_setting, default_current_sort_state, normalize_current_sort_request,
};
use super::state::{ApiResponse, LogsParams};

#[derive(Debug, Deserialize)]
pub struct TagListParams {
    format: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct HistoryParams {
    stream: Option<String>,
    format: Option<String>,
    /// これより後の履歴行だけ返す (差分取得のカーソル)。
    after: Option<u64>,
}

pub async fn version_current(State(_state): State<AppState>) -> Json<serde_json::Value> {
    Json(version::version_info().current())
}

pub async fn version_latest(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let info = version::version_info();
    let repo = "Rumia-Channel/narou.rs";
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client");

    let resp = client.get(url).header(USER_AGENT, "narou.rs").send().await;

    match resp {
        Ok(resp) if resp.status().is_success() => {
            let json_text = resp.text().await.unwrap_or_default();
            let json: serde_json::Value = serde_json::from_str(&json_text).unwrap_or_default();
            let latest = json["tag_name"]
                .as_str()
                .or_else(|| json["name"].as_str())
                .map(version::version_core)
                .unwrap_or_default();
            let current_plain = version::version_core(&info.version);
            let update_available = version_update_available(&latest, &current_plain);
            Json(info.latest(
                &latest,
                update_available,
                json["html_url"]
                    .as_str()
                    .unwrap_or("https://github.com/Rumia-Channel/narou.rs/releases/latest"),
                version::self_update_unavailable_reason(),
                crate::native::self_update::variant_choice_required(),
            ))
        }
        Ok(resp) => Json(info.latest_failure(
            &format!("latest version request failed: {}", resp.status()),
            "https://github.com/Rumia-Channel/narou.rs/releases/latest",
        )),
        Err(e) => Json(info.latest_failure(
            &e.to_string(),
            "https://github.com/Rumia-Channel/narou.rs/releases/latest",
        )),
    }
}

/// P2: notepad text lives in app_state('inv','notepad'); the file under
/// `.narou/` remains only as a legacy import source / pre-DB fallback.
fn notepad_state() -> Option<crate::native::sqlite::state::StateDb> {
    #[cfg(feature = "native-runtime")]
    {
        if crate::native::sqlite::state::legacy_yaml_active() {
            return None;
        }
        let narou_dir = Inventory::with_default_root()
            .ok()
            .map(|inventory| inventory.root_dir().join(".narou"));
        narou_dir.as_deref().and_then(crate::native::sqlite::state::active_for)
    }
    #[cfg(not(feature = "native-runtime"))]
    {
        None
    }
}

fn notepad_path() -> crate::error::Result<PathBuf> {
    Ok(Inventory::with_default_root()?
        .root_dir()
        .join(".narou")
        .join("notepad.txt"))
}

fn read_notepad() -> String {
    if let Some(state) = notepad_state() {
        return state
            .get_raw("inv", "notepad")
            .ok()
            .flatten()
            .unwrap_or_default();
    }
    notepad_path()
        .ok()
        .and_then(|path| read_notepad_content(&path).ok())
        .unwrap_or_default()
}


fn read_notepad_content(path: &Path) -> std::io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
}

pub async fn webui_config(State(state): State<AppState>) -> Json<serde_json::Value> {
    // 依存の無い設定は 1 回の `get_many` にまとめる (スコープごとに 1 回だけ
    // ストアを読む)。
    const CONFIG_NAMES: &[&str] = &[
        "webui.theme",
        "webui.performance-mode",
        "webui.table.reload-timing",
        "webui.debug-mode",
        "webui.tag-sort-order",
        "concurrency",
    ];
    let loaded: Vec<Option<serde_yaml::Value>> = state
        .services
        .settings
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
        .and_then(|value| crate::setting_core::setting_bool_value(&value))
        .unwrap_or(false);
    let concurrency_enabled = find("concurrency")
        .and_then(|value| crate::setting_core::setting_bool_value(&value))
        .unwrap_or(false);
    let tag_sort_order = TagSortOrder::from_setting(Some(
        string_value(find("webui.tag-sort-order"), "favorites_first").as_str(),
    ));

    // 能力宣言 (共有定義)。端末一覧だけは実行時の外部ツール検出値を使う。
    let capabilities = crate::application::webui::capabilities::WebUiCapabilities::native(
        crate::converter::device::OutputManager::available_devices()
            .into_iter()
            .map(|(name, available)| {
                crate::application::webui::capabilities::DeviceChoice { name: name.into(), available }
            })
            .collect(),
    );

    Json(serde_json::json!({
        "theme": theme,
        "performance_mode": performance_mode,
        "reload_timing": reload_timing,
        "debug_mode": debug_mode,
        "tag_sort_order": tag_sort_order.setting_value(),
        "ws_port": state.ws_port,
        "port": state.port,
        "concurrency_enabled": concurrency_enabled,
        // native の注記 (Worker の応答と同じキーを native 値で返す):
        // `concurrency` は実行レーンの分離、keep-txt の既定 true、
        // retry の配備上限なし。
        "concurrency_scope": "lanes",
        "convert_keep_txt_default": true,
        "queue_max_retries_limit": serde_json::Value::Null,
        "capabilities": serde_json::to_value(&capabilities)
            .expect("capabilities serialization is infallible"),
    }))
}

pub async fn tag_list(
    State(state): State<AppState>,
    Query(params): Query<TagListParams>,
) -> Response {
    let new_tag_color = super::configured_tag_color(&state).await;
    let records = state.services.library.records().await.unwrap_or_default();
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for record in &records {
        for tag in &record.tags {
            if crate::virtual_novel::is_internal_tag(tag) {
                continue;
            }
            *counts.entry(tag.clone()).or_insert(0) += 1;
        }
    }
    let mut list: Vec<(String, usize)> = counts.into_iter().collect();
    // 件数降順、同数はタグ名の昇順 (Worker の SQL と同じ規則 — 新規タグへの
    // 色割り当ても並び順に依存するため、両ランタイムで一致させる)。
    list.sort_by(tag_count_ordering);
    let count_map: std::collections::HashMap<String, usize> =
        list.iter().cloned().collect();
    let default_tags: Vec<String> = list.into_iter().map(|(tag, _)| tag).collect();

    // 並び順 (お気に入り優先 / アルファベット順 / 件数順 / 登録順) と
    // お気に入りタグの並びは利用者が選べる。
    let sort_order = TagSortOrder::from_setting(
        state
            .services
            .settings
            .get(TAG_SORT_ORDER_SETTING)
            .await
            .ok()
            .flatten()
            .and_then(|value| value.as_str().map(str::to_string))
            .as_deref(),
    );
    let favorites = crate::native::favorite_tags::load()
        .map(|state| state.tags)
        .unwrap_or_default();
    // 「登録順」はタグがライブラリへ入った順 (在庫の `tag_sequence`)。
    // 記録が無いライブラリは最小小説 ID → タグ名の順。
    let sequence = crate::native::tag_sequence::load().unwrap_or_default();
    let library = crate::application::tag_sequence::library_tags(
        records
            .iter()
            .map(|record| (record.id, record.tags.as_slice())),
        |tag| !crate::virtual_novel::is_internal_tag(tag),
    );
    let registered = crate::application::tag_sequence::registered_ranks(&sequence, &library);
    let tags = sort_tags(
        sort_order,
        &default_tags,
        &count_map,
        &favorites,
        &registered,
    );
    let tag_colors = state
        .services
        .tag_colors
        .for_tags(tags.clone(), new_tag_color.as_deref())
        .await
        .unwrap_or_default();

    if params.format.as_deref() == Some("json") {
        return Json(serde_json::json!({ "tags": tags, "tag_colors": tag_colors })).into_response();
    }

    Html(tag_list_html(&tags, &tag_colors)).into_response()
}

pub async fn tag_change_color(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<ApiResponse> {
    let tag = match super::validate_web_tag_name(body["tag"].as_str().unwrap_or("")) {
        Ok(tag) => tag,
        Err(message) => {
            return Json(ApiResponse {
                success: false,
                message,
            });
        }
    };
    let color = body["color"].as_str().unwrap_or("");
    if !color.is_empty() && !crate::tag_colors::is_valid_tag_color(color) {
        return Json(ApiResponse {
            success: false,
            message: format!("{}という色は存在しません", color),
        });
    }
    match state
        .services
        .tag_colors
        .set(&tag, (!color.is_empty()).then_some(color))
        .await
    {
        Ok(()) => Json(ApiResponse {
            success: true,
            message: "OK".to_string(),
        }),
        Err(error) => Json(ApiResponse {
            success: false,
            message: error.to_string(),
        }),
    }
}

pub async fn all_novel_ids(State(state): State<AppState>) -> Json<serde_json::Value> {
    let ids = state
        .services
        .library
        .records()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|record| record.id)
        .collect::<Vec<i64>>();
    Json(serde_json::json!({ "ids": ids }))
}

pub async fn notepad_read(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let content = read_notepad();
    Json(notepad_response_value(&content))
}

pub async fn notepad_save(
    State(state): State<AppState>,
    #[allow(unused_variables)]
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let content = body["content"]
        .as_str()
        .or_else(|| body["text"].as_str())
        .unwrap_or("");
    if let Err(message) =
        super::validate_web_text_size(content, super::MAX_WEB_TEXT_INPUT_BYTES, "notepad content")
    {
        return Json(serde_json::json!({
            "success": false,
            "message": message,
        }));
    }
    let request_object_id = body["object_id"].as_str().unwrap_or("");
    let update = |current: &str| {
        if request_object_id != notepad_object_id(current) {
            return Ok((current.to_string(), notepad_conflict_payload(current)));
        }
        Ok((content.to_string(), notepad_saved_payload(content)))
    };
    let result = if let Some(db) = notepad_state() {
        db.update_raw("inv", "notepad", update)
    } else {
        notepad_path().and_then(|path| {
            crate::db::inventory::with_locked_file_update(&path, update)
        })
    };
    match result {
        Ok((_, payload)) => {
            if payload["success"] == true {
                for event in mutation_events(MutationNotice::Notepad { content }) {
                    state.push_server.broadcast_raw(event);
                }
            }
            Json(payload)
        }
        Err(e) => Json(serde_json::json!({
            "success": false,
            "message": e.to_string(),
        })),
    }
}

pub async fn recent_logs(
    State(state): State<AppState>,
    Query(params): Query<LogsParams>,
) -> Json<serde_json::Value> {
    let count = params.count.unwrap_or(100).min(super::MAX_WEB_LOG_COUNT);
    let logs = state.push_server.recent_logs(count);
    Json(serde_json::json!({ "logs": logs }))
}

pub async fn console_history(
    State(state): State<AppState>,
    Query(params): Query<HistoryParams>,
) -> Response {
    // `after` があるときは差分だけ返す (WebSocket が使えない環境の補完用)。
    if let Some(after) = params.after {
        let (history, cursor) = state
            .push_server
            .history_since(params.stream.as_deref(), after);
        return Json(serde_json::json!({ "history": history, "id": cursor })).into_response();
    }
    if params.format.as_deref() == Some("json") {
        // 履歴とカーソルを同じロック内で取得する。別々に取得すると、その隙に
        // 発行された行が「履歴には無いのにカーソルには含まれる」ことになり、
        // 以降の差分取得でその行が飛ばされる。
        let (history, cursor) = state
            .push_server
            .history_since(params.stream.as_deref(), 0);
        return Json(serde_json::json!({ "history": history, "id": cursor })).into_response();
    }
    let history = state.push_server.get_history_for(params.stream.as_deref());
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        history,
    )
        .into_response()
}

pub async fn clear_history(State(state): State<AppState>) -> Json<ApiResponse> {
    state.push_server.clear_history();
    Json(ApiResponse {
        success: true,
        message: "History cleared".to_string(),
    })
}

pub async fn get_sort_state(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let sort_state = (|| -> Option<serde_json::Value> {
        let inv = Inventory::with_default_root().ok()?;
        let server_setting: serde_yaml::Value =
            inv.load("server_setting", InventoryScope::Global).ok()?;
        current_sort_from_server_setting(&server_setting).map(|state| state.to_json_value())
    })();

    match sort_state {
        Some(state) => Json(state),
        None => Json(default_current_sort_state().to_json_value()),
    }
}

pub async fn save_sort_state(
    State(_state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<ApiResponse> {
    let Some(sort_state) = normalize_current_sort_request(&body) else {
        return Json(ApiResponse {
            success: false,
            message: "valid column and dir are required".to_string(),
        });
    };

    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let inv = Inventory::with_default_root()?;
        let mut server_setting = match inv.load("server_setting", InventoryScope::Global) {
            Ok(serde_yaml::Value::Mapping(mapping)) => mapping,
            _ => serde_yaml::Mapping::new(),
        };
        server_setting.insert(
            serde_yaml::Value::String("current_sort".to_string()),
            sort_state.to_yaml_value(),
        );
        inv.save(
            "server_setting",
            InventoryScope::Global,
            &serde_yaml::Value::Mapping(server_setting),
        )?;
        Ok(())
    })();

    match result {
        Ok(()) => Json(ApiResponse {
            success: true,
            message: "OK".to_string(),
        }),
        Err(e) => Json(ApiResponse {
            success: false,
            message: e.to_string(),
        }),
    }
}

pub async fn validate_url_regexp_list(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!(
        state
            .services
            .site_definitions
            .url_patterns_for_validation()
    ))
}

#[cfg(test)]
mod tests {
    use super::notepad_path;

    #[test]
    fn notepad_path_uses_narou_root_instead_of_current_dir() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let nested = root.join("subdir").join("inner");
        std::fs::create_dir_all(root.join(".narou")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();

        let _guard = crate::test_support::set_current_dir_for_test(&nested);

        assert_eq!(notepad_path().unwrap(), root.join(".narou").join("notepad.txt"));
    }
}

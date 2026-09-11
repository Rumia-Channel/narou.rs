use axum::{
    extract::{Query, State},
    http::header,
    response::{Html, IntoResponse, Json, Response},
};
use reqwest::header::USER_AGENT;
use serde::Deserialize;
use sha2::{Digest, Sha256};
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
}

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

pub async fn version_current(State(_state): State<AppState>) -> Json<serde_json::Value> {
    Json(version::version_json())
}

pub async fn version_latest(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let current = version::create_version_string();
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
                .map(version_core)
                .unwrap_or_default();
            let current_plain = version_core(&current);
            let develop = !version::commit_version_exists();
            let local_build = version::is_local_build();
            let container = version::is_container_runtime();
            let self_update_supported = version::self_update_unavailable_reason().is_none();
            Json(serde_json::json!({
                "success": true,
                "current_version": current,
                "latest_version": latest,
                "update_available": version_is_newer(&latest, &current_plain),
                "develop": develop,
                "local_build": local_build,
                "container": container,
                "self_update_supported": self_update_supported,
                "self_update_unavailable_reason": version::self_update_unavailable_reason(),
                "url": json["html_url"].as_str().unwrap_or("https://github.com/Rumia-Channel/narou.rs/releases/latest"),
            }))
        }
        Ok(resp) => Json(serde_json::json!({
            "success": false,
            "current_version": current,
            "message": format!("latest version request failed: {}", resp.status()),
            "url": "https://github.com/Rumia-Channel/narou.rs/releases/latest",
        })),
        Err(e) => Json(serde_json::json!({
            "success": false,
            "current_version": current,
            "message": e.to_string(),
            "url": "https://github.com/Rumia-Channel/narou.rs/releases/latest",
        })),
    }
}

/// Extract the numeric `x.y.z` core from a version string, ignoring `v`
/// prefixes, suffixes like `(develop)`/`(local-build)`, and any invisible
/// characters that may slip into release metadata.
fn version_core(version: &str) -> String {
    let mut core = String::new();
    for ch in version.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            core.push(ch);
        } else if !core.is_empty() {
            break;
        }
    }
    core.trim_end_matches('.').to_string()
}

/// Numeric semver-style comparison: `latest` is an update only when it is
/// strictly newer than `current`. Falls back to inequality when either side
/// has no parseable version core.
fn version_is_newer(latest: &str, current: &str) -> bool {
    if latest.is_empty() {
        return false;
    }
    let parse = |v: &str| -> Option<Vec<u64>> {
        let parts: Option<Vec<u64>> = v.split('.').map(|p| p.parse().ok()).collect();
        parts.filter(|p| !p.is_empty())
    };
    match (parse(latest), parse(current)) {
        (Some(l), Some(c)) => {
            let len = l.len().max(c.len());
            for i in 0..len {
                let lv = l.get(i).copied().unwrap_or(0);
                let cv = c.get(i).copied().unwrap_or(0);
                if lv != cv {
                    return lv > cv;
                }
            }
            false
        }
        _ => latest != current,
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

fn write_notepad(content: &str) -> std::io::Result<()> {
    if let Some(state) = notepad_state() {
        return state
            .set_raw("inv", "notepad", content)
            .map_err(|error| std::io::Error::other(error.to_string()));
    }
    let path = notepad_path().map_err(|error| std::io::Error::other(error.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)
}

fn read_notepad_content(path: &Path) -> std::io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
}

pub async fn webui_config(State(state): State<AppState>) -> Json<serde_json::Value> {
    let setting = |name: &'static str| async {
        state.services.settings.get(name).await.ok().flatten()
    };
    let string_value = |value: Option<serde_yaml::Value>, default: &str| {
        value
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| default.to_string())
    };
    let theme = string_value(setting("webui.theme").await, "Cerulean");
    let performance_mode = string_value(setting("webui.performance-mode").await, "auto");
    let reload_timing = string_value(setting("webui.table.reload-timing").await, "every");
    let debug_mode = setting("webui.debug-mode")
        .await
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let concurrency_enabled = setting("concurrency")
        .await
        .and_then(|value| value.as_bool())
        .unwrap_or(false);

    Json(serde_json::json!({
        "theme": theme,
        "performance_mode": performance_mode,
        "reload_timing": reload_timing,
        "debug_mode": debug_mode,
        "ws_port": state.ws_port,
        "port": state.port,
        "concurrency_enabled": concurrency_enabled,
    }))
}

pub async fn tag_list(
    State(state): State<AppState>,
    Query(params): Query<TagListParams>,
) -> Response {
    let new_tag_color = super::configured_tag_color(&state).await;
    let records = match state.services.library.records().await {
        Ok(records) => records,
        Err(_) => Vec::new(),
    };
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for record in &records {
        for tag in &record.tags {
            *counts.entry(tag.clone()).or_insert(0) += 1;
        }
    }
    let mut list: Vec<(String, usize)> = counts.into_iter().collect();
    list.sort_by(|a, b| b.1.cmp(&a.1));
    let tags = list.into_iter().map(|(tag, _)| tag).collect::<Vec<_>>();
    let tag_colors = state
        .services
        .tag_colors
        .for_tags(tags.clone(), new_tag_color.as_deref())
        .await
        .unwrap_or_default();

    if params.format.as_deref() == Some("json") {
        return Json(serde_json::json!({ "tags": tags, "tag_colors": tag_colors })).into_response();
    }

    let mut html = String::from(
        "<div><span class=\"tag-label tag-default tag-reset\" data-tag=\"\">タグ検索を解除</span></div>\
<div class=\"text-muted\" style=\"font-size:0.8em\">Altキーを押しながらで除外検索</div>",
    );
    for tag in &tags {
        let escaped_tag = html_escape(tag);
        let class = tag_color_class(tag_colors.get(tag).map(String::as_str).unwrap_or("default"));
        html.push_str(&format!(
            "<div><span class=\"tag-label {}\" data-tag=\"{}\">{}</span> \
<span class=\"select-color-button\" data-target-tag=\"{}\"><span class=\"tag-label {} tag-fixed-width\">a</span></span></div>",
            class, escaped_tag, escaped_tag, escaped_tag, class
        ));
    }
    Html(html).into_response()
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
    let state_db = notepad_state();
    if state_db.is_none() {
        // Legacy file flow (pre-DB libraries / legacy escape hatch).
        let path = match notepad_path() {
            Ok(path) => path,
            Err(e) => {
                return Json(serde_json::json!({
                    "success": false,
                    "message": e.to_string(),
                }));
            }
        };
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return Json(serde_json::json!({
                "success": false,
                "message": e.to_string(),
            }));
        }
    }
    let current_content = read_notepad();
    let current_object_id = notepad_object_id(&current_content);
    let request_object_id = body["object_id"].as_str().unwrap_or("");

    if request_object_id != current_object_id {
        return Json(serde_json::json!({
            "success": false,
            "conflict": true,
            "message": "他の画面でメモ帳が更新されたため再読み込みしました。内容を確認してからもう一度保存してください",
            "content": current_content,
            "text": current_content,
            "object_id": current_object_id,
        }));
    }

    let result = if let Some(ref db) = state_db {
        db.set_raw("inv", "notepad", content)
            .map_err(|error| std::io::Error::other(error.to_string()))
    } else {
        write_notepad(content)
    };
    let object_id = notepad_object_id(content);
    let response = serde_json::json!({
        "content": content,
        "text": content,
        "object_id": object_id,
    });

    match result {
        Ok(_) => {
            state.push_server.broadcast_raw(&serde_json::json!({
                "type": "notepad.change",
                "data": response.clone(),
            }));
            Json(serde_json::json!({
                "success": true,
                "message": "Saved",
                "content": content,
                "text": content,
                "object_id": response["object_id"].clone(),
            }))
        }
        Err(e) => Json(serde_json::json!({
            "success": false,
            "message": e.to_string(),
        })),
    }
}

fn notepad_object_id(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn notepad_response_value(content: &str) -> serde_json::Value {
    serde_json::json!({
        "content": content,
        "text": content,
        "object_id": notepad_object_id(content),
    })
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
    let history = state.push_server.get_history_for(params.stream.as_deref());
    if params.format.as_deref() == Some("json") {
        return Json(serde_json::json!({ "history": history })).into_response();
    }
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

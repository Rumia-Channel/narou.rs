use axum::{
    extract::{Query, State},
    http::header,
    response::{Html, IntoResponse, Json, Response},
};
use reqwest::header::USER_AGENT;
use serde::Deserialize;
use std::time::Duration;

use crate::compat::{load_local_setting_bool, load_local_setting_string};
use crate::db::inventory::{Inventory, InventoryScope};
use crate::db::with_database;
use crate::version;

use super::AppState;
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

    let resp = client
        .get(url)
        .header(USER_AGENT, "narou.rs")
        .send()
        .await;

    match resp {
        Ok(resp) if resp.status().is_success() => {
            let json_text = resp.text().await.unwrap_or_default();
            let json: serde_json::Value = serde_json::from_str(&json_text).unwrap_or_default();
            let latest = json["tag_name"]
                .as_str()
                .or_else(|| json["name"].as_str())
                .unwrap_or("")
                .trim()
                .trim_start_matches('v')
                .to_string();
            let current_plain = normalize_version(&current);
            Json(serde_json::json!({
                "success": true,
                "current_version": current,
                "latest_version": latest,
                "update_available": !latest.is_empty() && latest != current_plain,
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

fn normalize_version(version: &str) -> String {
    version
        .trim()
        .trim_start_matches('v')
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string()
}

pub async fn webui_config(State(state): State<AppState>) -> Json<serde_json::Value> {
    let theme = load_local_setting_string("webui.theme").unwrap_or_else(|| "Cerulean".to_string());
    let performance_mode =
        load_local_setting_string("webui.performance-mode").unwrap_or_else(|| "auto".to_string());
    let reload_timing = load_local_setting_string("webui.table.reload-timing")
        .unwrap_or_else(|| "every".to_string());

    let concurrency_enabled = load_local_setting_bool("concurrency");

    Json(serde_json::json!({
        "theme": theme,
        "performance_mode": performance_mode,
        "reload_timing": reload_timing,
        "ws_port": state.ws_port,
        "port": state.port,
        "concurrency_enabled": concurrency_enabled,
    }))
}

pub async fn tag_list(
    State(_state): State<AppState>,
    Query(params): Query<TagListParams>,
) -> Response {
    let (tags, tag_colors) = with_database(|db| {
        let index = db.tag_index();
        let mut list: Vec<(&String, &Vec<i64>)> = index.iter().collect();
        list.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
        let tags = list.into_iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();

        let inventory = db.inventory();
        let mut tag_colors = super::tag_colors::load_tag_colors(inventory)?;
        if super::tag_colors::ensure_tag_colors(&mut tag_colors, tags.iter().map(String::as_str)) {
            super::tag_colors::save_tag_colors(inventory, &tag_colors)?;
        }

        Ok((tags, tag_colors.into_map()))
    })
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
        let class = tag_color_class(tag_colors.get(tag).map(|value| value.as_str()).unwrap_or("default"));
        html.push_str(&format!(
            "<div><span class=\"tag-label {}\" data-tag=\"{}\">{}</span> \
<span class=\"select-color-button\" data-target-tag=\"{}\"><span class=\"tag-label {} tag-fixed-width\">a</span></span></div>",
            class, escaped_tag, escaped_tag, escaped_tag, class
        ));
    }
    Html(html).into_response()
}

pub async fn tag_change_color(
    State(_state): State<AppState>,
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

    if !color.is_empty() && !super::tag_colors::is_valid_tag_color(color) {
        return Json(ApiResponse {
            success: false,
            message: format!("{}という色は存在しません", color),
        });
    }

    let result = with_database(|db| {
        let inv = db.inventory();
        let mut colors = super::tag_colors::load_tag_colors(inv)?;
        if color.is_empty() {
            colors.remove(&tag);
        } else {
            colors.set(&tag, color);
        }
        super::tag_colors::save_tag_colors(inv, &colors)?;
        Ok(())
    });

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

pub async fn all_novel_ids(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let ids = with_database(|db| {
        let ids: Vec<i64> = db.all_records().keys().copied().collect();
        Ok(ids)
    })
    .unwrap_or_default();
    Json(serde_json::json!({ "ids": ids }))
}

pub async fn notepad_read(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let content = std::fs::read_to_string(".narou/notepad.txt").unwrap_or_default();
    Json(serde_json::json!({ "content": content, "text": content }))
}

pub async fn notepad_save(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<ApiResponse> {
    let content = body["content"]
        .as_str()
        .or_else(|| body["text"].as_str())
        .unwrap_or("");
    if let Err(message) =
        super::validate_web_text_size(content, super::MAX_WEB_TEXT_INPUT_BYTES, "notepad content")
    {
        return Json(ApiResponse {
            success: false,
            message,
        });
    }
    let result = std::fs::write(".narou/notepad.txt", content);

    match result {
        Ok(_) => {
            state.push_server.broadcast_event("notepad.change", content);
            Json(ApiResponse {
                success: true,
                message: "Saved".to_string(),
            })
        }
        Err(e) => Json(ApiResponse {
            success: false,
            message: e.to_string(),
        }),
    }
}

pub async fn recent_logs(
    State(state): State<AppState>,
    Query(params): Query<LogsParams>,
) -> Json<serde_json::Value> {
    let count = params
        .count
        .unwrap_or(100)
        .min(super::MAX_WEB_LOG_COUNT);
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
        let server_setting: serde_json::Value =
            inv.load("server_setting", InventoryScope::Global).ok()?;
        server_setting.get("current_sort").cloned()
    })();

    match sort_state {
        Some(state) => Json(state),
        None => Json(serde_json::json!({"column": 2, "dir": "desc"})),
    }
}

pub async fn save_sort_state(
    State(_state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<ApiResponse> {
    let column = body.get("column");
    let dir = body.get("dir");

    if column.is_none() || dir.is_none() {
        return Json(ApiResponse {
            success: false,
            message: "column and dir are required".to_string(),
        });
    }

    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let inv = Inventory::with_default_root()?;
        let mut server_setting: serde_json::Map<String, serde_json::Value> = inv
            .load("server_setting", InventoryScope::Global)
            .unwrap_or_default();
        server_setting.insert(
            "current_sort".to_string(),
            serde_json::json!({
                "column": column.unwrap(),
                "dir": dir.unwrap(),
            }),
        );
        inv.save(
            "server_setting",
            InventoryScope::Global,
            &serde_json::Value::Object(server_setting),
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

pub async fn validate_url_regexp_list(
    State(_state): State<AppState>,
) -> Json<serde_json::Value> {
    use crate::downloader::site_setting::SiteSetting;

    let patterns: Vec<String> = SiteSetting::load_all()
        .unwrap_or_default()
        .iter()
        .flat_map(|s| s.url_patterns_for_validation())
        .collect();

    Json(serde_json::json!(patterns))
}

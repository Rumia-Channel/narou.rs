use axum::{
    extract::{Query, State},
    response::Json,
};

use crate::compat::load_local_setting_string;
use crate::db::inventory::InventoryScope;
use crate::db::with_database;
use crate::version;

use super::AppState;
use super::state::{ApiResponse, LogsParams};

pub async fn version_current(State(_state): State<AppState>) -> Json<serde_json::Value> {
    Json(version::version_json())
}

pub async fn webui_config(State(state): State<AppState>) -> Json<serde_json::Value> {
    let theme = load_local_setting_string("webui.theme").unwrap_or_else(|| "Cerulean".to_string());
    let performance_mode =
        load_local_setting_string("webui.performance-mode").unwrap_or_else(|| "auto".to_string());
    let reload_timing = load_local_setting_string("webui.table.reload-timing")
        .unwrap_or_else(|| "every".to_string());

    Json(serde_json::json!({
        "theme": theme,
        "performance_mode": performance_mode,
        "reload_timing": reload_timing,
        "ws_port": state.ws_port,
        "port": state.port,
    }))
}

pub async fn tag_list(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let tags = with_database(|db| {
        let index = db.tag_index();
        let mut list: Vec<(&String, &Vec<i64>)> = index.iter().collect();
        list.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
        Ok(list.into_iter().map(|(k, _)| k.clone()).collect::<Vec<_>>())
    })
    .unwrap_or_default();

    // Load tag colors from inventory
    let tag_colors = with_database(|db| {
        let inv = db.inventory();
        let colors: std::collections::HashMap<String, String> =
            inv.load("tag_colors", InventoryScope::Local).unwrap_or_default();
        Ok(colors)
    })
    .unwrap_or_default();

    Json(serde_json::json!({ "tags": tags, "tag_colors": tag_colors }))
}

pub async fn tag_change_color(
    State(_state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<ApiResponse> {
    let tag = body["tag"].as_str().unwrap_or("");
    let color = body["color"].as_str().unwrap_or("");

    if tag.is_empty() {
        return Json(ApiResponse {
            success: false,
            message: "tag is required".to_string(),
        });
    }

    let result = with_database(|db| {
        let inv = db.inventory();
        let mut colors: std::collections::HashMap<String, String> =
            inv.load("tag_colors", InventoryScope::Local).unwrap_or_default();
        if color.is_empty() {
            colors.remove(tag);
        } else {
            colors.insert(tag.to_string(), color.to_string());
        }
        inv.save("tag_colors", InventoryScope::Local, &colors)?;
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
    Json(serde_json::json!({ "content": content }))
}

pub async fn notepad_save(
    State(_state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<ApiResponse> {
    let content = body["content"].as_str().unwrap_or("");
    let result = std::fs::write(".narou/notepad.txt", content);

    match result {
        Ok(_) => Json(ApiResponse {
            success: true,
            message: "Saved".to_string(),
        }),
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
    let count = params.count.unwrap_or(100);
    let logs = state.push_server.recent_logs(count);
    Json(serde_json::json!({ "logs": logs }))
}

pub async fn console_history(State(state): State<AppState>) -> Json<serde_json::Value> {
    let history = state.push_server.get_history();
    Json(serde_json::json!({ "history": history }))
}

pub async fn clear_history(State(state): State<AppState>) -> Json<ApiResponse> {
    state.push_server.clear_history();
    Json(ApiResponse {
        success: true,
        message: "History cleared".to_string(),
    })
}

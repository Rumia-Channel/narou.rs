use axum::{
    extract::{Query, State},
    response::Json,
};

use crate::db::with_database;

use super::AppState;
use super::state::{ApiResponse, LogsParams};

pub async fn version_current(State(_state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "name": "narou.rs"
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

    Json(serde_json::json!({ "tags": tags }))
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
    let logger = super::push::StreamingLogger::new(state.push_server.clone());
    let logs = logger.recent_logs(count);
    Json(serde_json::json!({ "logs": logs }))
}

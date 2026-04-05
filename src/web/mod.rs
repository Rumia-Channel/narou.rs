use axum::{
    extract::Query,
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::db::with_database;
use crate::error::NarouError;

#[derive(Debug, Clone)]
pub struct AppState {
    pub port: u16,
}

#[derive(Debug, Serialize)]
pub struct NovelListResponse {
    pub draw: u64,
    pub records_total: u64,
    pub records_filtered: u64,
    pub data: Vec<NovelListItem>,
}

#[derive(Debug, Serialize)]
pub struct NovelListItem {
    pub id: i64,
    pub title: String,
    pub author: String,
    pub sitename: String,
    pub novel_type: u8,
    pub end: bool,
    pub last_update: String,
    pub general_lastup: Option<String>,
    pub tags: Vec<String>,
    pub new_arrivals: bool,
    pub frozen: bool,
    pub length: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub draw: Option<u64>,
    pub start: Option<u64>,
    pub length: Option<u64>,
    #[serde(rename = "search[value]")]
    pub search_value: Option<String>,
    #[serde(rename = "order[0][column]")]
    pub order_column: Option<u64>,
    #[serde(rename = "order[0][dir]")]
    pub order_dir: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ApiResponse {
    pub success: bool,
    pub message: String,
}

pub fn create_router() -> Router {
    let state = Arc::new(AppState { port: 3000 });

    Router::new()
        .route("/", get(index))
        .route("/api/novels/count", get(novels_count))
        .route("/api/list", get(api_list))
        .route("/api/version/current.json", get(version_current))
        .route("/api/tag_list", get(tag_list))
        .route("/api/notepad/read", get(notepad_read))
        .route("/api/notepad/save", post(notepad_save))
        .with_state(state)
}

async fn index() -> &'static str {
    "narou.rs API server"
}

async fn novels_count() -> Json<serde_json::Value> {
    let count = with_database(|db| Ok(db.all_records().len() as u64)).unwrap_or(0);
    Json(serde_json::json!({ "count": count }))
}

async fn api_list(
    Query(params): Query<ListParams>,
) -> Result<Json<NovelListResponse>, (StatusCode, String)> {
    let draw = params.draw.unwrap_or(1);
    let start = params.start.unwrap_or(0);
    let length = params.length.unwrap_or(50);
    let search = params.search_value.unwrap_or_default();
    let order_col = params.order_column.unwrap_or(0);
    let order_dir = params.order_dir.unwrap_or_else(|| "asc".to_string());

    let response = with_database(|db| {
        let all_records: Vec<_> = db.all_records().values().collect();

        let mut filtered: Vec<_> = if search.is_empty() {
            all_records
        } else {
            let search_lower = search.to_lowercase();
            all_records
                .into_iter()
                .filter(|r| {
                    r.title.to_lowercase().contains(&search_lower)
                        || r.author.to_lowercase().contains(&search_lower)
                        || r.tags.iter().any(|t| t.to_lowercase().contains(&search_lower))
                })
                .collect()
        };

        let sort_key = match order_col {
            0 => "id",
            1 => "last_update",
            2 => "general_lastup",
            3 => "title",
            4 => "author",
            5 => "sitename",
            _ => "id",
        };

        let reverse = order_dir == "desc";
        filtered.sort_by(|a, b| {
            let va = match sort_key {
                "id" => a.id.cmp(&b.id),
                "title" => a.title.to_lowercase().cmp(&b.title.to_lowercase()),
                "author" => a.author.to_lowercase().cmp(&b.author.to_lowercase()),
                "last_update" => a.last_update.cmp(&b.last_update),
                "general_lastup" => {
                    a.general_lastup.unwrap_or_default().cmp(&b.general_lastup.unwrap_or_default())
                }
                "sitename" => a.sitename.cmp(&b.sitename),
                _ => std::cmp::Ordering::Equal,
            };
            if reverse { va.reverse() } else { va }
        });

        let records_total = db.all_records().len() as u64;
        let records_filtered = filtered.len() as u64;

        let data: Vec<NovelListItem> = filtered
            .into_iter()
            .skip(start as usize)
            .take(length as usize)
            .map(|r| NovelListItem {
                id: r.id,
                title: r.title.clone(),
                author: r.author.clone(),
                sitename: r.sitename.clone(),
                novel_type: r.novel_type,
                end: r.end,
                last_update: r.last_update.format("%Y-%m-%d %H:%M").to_string(),
                general_lastup: r
                    .general_lastup
                    .map(|d: chrono::DateTime<chrono::Utc>| d.format("%Y-%m-%d").to_string()),
                tags: r.tags.clone(),
                new_arrivals: false,
                frozen: r.tags.contains(&"frozen".to_string()),
                length: r.length,
            })
            .collect();

        Ok(NovelListResponse {
            draw,
            records_total,
            records_filtered,
            data,
        })
    })
    .map_err(|e: NarouError| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(response))
}

async fn version_current() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "name": "narou.rs"
    }))
}

async fn tag_list() -> Json<serde_json::Value> {
    let tags = with_database(|db| {
        let index = db.tag_index();
        let mut list: Vec<(&String, &Vec<i64>)> = index.iter().collect();
        list.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
        Ok(list.into_iter().map(|(k, _)| k.clone()).collect::<Vec<_>>())
    })
    .unwrap_or_default();

    Json(serde_json::json!({ "tags": tags }))
}

async fn notepad_read() -> Json<serde_json::Value> {
    let content = std::fs::read_to_string(".narou/notepad.txt").unwrap_or_default();
    Json(serde_json::json!({ "content": content }))
}

async fn notepad_save(
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

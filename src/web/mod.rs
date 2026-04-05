pub mod push;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
    routing::{delete, get, post, put},
    Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tower_http::cors::CorsLayer;

use crate::db::with_database;
use crate::db::with_database_mut;
use crate::error::NarouError;

#[derive(Debug, Clone)]
pub struct AppState {
    pub port: u16,
    pub push_server: Arc<push::PushServer>,
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
    let push_server = Arc::new(push::PushServer::new());

    Router::new()
        .route("/", get(index))
        .route("/api/novels/count", get(novels_count))
        .route("/api/list", get(api_list))
        .route("/api/version/current.json", get(version_current))
        .route("/api/tag_list", get(tag_list))
        .route("/api/notepad/read", get(notepad_read))
        .route("/api/notepad/save", post(notepad_save))
        .route("/api/novels/{id}", get(get_novel))
        .route("/api/novels/{id}", delete(remove_novel))
        .route("/api/novels/{id}/freeze", post(freeze_novel))
        .route("/api/novels/{id}/unfreeze", post(unfreeze_novel))
        .route("/api/novels/{id}/tag", post(add_tag))
        .route("/api/novels/{id}/tag", delete(remove_tag))
        .route("/api/novels/{id}/tags", put(update_tags))
        .route("/api/novels/tag", post(batch_tag))
        .route("/api/novels/tag", delete(batch_untag))
        .route("/api/novels/freeze", post(batch_freeze))
        .route("/api/novels/unfreeze", post(batch_unfreeze))
        .route("/api/novels/remove", post(batch_remove))
        .route("/api/download", post(api_download))
        .route("/api/update", post(api_update))
        .route("/api/convert", post(api_convert))
        .route("/api/settings/{id}", get(get_settings))
        .route("/api/settings/{id}", post(save_settings))
        .route("/api/devices", get(list_devices))
        .route("/api/queue/status", get(queue_status))
        .route("/api/queue/clear", post(queue_clear))
        .route("/api/log/recent", get(recent_logs))
        .route("/ws", get(push::ws_handler_with_app_state))
        .layer(CorsLayer::permissive())
        .with_state(AppState { port: 3000, push_server })
}

async fn index() -> &'static str {
    "narou.rs API server"
}

async fn novels_count(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let count = with_database(|db| Ok(db.all_records().len() as u64)).unwrap_or(0);
    Json(serde_json::json!({ "count": count }))
}

async fn api_list(
    Query(params): Query<ListParams>,
    State(_state): State<AppState>,
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

async fn version_current(State(_state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "name": "narou.rs"
    }))
}

async fn tag_list(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let tags = with_database(|db| {
        let index = db.tag_index();
        let mut list: Vec<(&String, &Vec<i64>)> = index.iter().collect();
        list.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
        Ok(list.into_iter().map(|(k, _)| k.clone()).collect::<Vec<_>>())
    })
    .unwrap_or_default();

    Json(serde_json::json!({ "tags": tags }))
}

async fn notepad_read(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let content = std::fs::read_to_string(".narou/notepad.txt").unwrap_or_default();
    Json(serde_json::json!({ "content": content }))
}

async fn notepad_save(
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

#[derive(Debug, Deserialize)]
struct IdPath {
    id: i64,
}

async fn get_novel(
    State(_state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let record = with_database(|db| {
        db.get(id).cloned().ok_or_else(|| NarouError::NotFound(format!("ID: {}", id)))
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    let value = serde_json::to_value(&record).unwrap_or_default();
    Ok(Json(value))
}

async fn remove_novel(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let result = with_database_mut(|db| {
        if let Some(record) = db.remove(id) {
            let novel_dir = db.archive_root().join(&record.sitename);
            if record.use_subdirectory {
                if let Some(ref ncode) = record.ncode {
                    if ncode.len() >= 2 {
                        let dir = novel_dir.join(&ncode[..2]).join(&record.file_title);
                        let _ = std::fs::remove_dir_all(&dir);
                    }
                }
            } else {
                let dir = novel_dir.join(&record.file_title);
                let _ = std::fs::remove_dir_all(&dir);
            }
            db.save()?;
            Ok::<String, NarouError>(record.title)
        } else {
            Err(NarouError::NotFound(format!("ID: {}", id)))
        }
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    state.push_server.broadcast("remove", &result);
    Ok(Json(ApiResponse { success: true, message: result }))
}

async fn freeze_novel(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    with_database_mut(|db| {
        let record = db.get(id).cloned().ok_or_else(|| NarouError::NotFound(format!("ID: {}", id)))?;
        let mut updated = record;
        if !updated.tags.contains(&"frozen".to_string()) {
            updated.tags.push("frozen".to_string());
        }
        db.insert(updated);
        db.save()
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    state.push_server.broadcast("freeze", &id.to_string());
    Ok(Json(ApiResponse { success: true, message: format!("Froze {}", id) }))
}

async fn unfreeze_novel(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    with_database_mut(|db| {
        let record = db.get(id).cloned().ok_or_else(|| NarouError::NotFound(format!("ID: {}", id)))?;
        let mut updated = record;
        updated.tags.retain(|t| t != "frozen");
        db.insert(updated);
        db.save()
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    state.push_server.broadcast("unfreeze", &id.to_string());
    Ok(Json(ApiResponse { success: true, message: format!("Unfroze {}", id) }))
}

#[derive(Debug, Deserialize)]
struct TagBody {
    tag: String,
}

async fn add_tag(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    Json(body): Json<TagBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    with_database_mut(|db| {
        let record = db.get(id).cloned().ok_or_else(|| NarouError::NotFound(format!("ID: {}", id)))?;
        let mut updated = record;
        if !updated.tags.contains(&body.tag) {
            updated.tags.push(body.tag.clone());
        }
        db.insert(updated);
        db.save()
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    state.push_server.broadcast("tag_add", &format!("{} {}", id, body.tag));
    Ok(Json(ApiResponse { success: true, message: "Tag added".to_string() }))
}

async fn remove_tag(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    Json(body): Json<TagBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    with_database_mut(|db| {
        let record = db.get(id).cloned().ok_or_else(|| NarouError::NotFound(format!("ID: {}", id)))?;
        let mut updated = record;
        updated.tags.retain(|t| t != &body.tag);
        db.insert(updated);
        db.save()
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    state.push_server.broadcast("tag_remove", &format!("{} {}", id, body.tag));
    Ok(Json(ApiResponse { success: true, message: "Tag removed".to_string() }))
}

#[derive(Debug, Deserialize)]
struct TagsBody {
    tags: Vec<String>,
}

async fn update_tags(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    Json(body): Json<TagsBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    with_database_mut(|db| {
        let record = db.get(id).cloned().ok_or_else(|| NarouError::NotFound(format!("ID: {}", id)))?;
        let mut updated = record;
        updated.tags = body.tags;
        db.insert(updated);
        db.save()
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    state.push_server.broadcast("tags_update", &id.to_string());
    Ok(Json(ApiResponse { success: true, message: "Tags updated".to_string() }))
}

#[derive(Debug, Deserialize)]
struct BatchIdsBody {
    ids: Vec<i64>,
}

async fn batch_tag(
    State(state): State<AppState>,
    Json(body): Json<(BatchIdsBody, TagBody)>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let (ids_body, tag_body) = body;
    let count = with_database_mut(|db| {
        let mut count = 0usize;
        for id in &ids_body.ids {
            if let Some(record) = db.get(*id).cloned() {
                let mut updated = record;
                if !updated.tags.contains(&tag_body.tag) {
                    updated.tags.push(tag_body.tag.clone());
                }
                db.insert(updated);
                count += 1;
            }
        }
        db.save()?;
        Ok::<usize, NarouError>(count)
    })
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    state.push_server.broadcast("batch_tag", &format!("{} items tagged with {}", count, tag_body.tag));
    Ok(Json(ApiResponse { success: true, message: format!("Tagged {} novels", count) }))
}

async fn batch_untag(
    State(state): State<AppState>,
    Json(body): Json<(BatchIdsBody, TagBody)>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let (ids_body, tag_body) = body;
    let count = with_database_mut(|db| {
        let mut count = 0usize;
        for id in &ids_body.ids {
            if let Some(record) = db.get(*id).cloned() {
                let mut updated = record;
                updated.tags.retain(|t| t != &tag_body.tag);
                db.insert(updated);
                count += 1;
            }
        }
        db.save()?;
        Ok::<usize, NarouError>(count)
    })
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    state.push_server.broadcast("batch_untag", &count.to_string());
    Ok(Json(ApiResponse { success: true, message: format!("Untagged {} novels", count) }))
}

async fn batch_freeze(
    State(state): State<AppState>,
    Json(body): Json<BatchIdsBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let count = with_database_mut(|db| {
        let mut count = 0usize;
        for id in &body.ids {
            if let Some(record) = db.get(*id).cloned() {
                let mut updated = record;
                if !updated.tags.contains(&"frozen".to_string()) {
                    updated.tags.push("frozen".to_string());
                }
                db.insert(updated);
                count += 1;
            }
        }
        db.save()?;
        Ok::<usize, NarouError>(count)
    })
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    state.push_server.broadcast("batch_freeze", &count.to_string());
    Ok(Json(ApiResponse { success: true, message: format!("Froze {} novels", count) }))
}

async fn batch_unfreeze(
    State(state): State<AppState>,
    Json(body): Json<BatchIdsBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let count = with_database_mut(|db| {
        let mut count = 0usize;
        for id in &body.ids {
            if let Some(record) = db.get(*id).cloned() {
                let mut updated = record;
                updated.tags.retain(|t| t != "frozen");
                db.insert(updated);
                count += 1;
            }
        }
        db.save()?;
        Ok::<usize, NarouError>(count)
    })
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    state.push_server.broadcast("batch_unfreeze", &count.to_string());
    Ok(Json(ApiResponse { success: true, message: format!("Unfroze {} novels", count) }))
}

async fn batch_remove(
    State(state): State<AppState>,
    Json(body): Json<BatchIdsBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let count = with_database_mut(|db| {
        let mut count = 0usize;
        for id in &body.ids {
            if let Some(record) = db.remove(*id) {
                let novel_dir = db.archive_root().join(&record.sitename);
                if record.use_subdirectory {
                    if let Some(ref ncode) = record.ncode {
                        if ncode.len() >= 2 {
                            let dir = novel_dir.join(&ncode[..2]).join(&record.file_title);
                            let _ = std::fs::remove_dir_all(&dir);
                        }
                    }
                } else {
                    let dir = novel_dir.join(&record.file_title);
                    let _ = std::fs::remove_dir_all(&dir);
                }
                count += 1;
            }
        }
        db.save()?;
        Ok::<usize, NarouError>(count)
    })
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    state.push_server.broadcast("batch_remove", &count.to_string());
    Ok(Json(ApiResponse { success: true, message: format!("Removed {} novels", count) }))
}

#[derive(Debug, Deserialize)]
struct DownloadBody {
    targets: Vec<String>,
}

async fn api_download(
    State(state): State<AppState>,
    Json(body): Json<DownloadBody>,
) -> Json<serde_json::Value> {
    let targets = body.targets;
    let results: Vec<serde_json::Value> = targets.iter().map(|target| {
        state.push_server.broadcast_download_start(target);
        serde_json::json!({ "target": target, "status": "queued" })
    }).collect();

    serde_json::json!({ "results": results }).into()
}

#[derive(Debug, Deserialize)]
struct UpdateBody {
    ids: Option<Vec<i64>>,
    all: Option<bool>,
}

async fn api_update(
    State(state): State<AppState>,
    Json(body): Json<UpdateBody>,
) -> Json<serde_json::Value> {
    let ids: Vec<i64> = if body.all.unwrap_or(false) {
        with_database(|db| Ok(db.ids())).unwrap_or_default()
    } else if let Some(id_list) = body.ids {
        id_list
    } else {
        Vec::new()
    };

    state.push_server.broadcast("update_start", &format!("{} novels", ids.len()));
    serde_json::json!({ "status": "queued", "count": ids.len() }).into()
}

#[derive(Debug, Deserialize)]
struct ConvertBody {
    targets: Vec<String>,
    device: Option<String>,
}

async fn api_convert(
    State(state): State<AppState>,
    Json(body): Json<ConvertBody>,
) -> Json<serde_json::Value> {
    let device = body.device.unwrap_or_else(|| "text".to_string());
    let results: Vec<serde_json::Value> = body.targets.iter().map(|target| {
        state.push_server.broadcast_convert_start(target);
        serde_json::json!({ "target": target, "device": device, "status": "queued" })
    }).collect();

    serde_json::json!({ "results": results }).into()
}

async fn get_settings(
    State(_state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let record = with_database(|db| {
        db.get(id).cloned().ok_or_else(|| NarouError::NotFound(format!("ID: {}", id)))
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    let archive_root = with_database(|db| Ok(db.archive_root().to_path_buf()))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut novel_dir = archive_root.join(&record.sitename);
    if record.use_subdirectory {
        if let Some(ref ncode) = record.ncode {
            if ncode.len() >= 2 {
                novel_dir.push(&ncode[..2]);
            }
        }
    }
    novel_dir.push(&record.file_title);

    let settings = crate::converter::settings::NovelSettings::load_for_novel(
        id,
        &record.title,
        &record.author,
        &novel_dir,
    );

    let value = serde_json::to_value(&settings)
        .unwrap_or_else(|_| serde_json::json!({}));
    Ok(Json(value))
}

async fn save_settings(
    State(_state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let record = with_database(|db| {
        db.get(id).cloned().ok_or_else(|| NarouError::NotFound(format!("ID: {}", id)))
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    let archive_root = with_database(|db| Ok(db.archive_root().to_path_buf()))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut novel_dir = archive_root.join(&record.sitename);
    if record.use_subdirectory {
        if let Some(ref ncode) = record.ncode {
            if ncode.len() >= 2 {
                novel_dir.push(&ncode[..2]);
            }
        }
    }
    novel_dir.push(&record.file_title);

    let ini_path = novel_dir.join("setting.ini");
    std::fs::create_dir_all(&novel_dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut ini = crate::converter::settings::IniData::load_file(&ini_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if let Some(obj) = body.as_object() {
        for (key, value) in obj {
            if key == "id" || key == "title" || key == "author" || key == "archive_path" || key == "replace_patterns" {
                continue;
            }
            let ini_value = match value {
                serde_json::Value::Bool(b) => crate::converter::settings::IniValue::Boolean(*b),
                serde_json::Value::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        crate::converter::settings::IniValue::Integer(i)
                    } else if let Some(f) = n.as_f64() {
                        crate::converter::settings::IniValue::Float(f)
                    } else {
                        continue;
                    }
                }
                serde_json::Value::String(s) => crate::converter::settings::IniValue::String(s.clone()),
                serde_json::Value::Null => crate::converter::settings::IniValue::Null,
                _ => continue,
            };
            ini.set_global(key, ini_value);
        }
    }

    ini.save(&ini_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(ApiResponse { success: true, message: "Settings saved".to_string() }))
}

async fn list_devices(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let devices = crate::converter::device::OutputManager::available_devices();
    let list: Vec<serde_json::Value> = devices.iter().map(|(name, available)| {
        serde_json::json!({ "name": name, "available": available })
    }).collect();
    Json(serde_json::json!({ "devices": list }))
}

async fn queue_status(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let queue = crate::queue::PersistentQueue::with_default().unwrap_or_else(|_| {
        let path = std::path::PathBuf::from(".narou").join("queue.yaml");
        crate::queue::PersistentQueue::new(&path).unwrap()
    });

    Json(serde_json::json!({
        "pending": queue.pending_count(),
        "completed": queue.completed_count(),
        "failed": queue.failed_count(),
    }))
}

async fn queue_clear(State(_state): State<AppState>) -> Json<ApiResponse> {
    let result = crate::queue::PersistentQueue::with_default()
        .and_then(|q| q.clear());

    match result {
        Ok(_) => Json(ApiResponse { success: true, message: "Queue cleared".to_string() }),
        Err(e) => Json(ApiResponse { success: false, message: e.to_string() }),
    }
}

#[derive(Debug, Deserialize)]
struct LogsParams {
    count: Option<usize>,
}

async fn recent_logs(
    State(state): State<AppState>,
    Query(params): Query<LogsParams>,
) -> Json<serde_json::Value> {
    let count = params.count.unwrap_or(100);
    let logger = push::StreamingLogger::new(state.push_server.clone());
    let logs = logger.recent_logs(count);
    Json(serde_json::json!({ "logs": logs }))
}

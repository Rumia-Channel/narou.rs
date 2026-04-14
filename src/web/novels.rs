use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
};

use crate::compat::{load_frozen_ids_from_inventory, record_is_frozen, set_frozen_state};
use crate::db::{with_database, with_database_mut};
use crate::error::NarouError;

use super::AppState;
use super::state::{ApiResponse, IdPath, ListParams, NovelListItem, NovelListResponse};

pub async fn index() -> &'static str {
    "narou.rs API server"
}

pub async fn novels_count(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let count = with_database(|db| Ok(db.all_records().len() as u64)).unwrap_or(0);
    Json(serde_json::json!({ "count": count }))
}

pub async fn api_list(
    Query(params): Query<ListParams>,
    State(_state): State<AppState>,
) -> Result<Json<NovelListResponse>, (StatusCode, String)> {
    let draw = params.draw.unwrap_or(1);
    let start = params.start.unwrap_or(0);
    let length = params.length.unwrap_or(50);
    let search = params.search_value.unwrap_or_default();
    let order_col = params.order_column.unwrap_or(0);
    let order_dir = params.order_dir.unwrap_or_else(|| "asc".to_string());
    let frozen_ids = with_database(|db| load_frozen_ids_from_inventory(db.inventory())).unwrap_or_default();

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
                        || r.tags
                            .iter()
                            .any(|t| t.to_lowercase().contains(&search_lower))
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
                "general_lastup" => a
                    .general_lastup
                    .unwrap_or_default()
                    .cmp(&b.general_lastup.unwrap_or_default()),
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
                frozen: record_is_frozen(r, &frozen_ids),
                length: r.length,
                toc_url: r.toc_url.clone(),
                general_all_no: r.general_all_no,
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

pub async fn get_novel(
    State(_state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let record = with_database(|db| {
        db.get(id)
            .cloned()
            .ok_or_else(|| NarouError::NotFound(format!("ID: {}", id)))
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    let value = serde_json::to_value(&record).unwrap_or_default();
    Ok(Json(value))
}

pub async fn remove_novel(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let result = with_database_mut(|db| {
        if let Some(record) = db.remove(id) {
            let dir = crate::db::existing_novel_dir_for_record(db.archive_root(), &record);
            let _ = std::fs::remove_dir_all(&dir);
            db.save()?;
            Ok::<String, NarouError>(record.title)
        } else {
            Err(NarouError::NotFound(format!("ID: {}", id)))
        }
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    state.push_server.broadcast("remove", &result);
    Ok(Json(ApiResponse {
        success: true,
        message: result,
    }))
}

pub async fn freeze_novel(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    set_frozen_state(id, true).map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    state.push_server.broadcast("freeze", &id.to_string());
    Ok(Json(ApiResponse {
        success: true,
        message: format!("Froze {}", id),
    }))
}

pub async fn unfreeze_novel(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    set_frozen_state(id, false).map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    state.push_server.broadcast("unfreeze", &id.to_string());
    Ok(Json(ApiResponse {
        success: true,
        message: format!("Unfroze {}", id),
    }))
}

use axum::{extract::State, http::StatusCode, response::Json};

use crate::compat::set_frozen_state;
use crate::db::with_database_mut;
use crate::error::NarouError;

use super::AppState;
use super::state::{ApiResponse, BatchIdsBody, TagBody};

pub async fn batch_tag(
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

    state.push_server.broadcast(
        "batch_tag",
        &format!("{} items tagged with {}", count, tag_body.tag),
    );
    Ok(Json(ApiResponse {
        success: true,
        message: format!("Tagged {} novels", count),
    }))
}

pub async fn batch_untag(
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

    state
        .push_server
        .broadcast("batch_untag", &count.to_string());
    Ok(Json(ApiResponse {
        success: true,
        message: format!("Untagged {} novels", count),
    }))
}

pub async fn batch_freeze(
    State(state): State<AppState>,
    Json(body): Json<BatchIdsBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let mut count = 0usize;
    for id in &body.ids {
        if set_frozen_state(*id, true).is_ok() {
            count += 1;
        }
    }

    state
        .push_server
        .broadcast("batch_freeze", &count.to_string());
    Ok(Json(ApiResponse {
        success: true,
        message: format!("Froze {} novels", count),
    }))
}

pub async fn batch_unfreeze(
    State(state): State<AppState>,
    Json(body): Json<BatchIdsBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let mut count = 0usize;
    for id in &body.ids {
        if set_frozen_state(*id, false).is_ok() {
            count += 1;
        }
    }

    state
        .push_server
        .broadcast("batch_unfreeze", &count.to_string());
    Ok(Json(ApiResponse {
        success: true,
        message: format!("Unfroze {} novels", count),
    }))
}

pub async fn batch_remove(
    State(state): State<AppState>,
    Json(body): Json<BatchIdsBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let with_file = body.with_file.unwrap_or(false);
    let count = with_database_mut(|db| {
        let mut count = 0usize;
        for id in &body.ids {
            if let Some(record) = db.remove(*id) {
                if with_file {
                    let dir = crate::db::existing_novel_dir_for_record(db.archive_root(), &record);
                    let _ = std::fs::remove_dir_all(&dir);
                }
                count += 1;
            }
        }
        db.save()?;
        Ok::<usize, NarouError>(count)
    })
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    state
        .push_server
        .broadcast("batch_remove", &count.to_string());
    Ok(Json(ApiResponse {
        success: true,
        message: format!("Removed {} novels", count),
    }))
}

pub async fn batch_remove_with_file(
    State(state): State<AppState>,
    Json(body): Json<BatchIdsBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let body = BatchIdsBody {
        ids: body.ids,
        with_file: Some(true),
    };
    batch_remove(State(state), Json(body)).await
}

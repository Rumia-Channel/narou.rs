use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};

use crate::db::with_database_mut;
use crate::error::NarouError;

use super::AppState;
use super::state::{ApiResponse, IdPath, TagBody, TagsBody};

pub async fn add_tag(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    Json(body): Json<TagBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    with_database_mut(|db| {
        let record = db
            .get(id)
            .cloned()
            .ok_or_else(|| NarouError::NotFound(format!("ID: {}", id)))?;
        let mut updated = record;
        if !updated.tags.contains(&body.tag) {
            updated.tags.push(body.tag.clone());
        }
        db.insert(updated);
        db.save()
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    state.push_server.broadcast_event("table.reload", "");
    state.push_server.broadcast_event("tag.updateCanvas", "");
    Ok(Json(ApiResponse {
        success: true,
        message: "Tag added".to_string(),
    }))
}

pub async fn remove_tag(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    Json(body): Json<TagBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    with_database_mut(|db| {
        let record = db
            .get(id)
            .cloned()
            .ok_or_else(|| NarouError::NotFound(format!("ID: {}", id)))?;
        let mut updated = record;
        updated.tags.retain(|t| t != &body.tag);
        db.insert(updated);
        db.save()
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    state.push_server.broadcast_event("table.reload", "");
    state.push_server.broadcast_event("tag.updateCanvas", "");
    Ok(Json(ApiResponse {
        success: true,
        message: "Tag removed".to_string(),
    }))
}

/// POST /api/novels/{id}/tags — add multiple tags (frontend-compatible)
pub async fn add_tags(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    Json(body): Json<TagsBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    with_database_mut(|db| {
        let record = db
            .get(id)
            .cloned()
            .ok_or_else(|| NarouError::NotFound(format!("ID: {}", id)))?;
        let mut updated = record;
        for tag in &body.tags {
            if !updated.tags.contains(tag) {
                updated.tags.push(tag.clone());
            }
        }
        db.insert(updated);
        db.save()
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    state.push_server.broadcast_event("table.reload", "");
    state.push_server.broadcast_event("tag.updateCanvas", "");
    Ok(Json(ApiResponse {
        success: true,
        message: "Tags added".to_string(),
    }))
}

/// POST /api/novels/{id}/tags/remove — remove multiple tags (frontend-compatible)
pub async fn remove_tags(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    Json(body): Json<TagsBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    with_database_mut(|db| {
        let record = db
            .get(id)
            .cloned()
            .ok_or_else(|| NarouError::NotFound(format!("ID: {}", id)))?;
        let mut updated = record;
        updated.tags.retain(|t| !body.tags.contains(t));
        db.insert(updated);
        db.save()
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    state.push_server.broadcast_event("table.reload", "");
    state.push_server.broadcast_event("tag.updateCanvas", "");
    Ok(Json(ApiResponse {
        success: true,
        message: "Tags removed".to_string(),
    }))
}

pub async fn update_tags(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    Json(body): Json<TagsBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    with_database_mut(|db| {
        let record = db
            .get(id)
            .cloned()
            .ok_or_else(|| NarouError::NotFound(format!("ID: {}", id)))?;
        let mut updated = record;
        updated.tags = body.tags;
        db.insert(updated);
        db.save()
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    state.push_server.broadcast_event("table.reload", "");
    state.push_server.broadcast_event("tag.updateCanvas", "");
    Ok(Json(ApiResponse {
        success: true,
        message: "Tags updated".to_string(),
    }))
}

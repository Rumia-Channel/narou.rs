use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};

use crate::db::with_database_mut;
use crate::error::NarouError;

use super::state::{ApiResponse, IdPath, TagBody, TagsBody};
use super::AppState;

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

    state
        .push_server
        .broadcast("tag_add", &format!("{} {}", id, body.tag));
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

    state
        .push_server
        .broadcast("tag_remove", &format!("{} {}", id, body.tag));
    Ok(Json(ApiResponse {
        success: true,
        message: "Tag removed".to_string(),
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

    state.push_server.broadcast("tags_update", &id.to_string());
    Ok(Json(ApiResponse {
        success: true,
        message: "Tags updated".to_string(),
    }))
}

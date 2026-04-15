use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};

use crate::db::with_database_mut;
use crate::error::NarouError;

use super::AppState;
use super::state::{ApiResponse, EditTagBody, IdPath, TagBody, TagsBody};

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

/// POST /api/edit_tag — bulk tag edit with tri-state (Ruby parity)
/// states: { "tag_name": 0|1|2 } where 0=delete, 1=keep, 2=add
pub async fn edit_tag(
    State(state): State<AppState>,
    Json(body): Json<EditTagBody>,
) -> Json<serde_json::Value> {
    let ids: Vec<i64> = body
        .ids
        .iter()
        .filter_map(|v| match v {
            serde_json::Value::Number(n) => n.as_i64(),
            serde_json::Value::String(s) => s.parse::<i64>().ok(),
            _ => None,
        })
        .collect();

    if ids.is_empty() {
        return serde_json::json!({ "success": false, "error": "No valid IDs" }).into();
    }

    // Group tags by state: 0=delete, 2=add (1=keep is no-op)
    let mut tags_to_add: Vec<String> = Vec::new();
    let mut tags_to_delete: Vec<String> = Vec::new();

    for (tag, state_val) in &body.states {
        let s = match state_val {
            serde_json::Value::Number(n) => n.as_i64().unwrap_or(1),
            serde_json::Value::String(s) => s.parse::<i64>().unwrap_or(1),
            _ => 1,
        };
        match s {
            0 => tags_to_delete.push(tag.clone()),
            2 => tags_to_add.push(tag.clone()),
            _ => {} // 1 = keep, no-op
        }
    }

    let result = with_database_mut(|db| {
        for &id in &ids {
            if let Some(record) = db.get(id).cloned() {
                let mut updated = record;
                // Delete tags
                if !tags_to_delete.is_empty() {
                    updated.tags.retain(|t| !tags_to_delete.contains(t));
                }
                // Add tags
                for tag in &tags_to_add {
                    if !updated.tags.contains(tag) {
                        updated.tags.push(tag.clone());
                    }
                }
                db.insert(updated);
            }
        }
        db.save()
    });

    if let Err(e) = result {
        return serde_json::json!({ "success": false, "error": e.to_string() }).into();
    }

    state.push_server.broadcast_event("table.reload", "");
    state.push_server.broadcast_event("tag.updateCanvas", "");
    serde_json::json!({ "success": true }).into()
}

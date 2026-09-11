use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};


use super::AppState;
use super::sort_state::sort_ids_from_records;
use super::state::{ApiResponse, EditTagBody, IdPath, TagBody, TagsBody};

fn validate_tags(tags: &[String]) -> Result<Vec<String>, String> {
    if tags.len() > super::MAX_WEB_TAGS_PER_REQUEST {
        return Err("too many tags".to_string());
    }
    tags.iter()
        .map(|tag| super::normalize_web_tag_name(tag))
        .collect()
}

async fn apply_tag_change(
    state: &AppState,
    ids: &[i64],
    action: crate::application::TagAction,
    tags: Vec<String>,
) -> Result<crate::application::TagChangeResult, (StatusCode, String)> {
    let result = state
        .services
        .novel_actions
        .change_tags(&crate::application::TagChangeRequest {
            ids: ids.iter().copied().map(Into::into).collect(),
            action,
            tags,
        })
        .await
        .map_err(|error| match error {
            crate::application::ApplicationError::InvalidRequest(message) => {
                (StatusCode::BAD_REQUEST, message)
            }
            crate::application::ApplicationError::NotFound(message) => {
                (StatusCode::NOT_FOUND, message)
            }
            crate::application::ApplicationError::Platform(message) => {
                (StatusCode::INTERNAL_SERVER_ERROR, message)
            }
        })?;
    state.push_server.broadcast_event("table.reload", "");
    state.push_server.broadcast_event("tag.updateCanvas", "");
    Ok(result)
}

async fn sort_ids_for_request(
    state: &AppState,
    ids: &[i64],
    sort_state: Option<&serde_json::Value>,
    timestamp: Option<u64>,
) -> Vec<i64> {
    let records = state.services.library.records().await.unwrap_or_default();
    sort_ids_from_records(ids, &records, sort_state, timestamp)
}

fn ensure_all_ids_found(
    result: &crate::application::TagChangeResult,
) -> Result<(), (StatusCode, String)> {
    if result.missing.is_empty() {
        Ok(())
    } else {
        Err((
            StatusCode::NOT_FOUND,
            format!("ID: {}", result.missing[0].0),
        ))
    }
}

pub async fn add_tag(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    Json(body): Json<TagBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let tag = super::normalize_web_tag_name(&body.tag)
        .map_err(|message| (StatusCode::BAD_REQUEST, message))?;
    let result = apply_tag_change(
        &state,
        &[id],
        crate::application::TagAction::Add,
        vec![tag],
    )
    .await?;
    ensure_all_ids_found(&result)?;

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
    let tag = super::normalize_web_tag_name(&body.tag)
        .map_err(|message| (StatusCode::BAD_REQUEST, message))?;
    let result = apply_tag_change(
        &state,
        &[id],
        crate::application::TagAction::Remove,
        vec![tag],
    )
    .await?;
    ensure_all_ids_found(&result)?;

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
    let tags = validate_tags(&body.tags).map_err(|message| (StatusCode::BAD_REQUEST, message))?;
    let result = apply_tag_change(
        &state,
        &[id],
        crate::application::TagAction::Add,
        tags,
    )
    .await?;
    ensure_all_ids_found(&result)?;

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
    let tags = validate_tags(&body.tags).map_err(|message| (StatusCode::BAD_REQUEST, message))?;
    let result = apply_tag_change(
        &state,
        &[id],
        crate::application::TagAction::Remove,
        tags,
    )
    .await?;
    ensure_all_ids_found(&result)?;

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
    let tags = validate_tags(&body.tags).map_err(|message| (StatusCode::BAD_REQUEST, message))?;
    let result = apply_tag_change(
        &state,
        &[id],
        crate::application::TagAction::Replace,
        tags,
    )
    .await?;
    ensure_all_ids_found(&result)?;

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
    if body.ids.len() > super::max_web_targets_per_request(&state).await {
        return serde_json::json!({ "success": false, "error": "too many ids" }).into();
    }
    if body.states.len() > super::MAX_WEB_TAGS_PER_REQUEST {
        return serde_json::json!({ "success": false, "error": "too many tags" }).into();
    }
    let ids: Vec<i64> = body
        .ids
        .iter()
        .filter_map(|v| match v {
            serde_json::Value::Number(n) => n.as_i64(),
            serde_json::Value::String(s) => s.parse::<i64>().ok(),
            _ => None,
        })
        .collect();
    let ids =
        sort_ids_for_request(&state, &ids, body.sort_state.as_ref(), body.timestamp).await;

    if ids.is_empty() {
        return serde_json::json!({ "success": false, "error": "No valid IDs" }).into();
    }

    let mut tags_to_add: Vec<String> = Vec::new();
    let mut tags_to_delete: Vec<String> = Vec::new();

    for (tag, state_val) in &body.states {
        let tag = match super::normalize_web_tag_name(tag) {
            Ok(tag) => tag,
            Err(error) => {
                return serde_json::json!({ "success": false, "error": error }).into();
            }
        };
        let s = match state_val {
            serde_json::Value::Number(n) => n.as_i64().unwrap_or(1),
            serde_json::Value::String(s) => s.parse::<i64>().unwrap_or(1),
            _ => 1,
        };
        match s {
            0 => tags_to_delete.push(tag),
            2 => tags_to_add.push(tag),
            _ => {}
        }
    }

    if !tags_to_delete.is_empty() {
        if let Err((_, error)) = apply_tag_change(
            &state,
            &ids,
            crate::application::TagAction::Remove,
            tags_to_delete,
        )
        .await
        .and_then(|result| ensure_all_ids_found(&result).map(|()| result))
        {
            return serde_json::json!({ "success": false, "error": error }).into();
        }
    }
    if !tags_to_add.is_empty() {
        if let Err((_, error)) = apply_tag_change(
            &state,
            &ids,
            crate::application::TagAction::Add,
            tags_to_add,
        )
        .await
        .and_then(|result| ensure_all_ids_found(&result).map(|()| result))
        {
            return serde_json::json!({ "success": false, "error": error }).into();
        }
    }

    serde_json::json!({ "success": true }).into()
}

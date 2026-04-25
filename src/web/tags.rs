use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};

use crate::db::with_database_mut;

use super::AppState;
use super::sort_state::sort_ids_for_request;
use super::state::{ApiResponse, EditTagBody, IdPath, TagBody, TagsBody};

fn validate_tags(tags: &[String]) -> Result<Vec<String>, String> {
    if tags.len() > super::MAX_WEB_TAGS_PER_REQUEST {
        return Err("too many tags".to_string());
    }
    tags.iter()
        .map(|tag| super::normalize_web_tag_name(tag))
        .collect()
}

async fn run_tag_commands(
    state: &AppState,
    commands: Vec<Vec<String>>,
) -> Result<(), (StatusCode, String)> {
    for args in commands {
        let output = super::jobs::run_cli_and_broadcast(
            state,
            args,
            super::non_external_console_target(),
        )
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
        if !output.status.success() {
            return Err((StatusCode::INTERNAL_SERVER_ERROR, "tag の実行に失敗しました".to_string()));
        }
    }
    with_database_mut(|db| db.refresh())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    state.push_server.broadcast_event("table.reload", "");
    state.push_server.broadcast_event("tag.updateCanvas", "");
    Ok(())
}

pub async fn add_tag(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    Json(body): Json<TagBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let tag = super::normalize_web_tag_name(&body.tag)
        .map_err(|message| (StatusCode::BAD_REQUEST, message))?;
    run_tag_commands(
        &state,
        vec![vec![
            "tag".to_string(),
            "--add".to_string(),
            tag,
            id.to_string(),
        ]],
    )
    .await?;

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
    run_tag_commands(
        &state,
        vec![vec![
            "tag".to_string(),
            "--delete".to_string(),
            tag,
            id.to_string(),
        ]],
    )
    .await?;

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
    run_tag_commands(
        &state,
        vec![{
            let mut args = vec!["tag".to_string(), "--add".to_string(), tags.join(" ")];
            args.push(id.to_string());
            args
        }],
    )
    .await?;

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
    run_tag_commands(
        &state,
        vec![{
            let mut args = vec!["tag".to_string(), "--delete".to_string(), tags.join(" ")];
            args.push(id.to_string());
            args
        }],
    )
    .await?;

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
    let mut commands = vec![vec!["tag".to_string(), "--clear".to_string(), id.to_string()]];
    if !tags.is_empty() {
        commands.push({
            let mut args = vec!["tag".to_string(), "--add".to_string(), tags.join(" ")];
            args.push(id.to_string());
            args
        });
    }
    run_tag_commands(&state, commands).await?;

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
    if body.ids.len() > super::MAX_WEB_TARGETS_PER_REQUEST {
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
    let ids = sort_ids_for_request(&ids, body.sort_state.as_ref(), body.timestamp);

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

    let mut commands = Vec::new();
    if !tags_to_delete.is_empty() {
        let mut args = vec![
            "tag".to_string(),
            "--delete".to_string(),
            tags_to_delete.join(" "),
        ];
        args.extend(ids.iter().map(ToString::to_string));
        commands.push(args);
    }
    if !tags_to_add.is_empty() {
        let mut args = vec![
            "tag".to_string(),
            "--add".to_string(),
            tags_to_add.join(" "),
        ];
        args.extend(ids.iter().map(ToString::to_string));
        commands.push(args);
    }

    if let Err((_, error)) = run_tag_commands(&state, commands).await {
        return serde_json::json!({ "success": false, "error": error }).into();
    }

    serde_json::json!({ "success": true }).into()
}

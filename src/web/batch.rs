use axum::{extract::State, http::StatusCode, response::Json};

use crate::db::with_database_mut;

use super::AppState;
use super::sort_state::sort_ids_for_request;
use super::state::{ApiResponse, BatchIdsBody, TagBody};

async fn run_batch_cli(
    state: &AppState,
    args: Vec<String>,
    reload_tags: bool,
) -> Result<(), (StatusCode, String)> {
    let output = super::jobs::run_cli_and_broadcast(state, args, super::non_external_console_target())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if !output.status.success() {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, "CLI command failed".to_string()));
    }
    with_database_mut(|db| db.refresh())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    state.push_server.broadcast_event("table.reload", "");
    if reload_tags {
        state.push_server.broadcast_event("tag.updateCanvas", "");
    }
    Ok(())
}

pub async fn batch_tag(
    State(state): State<AppState>,
    Json(body): Json<(BatchIdsBody, TagBody)>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let (ids_body, tag_body) = body;
    if ids_body.ids.len() > super::max_web_targets_per_request() {
        return Err((StatusCode::BAD_REQUEST, "too many ids".to_string()));
    }
    let ids = sort_ids_for_request(&ids_body.ids, ids_body.sort_state.as_ref(), ids_body.timestamp);
    let tag = super::normalize_web_tag_name(&tag_body.tag)
        .map_err(|message| (StatusCode::BAD_REQUEST, message))?;
    let mut args = vec!["tag".to_string(), "--add".to_string(), tag];
    args.extend(ids.iter().map(ToString::to_string));
    run_batch_cli(&state, args, true).await?;

    Ok(Json(ApiResponse {
        success: true,
        message: format!("Tagged {} novels", ids.len()),
    }))
}

pub async fn batch_untag(
    State(state): State<AppState>,
    Json(body): Json<(BatchIdsBody, TagBody)>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let (ids_body, tag_body) = body;
    if ids_body.ids.len() > super::max_web_targets_per_request() {
        return Err((StatusCode::BAD_REQUEST, "too many ids".to_string()));
    }
    let ids = sort_ids_for_request(&ids_body.ids, ids_body.sort_state.as_ref(), ids_body.timestamp);
    let tag = super::normalize_web_tag_name(&tag_body.tag)
        .map_err(|message| (StatusCode::BAD_REQUEST, message))?;
    let mut args = vec!["tag".to_string(), "--delete".to_string(), tag];
    args.extend(ids.iter().map(ToString::to_string));
    run_batch_cli(&state, args, true).await?;

    Ok(Json(ApiResponse {
        success: true,
        message: format!("Untagged {} novels", ids.len()),
    }))
}

pub async fn batch_freeze(
    State(state): State<AppState>,
    Json(body): Json<BatchIdsBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    if body.ids.len() > super::max_web_targets_per_request() {
        return Err((StatusCode::BAD_REQUEST, "too many ids".to_string()));
    }
    let ids = sort_ids_for_request(&body.ids, body.sort_state.as_ref(), body.timestamp);
    let mut args = vec!["freeze".to_string(), "--on".to_string()];
    args.extend(ids.iter().map(ToString::to_string));
    run_batch_cli(&state, args, false).await?;

    Ok(Json(ApiResponse {
        success: true,
        message: format!("Froze {} novels", ids.len()),
    }))
}

pub async fn batch_unfreeze(
    State(state): State<AppState>,
    Json(body): Json<BatchIdsBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    if body.ids.len() > super::max_web_targets_per_request() {
        return Err((StatusCode::BAD_REQUEST, "too many ids".to_string()));
    }
    let ids = sort_ids_for_request(&body.ids, body.sort_state.as_ref(), body.timestamp);
    let mut args = vec!["freeze".to_string(), "--off".to_string()];
    args.extend(ids.iter().map(ToString::to_string));
    run_batch_cli(&state, args, false).await?;

    Ok(Json(ApiResponse {
        success: true,
        message: format!("Unfroze {} novels", ids.len()),
    }))
}

pub async fn batch_remove(
    State(state): State<AppState>,
    Json(body): Json<BatchIdsBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    if body.ids.len() > super::max_web_targets_per_request() {
        return Err((StatusCode::BAD_REQUEST, "too many ids".to_string()));
    }
    let with_file = body.with_file.unwrap_or(false);
    let ids = sort_ids_for_request(&body.ids, body.sort_state.as_ref(), body.timestamp);
    let mut args = vec!["remove".to_string(), "--yes".to_string()];
    if with_file {
        args.push("--with-file".to_string());
    }
    args.extend(ids.iter().map(ToString::to_string));
    run_batch_cli(&state, args, true).await?;

    Ok(Json(ApiResponse {
        success: true,
        message: format!("Removed {} novels", ids.len()),
    }))
}

pub async fn batch_remove_with_file(
    State(state): State<AppState>,
    Json(body): Json<BatchIdsBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let body = BatchIdsBody {
        ids: body.ids,
        with_file: Some(true),
        sort_state: body.sort_state,
        timestamp: body.timestamp,
    };
    batch_remove(State(state), Json(body)).await
}

pub async fn batch_freeze_toggle(
    State(state): State<AppState>,
    Json(body): Json<BatchIdsBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if body.ids.len() > super::max_web_targets_per_request() {
        return Err((StatusCode::BAD_REQUEST, "too many ids".to_string()));
    }
    let ids = sort_ids_for_request(&body.ids, body.sort_state.as_ref(), body.timestamp);
    let mut args = vec!["freeze".to_string()];
    args.extend(ids.iter().map(ToString::to_string));
    run_batch_cli(&state, args, false).await?;

    Ok(Json(serde_json::json!({
        "success": true,
        "message": "凍結状態を切り替えました",
        "count": ids.len(),
    })))
}

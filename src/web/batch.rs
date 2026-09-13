use axum::{extract::State, http::StatusCode, response::Json};
use super::AppState;
use super::sort_state::sort_ids_from_records;
use super::state::{ApiResponse, BatchIdsBody, TagBody};

async fn apply_tag_change(
    state: &AppState,
    ids: &[i64],
    action: crate::application::TagAction,
    tags: Vec<String>,
) -> Result<crate::application::TagChangeResult, (StatusCode, String)> {
    state
        .services
        .novel_actions
        .change_tags(&crate::application::TagChangeRequest {
            ids: ids.iter().copied().map(Into::into).collect(),
            action,
            tags,
        })
        .await
        .map_err(map_application_error)
}

async fn apply_freeze(
    state: &AppState,
    ids: &[i64],
    freeze: bool,
) -> Result<crate::application::FreezeResult, (StatusCode, String)> {
    state
        .services
        .novel_actions
        .apply_freeze(&crate::application::FreezeRequest {
            ids: ids.iter().copied().map(Into::into).collect(),
            freeze,
        })
        .await
        .map_err(map_application_error)
}

fn map_application_error(error: crate::application::ApplicationError) -> (StatusCode, String) {
    match error {
        crate::application::ApplicationError::InvalidRequest(message) => {
            (StatusCode::BAD_REQUEST, message)
        }
        crate::application::ApplicationError::NotFound(message) => {
            (StatusCode::NOT_FOUND, message)
        }
        crate::application::ApplicationError::Platform(message) => {
            (StatusCode::INTERNAL_SERVER_ERROR, message)
        }
    }
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

fn ensure_no_missing(
    missing: &[crate::platform::NovelId],
) -> Result<(), (StatusCode, String)> {
    missing.first().map_or(Ok(()), |id| {
        Err((StatusCode::NOT_FOUND, format!("ID: {}", id.0)))
    })
}

pub async fn batch_tag(
    State(state): State<AppState>,
    Json(body): Json<(BatchIdsBody, TagBody)>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let (ids_body, tag_body) = body;
    if ids_body.ids.len() > super::max_web_targets_per_request(&state).await {
        return Err((StatusCode::BAD_REQUEST, "too many ids".to_string()));
    }
    let ids =
        sort_ids_for_request(&state, &ids_body.ids, ids_body.sort_state.as_ref(), ids_body.timestamp)
            .await;
    let tag = super::normalize_web_tag_name(&tag_body.tag)
        .map_err(|message| (StatusCode::BAD_REQUEST, message))?;
    let result = apply_tag_change(&state, &ids, crate::application::TagAction::Add, vec![tag]).await?;
    ensure_no_missing(&result.missing)?;
    state.push_server.broadcast_event("table.reload", "");
    state.push_server.broadcast_event("tag.updateCanvas", "");

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
    if ids_body.ids.len() > super::max_web_targets_per_request(&state).await {
        return Err((StatusCode::BAD_REQUEST, "too many ids".to_string()));
    }
    let ids =
        sort_ids_for_request(&state, &ids_body.ids, ids_body.sort_state.as_ref(), ids_body.timestamp)
            .await;
    let tag = super::normalize_web_tag_name(&tag_body.tag)
        .map_err(|message| (StatusCode::BAD_REQUEST, message))?;
    let result =
        apply_tag_change(&state, &ids, crate::application::TagAction::Remove, vec![tag]).await?;
    ensure_no_missing(&result.missing)?;
    state.push_server.broadcast_event("table.reload", "");
    state.push_server.broadcast_event("tag.updateCanvas", "");

    Ok(Json(ApiResponse {
        success: true,
        message: format!("Untagged {} novels", ids.len()),
    }))
}

pub async fn batch_freeze(
    State(state): State<AppState>,
    Json(body): Json<BatchIdsBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    if body.ids.len() > super::max_web_targets_per_request(&state).await {
        return Err((StatusCode::BAD_REQUEST, "too many ids".to_string()));
    }
    let ids = sort_ids_for_request(&state, &body.ids, body.sort_state.as_ref(), body.timestamp).await;
    let result = apply_freeze(&state, &ids, true).await?;
    ensure_no_missing(&result.missing)?;
    state.push_server.broadcast_event("table.reload", "");

    Ok(Json(ApiResponse {
        success: result.store_failed.is_empty(),
        message: format!("Froze {} novels", ids.len()),
    }))
}

pub async fn batch_unfreeze(
    State(state): State<AppState>,
    Json(body): Json<BatchIdsBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    if body.ids.len() > super::max_web_targets_per_request(&state).await {
        return Err((StatusCode::BAD_REQUEST, "too many ids".to_string()));
    }
    let ids = sort_ids_for_request(&state, &body.ids, body.sort_state.as_ref(), body.timestamp).await;
    let result = apply_freeze(&state, &ids, false).await?;
    ensure_no_missing(&result.missing)?;
    state.push_server.broadcast_event("table.reload", "");

    Ok(Json(ApiResponse {
        success: result.store_failed.is_empty(),
        message: format!("Unfroze {} novels", ids.len()),
    }))
}

pub async fn batch_remove(
    State(state): State<AppState>,
    Json(body): Json<BatchIdsBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    if body.ids.len() > super::max_web_targets_per_request(&state).await {
        return Err((StatusCode::BAD_REQUEST, "too many ids".to_string()));
    }
    let with_file = body.with_file.unwrap_or(false);
    let ids = sort_ids_for_request(&state, &body.ids, body.sort_state.as_ref(), body.timestamp).await;
    let result = state
        .services
        .novel_actions
        .remove(&crate::application::RemoveRequest {
            ids: ids.iter().copied().map(Into::into).collect(),
            delete_files: with_file,
        })
        .await
        .map_err(map_application_error)?;
    ensure_no_missing(&result.missing)?;
    let files_ok = result
        .files
        .iter()
        .all(|(_, status)| matches!(status, crate::application::FileDeletionStatus::Deleted));
    state.push_server.broadcast_event("table.reload", "");
    state.push_server.broadcast_event("tag.updateCanvas", "");

    Ok(Json(ApiResponse {
        success: files_ok,
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
    if body.ids.len() > super::max_web_targets_per_request(&state).await {
        return Err((StatusCode::BAD_REQUEST, "too many ids".to_string()));
    }
    let ids = sort_ids_for_request(&state, &body.ids, body.sort_state.as_ref(), body.timestamp).await;
    let result = state
        .services
        .novel_actions
        .toggle_freeze(&ids.iter().copied().map(Into::into).collect::<Vec<_>>())
        .await
        .map_err(map_application_error)?;
    ensure_no_missing(&result.missing)?;
    state.push_server.broadcast_event("table.reload", "");

    Ok(Json(serde_json::json!({
        "success": result.store_failed.is_empty(),
        "message": "凍結状態を切り替えました",
        "count": ids.len(),
    })))
}

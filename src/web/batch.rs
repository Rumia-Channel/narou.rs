use axum::{extract::State, http::StatusCode, response::Json};

use crate::compat::{load_frozen_ids_from_inventory, set_frozen_state};
use crate::db::{with_database, with_database_mut};
use crate::error::NarouError;

use super::AppState;
use super::state::{ApiResponse, BatchIdsBody, TagBody};

pub async fn batch_tag(
    State(state): State<AppState>,
    Json(body): Json<(BatchIdsBody, TagBody)>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let (ids_body, tag_body) = body;
    if ids_body.ids.len() > super::MAX_WEB_TARGETS_PER_REQUEST {
        return Err((StatusCode::BAD_REQUEST, "too many ids".to_string()));
    }
    let tag = super::validate_web_tag_name(&tag_body.tag)
        .map_err(|message| (StatusCode::BAD_REQUEST, message))?;
    let count = with_database_mut(|db| {
        let mut count = 0usize;
        for id in &ids_body.ids {
            if let Some(record) = db.get(*id).cloned() {
                let mut updated = record;
                if !updated.tags.contains(&tag) {
                    updated.tags.push(tag.clone());
                }
                db.insert(updated);
                count += 1;
            }
        }
        db.save()?;
        Ok::<usize, NarouError>(count)
    })
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    state.push_server.broadcast_event("table.reload", "");
    state.push_server.broadcast_event("tag.updateCanvas", "");
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
    if ids_body.ids.len() > super::MAX_WEB_TARGETS_PER_REQUEST {
        return Err((StatusCode::BAD_REQUEST, "too many ids".to_string()));
    }
    let tag = super::validate_web_tag_name(&tag_body.tag)
        .map_err(|message| (StatusCode::BAD_REQUEST, message))?;
    let count = with_database_mut(|db| {
        let mut count = 0usize;
        for id in &ids_body.ids {
            if let Some(record) = db.get(*id).cloned() {
                let mut updated = record;
                updated.tags.retain(|t| t != &tag);
                db.insert(updated);
                count += 1;
            }
        }
        db.save()?;
        Ok::<usize, NarouError>(count)
    })
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    state.push_server.broadcast_event("table.reload", "");
    state.push_server.broadcast_event("tag.updateCanvas", "");
    Ok(Json(ApiResponse {
        success: true,
        message: format!("Untagged {} novels", count),
    }))
}

pub async fn batch_freeze(
    State(state): State<AppState>,
    Json(body): Json<BatchIdsBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    if body.ids.len() > super::MAX_WEB_TARGETS_PER_REQUEST {
        return Err((StatusCode::BAD_REQUEST, "too many ids".to_string()));
    }
    let mut count = 0usize;
    for id in &body.ids {
        if set_frozen_state(*id, true).is_ok() {
            count += 1;
        }
    }

    state.push_server.broadcast_event("table.reload", "");
    Ok(Json(ApiResponse {
        success: true,
        message: format!("Froze {} novels", count),
    }))
}

pub async fn batch_unfreeze(
    State(state): State<AppState>,
    Json(body): Json<BatchIdsBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    if body.ids.len() > super::MAX_WEB_TARGETS_PER_REQUEST {
        return Err((StatusCode::BAD_REQUEST, "too many ids".to_string()));
    }
    let mut count = 0usize;
    for id in &body.ids {
        if set_frozen_state(*id, false).is_ok() {
            count += 1;
        }
    }

    state.push_server.broadcast_event("table.reload", "");
    Ok(Json(ApiResponse {
        success: true,
        message: format!("Unfroze {} novels", count),
    }))
}

pub async fn batch_remove(
    State(state): State<AppState>,
    Json(body): Json<BatchIdsBody>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    if body.ids.len() > super::MAX_WEB_TARGETS_PER_REQUEST {
        return Err((StatusCode::BAD_REQUEST, "too many ids".to_string()));
    }
    let with_file = body.with_file.unwrap_or(false);
    let removed_titles = with_database_mut(|db| {
        let mut titles = Vec::new();
        for id in &body.ids {
            if let Some(record) = db.remove(*id) {
                if with_file {
                    super::remove_novel_storage_dir(db.archive_root(), &record)
                        .map_err(NarouError::Database)?;
                }
                titles.push(record.title);
            }
        }
        db.save()?;
        Ok::<Vec<String>, NarouError>(titles)
    })
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let count = removed_titles.len();

    state.push_server.broadcast_event("table.reload", "");
    state.push_server.broadcast_event("tag.updateCanvas", "");
    if count > 0 {
        state.push_server.broadcast_echo(
            &super::removal_log_message(&removed_titles, with_file),
            super::non_external_console_target(),
        );
    }
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

pub async fn batch_freeze_toggle(
    State(state): State<AppState>,
    Json(body): Json<BatchIdsBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if body.ids.len() > super::MAX_WEB_TARGETS_PER_REQUEST {
        return Err((StatusCode::BAD_REQUEST, "too many ids".to_string()));
    }
    let frozen_ids = with_database(|db| load_frozen_ids_from_inventory(db.inventory()))
        .unwrap_or_default();
    let mut count = 0usize;
    for id in &body.ids {
        let is_frozen = frozen_ids.contains(id);
        if set_frozen_state(*id, !is_frozen).is_ok() {
            count += 1;
        }
    }

    state
        .push_server
        .broadcast_event("table.reload", "");
    Ok(Json(serde_json::json!({
        "success": true,
        "message": "凍結状態を切り替えました",
        "count": count,
    })))
}

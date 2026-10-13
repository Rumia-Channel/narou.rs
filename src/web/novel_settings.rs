
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};

use crate::application::novel_settings::parse_settings_patch;

use super::AppState;
use super::state::IdPath;


fn map_application_error(error: crate::application::ApplicationError) -> (StatusCode, String) {
    match error {
        crate::application::ApplicationError::InvalidRequest(message) => {
            (StatusCode::BAD_REQUEST, message)
        }
        crate::application::ApplicationError::Conflict(message) => {
            (StatusCode::CONFLICT, message)
        }
        crate::application::ApplicationError::NotFound(message) => {
            (StatusCode::NOT_FOUND, message)
        }
        crate::application::ApplicationError::Platform(message) => {
            (StatusCode::INTERNAL_SERVER_ERROR, message)
        }
    }
}

pub async fn get_settings(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let view = state
        .services
        .novel_settings
        .load(id.into())
        .await
        .map_err(map_application_error)?;
    Ok(Json(view.to_json_value(crate::application::webui::capabilities::RuntimeKind::Native)))
}

pub async fn save_settings(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let patch = parse_settings_patch(&body)
        .map_err(|message| (StatusCode::BAD_REQUEST, message))?;

    let result = state
        .services
        .novel_settings
        .save(id.into(), &patch)
        .await
        .map_err(map_application_error)?;

    Ok(Json(result.to_json_value()))
}

pub async fn list_devices(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let devices = crate::converter::device::OutputManager::available_devices();
    let list: Vec<serde_json::Value> = devices
        .iter()
        .map(|(name, available)| serde_json::json!({ "name": name, "available": available }))
        .collect();
    Json(serde_json::json!({ "devices": list }))
}

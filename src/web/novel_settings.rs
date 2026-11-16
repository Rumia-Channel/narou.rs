
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};

use crate::application::novel_settings::parse_settings_patch;

use super::AppState;
use super::state::IdPath;


fn map_application_error(error: crate::application::ApplicationError) -> (StatusCode, String) {
    let (status, _) = crate::application::external_api::http_error(&error);
    (StatusCode::from_u16(status).expect("application status"), error.message().to_owned())
}

pub async fn get_settings(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Response, (StatusCode, String)> {
    let mut reads = state.services.novel_settings.read_scope("native-novel-settings");
    let result = state.services.novel_settings.load(&mut reads, id.into()).await;
    match result {
        Ok(view) => Ok(super::novels::leased_response(
            Json(view.to_json_value(crate::application::webui::capabilities::RuntimeKind::Native)).into_response(), reads,
        )),
        Err(error) => { let _ = reads.release_all().await; Err(map_application_error(error)) }
    }
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

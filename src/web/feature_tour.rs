use axum::{Json, extract::State};

use crate::application::webui::feature_tour::{
    DISABLED_KEY, SEEN_VERSION_KEY, compare_versions, current_entries, is_known_tour_version,
    latest_tour_version, pending_entries,
};
use crate::setting_core::SettingScope;
use crate::version;

use super::AppState;
use super::state::ApiResponse;

pub async fn pending(State(state): State<AppState>) -> Json<serde_json::Value> {
    let seen_version = load_seen_version(&state).await;
    let disabled = load_disabled(&state).await;
    let entries = pending_entries(seen_version.as_deref(), version::VERSION);
    let latest_pending_version = entries
        .iter()
        .map(|entry| entry.version)
        .max_by(|a, b| compare_versions(a, b))
        .unwrap_or("");

    let storage_migration = storage_migration_prompt();
    Json(serde_json::json!({
        "success": true,
        "current_version": version::create_version_string(),
        "seen_version": seen_version,
        "disabled": disabled,
        "latest_pending_version": latest_pending_version,
        "entries": if disabled { Vec::new() } else { entries },
        "storage_migration": storage_migration,
    }))
}

pub async fn all(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "success": true,
        "current_version": version::create_version_string(),
        "seen_version": load_seen_version(&state).await,
        "disabled": load_disabled(&state).await,
        "latest_pending_version": latest_tour_version(),
        "entries": current_entries(version::VERSION),
    }))
}

pub async fn mark_seen(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<ApiResponse> {
    let requested = body["version"].as_str().unwrap_or("").trim();
    if requested.is_empty() {
        return Json(ApiResponse {
            success: false,
            message: "version is required".to_string(),
        });
    }
    if !is_known_tour_version(requested) {
        return Json(ApiResponse {
            success: false,
            message: "unknown tour version".to_string(),
        });
    }

    match crate::application::webui::feature_tour::mark_seen(&state.services.settings, requested).await {
        Ok(()) => Json(ApiResponse {
            success: true,
            message: "OK".to_string(),
        }),
        Err(e) => Json(ApiResponse {
            success: false,
            message: e.to_string(),
        }),
    }
}

pub async fn configure(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<ApiResponse> {
    let disabled = body["disabled"].as_bool().unwrap_or(false);
    match save_disabled(&state, disabled).await {
        Ok(()) => Json(ApiResponse {
            success: true,
            message: "OK".to_string(),
        }),
        Err(e) => Json(ApiResponse {
            success: false,
            message: e.to_string(),
        }),
    }
}

async fn load_seen_version(state: &AppState) -> Option<String> {
    state
        .services
        .settings
        .get_raw(SettingScope::Local, SEEN_VERSION_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|value| value.as_str().map(str::to_owned))
}

async fn load_disabled(state: &AppState) -> bool {
    state
        .services
        .settings
        .get_raw(SettingScope::Local, DISABLED_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|value| crate::setting_core::setting_bool_value(&value))
        .unwrap_or(false)
}


async fn save_disabled(
    state: &AppState,
    disabled: bool,
) -> Result<(), crate::application::ApplicationError> {
    state
        .services
        .settings
        .set_raw(
            SettingScope::Local,
            DISABLED_KEY,
            serde_yaml::Value::Bool(disabled),
        )
        .await
}

#[cfg(feature = "native-runtime")]
fn storage_migration_prompt() -> serde_json::Value {
    let status = crate::web::storage::status_payload();
    let mode = status["mode"].clone();
    serde_json::json!({
        "available": mode == "yaml" && status["locked_by_env"] == serde_json::json!(false),
        "mode": mode,
    })
}

#[cfg(not(feature = "native-runtime"))]
fn storage_migration_prompt() -> serde_json::Value {
    serde_json::json!({ "available": false, "mode": "yaml" })
}

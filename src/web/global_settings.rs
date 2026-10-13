//! `GET/POST /api/global_setting`。JSON の組み立ては
//! [`crate::application::settings_view`] にあり、Worker と同じものを使う。

use axum::{extract::State, http::StatusCode, response::{IntoResponse, Json, Response}};

use crate::application::settings_view;
use crate::application::SettingsEffect;

use super::AppState;

/// GET /api/global_setting — 設定一覧（タブ・項目メタデータ・置換設定）
pub async fn get_global_settings(
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    Json(settings_view::load_view(&state.services.settings).await)
}

/// POST /api/global_setting — 設定と置換設定の保存
pub async fn save_global_settings(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let result = match settings_view::apply_save(&state.services.settings, &body).await {
        Ok(result) => result,
        Err(error) => {
            let conflict = matches!(&error, crate::application::ApplicationError::Conflict(_));
            return (if conflict { StatusCode::CONFLICT } else { StatusCode::OK }, Json(serde_json::json!({
                "success": false,
                "conflict": conflict,
                "message": error.to_string(),
            }))).into_response();
        }
    };
    let effects = &result.effects;
    let mut response = settings_view::save_response(&result);
    response["reboot"] = serde_json::json!(false);
    response["requires_restart"] = serde_json::json!(effects.contains(&SettingsEffect::StorageRestartRequired));

    if effects
        .iter()
        .any(|effect| matches!(effect, SettingsEffect::AutoScheduleChanged))
    {
        let started = crate::web::scheduler::start_or_restart_auto_update_scheduler(
            state.services.scheduler.clone(),
            state.queue.clone(),
            state.running_jobs.clone(),
            state.push_server.clone(),
            &state.auto_update_scheduler,
        );
        let message = if started {
            "自動アップデートスケジューラーを更新しました"
        } else {
            "自動アップデートスケジューラーを停止しました"
        };
        state.push_server.broadcast_echo(message, "stdout");
    }
    if effects
        .iter()
        .any(|effect| matches!(effect, SettingsEffect::WebuiConfigChanged))
    {
        state.push_server.broadcast_event("webui.config.reload", "");
    }
    if effects
        .iter()
        .any(|effect| matches!(effect, SettingsEffect::ServerSecurityChanged))
    {
        match state.reload_server_security() {
            Ok(_) => state
                .push_server
                .broadcast_echo("server-* 設定を再適用しました", "stdout"),
            Err(message) => state.push_server.broadcast_echo(
                &format!("server-* 設定の再適用に失敗しました: {message}"),
                "stdout",
            ),
        }
    }

    if result.failure.is_none() && effects.contains(&SettingsEffect::StorageRestartRequired) {
        match super::jobs::schedule_server_reboot(state.clone()).await {
            Ok(()) => {
                state.push_server.broadcast_event("reboot", "");
                response["reboot"] = serde_json::json!(true);
                response["message"] = serde_json::json!(crate::native::application::STORAGE_RESTART_REQUIRED_NOTE);
            }
            Err(error) => {
                response["message"] = serde_json::json!(
                    format!("設定を保存しました。再起動の開始に失敗したため手動で再起動してください: {error}")
                );
            }
        }
    }
    let status = if response["conflict"] == true { StatusCode::CONFLICT } else { StatusCode::OK };
    (status, Json(response)).into_response()
}

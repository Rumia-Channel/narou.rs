//! `GET/POST /api/global_setting`。JSON の組み立ては
//! [`crate::application::settings_view`] にあり、Worker と同じものを使う。

use axum::{extract::State, response::Json};

use crate::application::settings_view;
use crate::application::SettingsEffect;

use super::AppState;
use super::state::ApiResponse;

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
) -> Json<ApiResponse> {
    let effects = match settings_view::apply_save(&state.services.settings, &body).await {
        Ok(effects) => effects,
        Err(message) => {
            return Json(ApiResponse {
                success: false,
                message,
            });
        }
    };

    if effects
        .iter()
        .any(|effect| matches!(effect, SettingsEffect::AutoScheduleChanged))
    {
        let started = crate::web::scheduler::start_or_restart_auto_update_scheduler(
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

    Json(ApiResponse {
        success: true,
        message: settings_view::SAVE_MESSAGE.to_string(),
    })
}

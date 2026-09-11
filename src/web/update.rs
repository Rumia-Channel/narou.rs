//! `/api/update/start` — self-update request handling and presentation.
//!
//! Release discovery, download, archive validation, and updater process
//! management live in the native `SelfUpdateService`.  This module only
//! validates the request, translates application events to WebSocket messages,
//! and schedules the existing graceful shutdown.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use axum::{Json, extract::State, http::StatusCode};

use super::AppState;
use super::jobs::prepare_process_shutdown;
use super::state::ApiResponse;
use crate::application::{ApplicationEvent, EventSink, SelfUpdateRequest};
use crate::platform::PlatformFuture;

const PROGRESS_TOPIC: &str = "update";

#[derive(Debug, Deserialize, Default)]
pub struct UpdateStartBody {
    /// 任意: 指定すれば独自のアセット URL を使用 (デバッグ用)。
    #[serde(default)]
    pub asset_url: Option<String>,
}

#[derive(Clone)]
struct PushEventSink {
    push_server: Arc<super::push::PushServer>,
}

impl PushEventSink {
    fn new(push_server: Arc<super::push::PushServer>) -> Self {
        Self { push_server }
    }
}

impl EventSink for PushEventSink {
    fn publish<'a>(
        &'a self,
        event: ApplicationEvent,
    ) -> PlatformFuture<'a, crate::error::Result<()>> {
        Box::pin(async move {
            let target_console = event.data["target_console"]
                .as_str()
                .unwrap_or("stdout");
            match event.name.as_str() {
                "progressbar.init" => self
                    .push_server
                    .broadcast_progressbar_init_to(PROGRESS_TOPIC, target_console),
                "progressbar.step" => self.push_server.broadcast_progressbar_step_to(
                    event.data["percent"].as_f64().unwrap_or_default(),
                    event.data["topic"].as_str().unwrap_or(PROGRESS_TOPIC),
                    target_console,
                ),
                "progressbar.clear" => self.push_server.broadcast_progressbar_clear_to(
                    event.data["topic"].as_str().unwrap_or(PROGRESS_TOPIC),
                    target_console,
                ),
                "echo" => self.push_server.broadcast_echo(
                    event.data["body"].as_str().unwrap_or_default(),
                    target_console,
                ),
                "reboot" => self.push_server.broadcast_event("reboot", ""),
                name => self
                    .push_server
                    .broadcast_raw(&serde_json::json!({ "type": name, "data": event.data })),
            }
            Ok(())
        })
    }
}

pub async fn api_update_start(
    State(state): State<AppState>,
    body: Option<Json<UpdateStartBody>>,
) -> std::result::Result<Json<ApiResponse>, (StatusCode, Json<ApiResponse>)> {
    let body = body.map(|Json(body)| body).unwrap_or_default();
    if let Some(reason) = crate::version::self_update_unavailable_reason() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiResponse {
                success: false,
                message: reason.to_string(),
            }),
        ));
    }

    let events: Arc<dyn EventSink> = Arc::new(PushEventSink::new(state.push_server.clone()));
    match state
        .services
        .self_update
        .start(
            SelfUpdateRequest {
                asset_url: body.asset_url,
            },
            events,
        )
        .await
    {
        Ok(_) => {
            let shutdown_state = state.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(400)).await;
                prepare_process_shutdown(&shutdown_state).await;
                tokio::time::sleep(Duration::from_millis(200)).await;
                std::process::exit(0);
            });
            Ok(Json(ApiResponse {
                success: true,
                message: "アップデートを開始しました".to_string(),
            }))
        }
        Err(error) => {
            let message = error.to_string();
            let status = if message.contains("未対応") {
                StatusCode::BAD_REQUEST
            } else if message.contains("リリース")
                || message.contains("ダウンロード")
                || message.contains("zip")
            {
                StatusCode::BAD_GATEWAY
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            Err((
                status,
                Json(ApiResponse {
                    success: false,
                    message,
                }),
            ))
        }
    }
}

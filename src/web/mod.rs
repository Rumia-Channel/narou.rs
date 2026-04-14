pub mod batch;
pub mod frontend;
pub mod global_settings;
pub mod jobs;
pub mod misc;
pub mod novel_settings;
pub mod novels;
pub mod push;
pub mod scheduler;
pub mod state;
pub mod tags;
pub mod worker;

use axum::{
    Router,
    http::{StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use std::sync::Arc;
use tower_http::cors::CorsLayer;

#[derive(Debug, Clone)]
pub struct AppState {
    pub port: u16,
    pub ws_port: u16,
    pub push_server: Arc<push::PushServer>,
    pub basic_auth_header: Option<String>,
}

pub fn create_router(state: AppState) -> Router {
    let auth_state = state.clone();
    Router::new()
        .route("/", get(frontend::index))
        .route("/assets/{*path}", get(frontend::asset))
        .route("/api/novels/count", get(novels::novels_count))
        .route("/api/list", get(novels::api_list))
        .route("/api/version/current.json", get(misc::version_current))
        .route("/api/webui/config", get(misc::webui_config))
        .route("/api/tag_list", get(misc::tag_list))
        .route("/api/tag/change_color", post(misc::tag_change_color))
        .route("/api/novels/all_ids", get(misc::all_novel_ids))
        .route("/api/notepad/read", get(misc::notepad_read))
        .route("/api/notepad/save", post(misc::notepad_save))
        .route("/api/novels/{id}", get(novels::get_novel))
        .route("/api/novels/{id}", delete(novels::remove_novel))
        .route("/api/novels/{id}/freeze", post(novels::freeze_novel))
        .route("/api/novels/{id}/unfreeze", post(novels::unfreeze_novel))
        .route("/api/novels/{id}/tag", post(tags::add_tag))
        .route("/api/novels/{id}/tag", delete(tags::remove_tag))
        .route("/api/novels/{id}/tags", put(tags::update_tags))
        .route("/api/novels/tag", post(batch::batch_tag))
        .route("/api/novels/tag", delete(batch::batch_untag))
        .route("/api/novels/freeze", post(batch::batch_freeze))
        .route("/api/novels/unfreeze", post(batch::batch_unfreeze))
        .route("/api/novels/remove", post(batch::batch_remove))
        .route("/api/download", post(jobs::api_download))
        .route("/api/update", post(jobs::api_update))
        .route("/api/convert", post(jobs::api_convert))
        .route("/api/settings/{id}", get(novel_settings::get_settings))
        .route("/api/settings/{id}", post(novel_settings::save_settings))
        .route("/api/devices", get(novel_settings::list_devices))
        .route("/api/queue/status", get(jobs::queue_status))
        .route("/api/queue/clear", post(jobs::queue_clear))
        .route("/api/log/recent", get(misc::recent_logs))
        .route("/api/global_setting", get(global_settings::get_global_settings))
        .route("/api/global_setting", post(global_settings::save_global_settings))
        .route("/settings", get(frontend::settings_page))
        .layer(middleware::from_fn_with_state(
            auth_state,
            basic_auth_middleware,
        ))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn basic_auth_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    let Some(expected) = state.basic_auth_header.as_deref() else {
        return next.run(request).await;
    };

    let authorized = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        == Some(expected);
    if authorized {
        return next.run(request).await;
    }

    let mut response = (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        header::HeaderValue::from_static("Basic realm=\"narou.rs\""),
    );
    response
}

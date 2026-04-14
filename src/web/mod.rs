pub mod batch;
pub mod jobs;
pub mod misc;
pub mod novel_settings;
pub mod novels;
pub mod push;
pub mod state;
pub mod tags;

use axum::{
    Router,
    routing::{delete, get, post, put},
};
use std::sync::Arc;
use tower_http::cors::CorsLayer;

#[derive(Debug, Clone)]
pub struct AppState {
    pub port: u16,
    pub ws_port: u16,
    pub push_server: Arc<push::PushServer>,
}

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(novels::index))
        .route("/api/novels/count", get(novels::novels_count))
        .route("/api/list", get(novels::api_list))
        .route("/api/version/current.json", get(misc::version_current))
        .route("/api/tag_list", get(misc::tag_list))
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
        .layer(CorsLayer::permissive())
        .with_state(state)
}

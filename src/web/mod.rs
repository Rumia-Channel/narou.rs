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
    pub running_job: Arc<parking_lot::Mutex<Option<crate::queue::QueueJob>>>,
    pub running_child_pid: Arc<parking_lot::Mutex<Option<u32>>>,
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
        .route("/api/novels/{id}/tags", post(tags::add_tags))
        .route("/api/novels/{id}/tags", put(tags::update_tags))
        .route("/api/novels/{id}/tags/remove", post(tags::remove_tags))
        .route("/api/novels/tag", post(batch::batch_tag))
        .route("/api/novels/tag", delete(batch::batch_untag))
        .route("/api/novels/freeze", post(batch::batch_freeze))
        .route("/api/novels/unfreeze", post(batch::batch_unfreeze))
        .route("/api/freeze", post(batch::batch_freeze_toggle))
        .route("/api/freeze_on", post(batch::batch_freeze))
        .route("/api/freeze_off", post(batch::batch_unfreeze))
        .route("/api/novels/remove", post(batch::batch_remove))
        .route("/api/remove", post(batch::batch_remove))
        .route("/api/remove_with_file", post(batch::batch_remove_with_file))
        .route("/api/download", post(jobs::api_download))
        .route("/api/download_force", post(jobs::api_download_force))
        .route("/api/cancel", post(jobs::api_cancel))
        .route("/api/update", post(jobs::api_update))
        .route("/api/convert", post(jobs::api_convert))
        .route("/api/settings/{id}", get(novel_settings::get_settings))
        .route("/api/settings/{id}", post(novel_settings::save_settings))
        .route("/api/devices", get(novel_settings::list_devices))
        .route("/api/queue/status", get(jobs::queue_status))
        .route("/api/queue/clear", post(jobs::queue_clear))
        .route("/api/queue/cancel", post(jobs::queue_cancel))
        .route("/api/cancel_running_task", post(jobs::cancel_running_task))
        .route("/api/get_pending_tasks", get(jobs::get_pending_tasks))
        .route("/api/remove_pending_task", post(jobs::remove_pending_task))
        .route("/api/reorder_pending_tasks", post(jobs::reorder_pending_tasks))
        .route("/api/get_queue_size", get(jobs::get_queue_size))
        .route("/api/log/recent", get(misc::recent_logs))
        .route("/api/history", get(misc::console_history))
        .route("/api/clear_history", post(misc::clear_history))
        .route("/api/sort_state", get(misc::get_sort_state))
        .route("/api/sort_state", post(misc::save_sort_state))
        .route("/api/story", get(novels::get_story))
        .route("/api/global_setting", get(global_settings::get_global_settings))
        .route("/api/global_setting", post(global_settings::save_global_settings))
        .route("/api/send", post(jobs::api_send))
        .route("/api/inspect", post(jobs::api_inspect))
        .route("/api/folder", post(jobs::api_folder))
        .route("/api/backup", post(jobs::api_backup))
        .route("/api/mail", post(jobs::api_mail))
        .route("/api/setting_burn", post(jobs::api_setting_burn))
        .route("/api/diff_list", post(jobs::api_diff_list))
        .route("/api/diff", post(jobs::api_diff))
        .route("/api/diff_clean", post(jobs::api_diff_clean))
        .route("/api/csv/import", post(jobs::api_csv_import))
        .route("/api/csv/download", get(jobs::api_csv_download))
        .route("/api/validate_url_regexp_list", get(misc::validate_url_regexp_list))
        .route("/api/update_by_tag", post(jobs::api_update_by_tag))
        .route("/api/taginfo.json", post(jobs::api_taginfo))
        .route("/api/update_general_lastup", post(jobs::api_update_general_lastup))
        .route("/api/restore_pending_tasks", post(jobs::restore_pending_tasks))
        .route("/api/defer_restore_pending_tasks", post(jobs::defer_restore_pending_tasks))
        .route("/api/confirm_running_tasks", post(jobs::confirm_running_tasks))
        .route("/api/shutdown", post(jobs::api_shutdown))
        .route("/api/reboot", post(jobs::api_reboot))
        .route("/settings", get(frontend::settings_page))
        .route("/help", get(frontend::help_page))
        .route("/novels/{id}/setting", get(frontend::novel_setting_page))
        .route("/_rebooting", get(frontend::rebooting_page))
        .route("/notepad", get(frontend::notepad_page))
        .route("/widget/drag_and_drop", get(frontend::dnd_window_page))
        .route("/edit_menu", get(frontend::edit_menu_page))
        .route("/novels/{id}/author_comments", get(frontend::author_comments_page))
        .route("/novels/{id}/download", get(novels::download_ebook))
        .route("/api/novels/{id}/author_comments", get(novels::author_comments))
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

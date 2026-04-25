pub mod batch;
pub mod frontend;
pub mod global_settings;
pub mod jobs;
pub mod misc;
pub mod novel_settings;
pub mod novels;
pub mod push;
pub mod scheduler;
pub mod sort_state;
pub mod state;
mod tag_colors;
pub mod tags;
pub mod worker;

use axum::{
    Router,
    http::{HeaderMap, Method, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::AtomicBool};
use subtle::ConstantTimeEq;
use tokio::task::JoinHandle;

pub(crate) const MAX_WEB_TARGETS_PER_REQUEST: usize = 512;
pub(crate) const MAX_WEB_TAGS_PER_REQUEST: usize = 128;
pub(crate) const MAX_WEB_TARGET_LENGTH: usize = 4096;
pub(crate) const MAX_WEB_TAG_LENGTH: usize = 255;
pub(crate) const MAX_WEB_TEXT_INPUT_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_WEB_CSV_IMPORT_BYTES: usize = 5 * 1024 * 1024;
pub(crate) const MAX_WEB_LOG_COUNT: usize = 1000;
pub(crate) const MAX_WEB_PAGE_LENGTH: u64 = 500;
pub(crate) const MAX_WEB_SEARCH_BYTES: usize = 4096;
pub const INTERNAL_CONTROL_HEADER: &str = "x-narou-internal-token";

#[derive(Debug, Clone)]
pub struct AppState {
    pub port: u16,
    pub ws_port: u16,
    pub push_server: Arc<push::PushServer>,
    pub basic_auth_header: Option<String>,
    pub control_token: String,
    pub allowed_request_hosts: Vec<String>,
    pub reverse_proxy_mode: bool,
    pub queue: Arc<crate::queue::PersistentQueue>,
    pub restore_prompt_pending: Arc<AtomicBool>,
    pub restorable_tasks_available: Arc<AtomicBool>,
    pub running_jobs: Arc<parking_lot::Mutex<Vec<crate::queue::QueueJob>>>,
    pub running_child_pids: Arc<parking_lot::Mutex<std::collections::HashMap<String, u32>>>,
    pub cancelled_job_ids: Arc<parking_lot::Mutex<HashSet<String>>>,
    pub auto_update_scheduler: Arc<parking_lot::Mutex<Option<JoinHandle<()>>>>,
}

pub(crate) fn non_external_console_target() -> &'static str {
    if crate::compat::load_local_setting_bool("concurrency") {
        "stdout2"
    } else {
        "stdout"
    }
}

pub(crate) fn normalize_web_device_override(value: Option<&str>) -> Result<Option<String>, String> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let normalized = value.to_ascii_lowercase();
    match normalized.as_str() {
        "text" | "kindle" | "kobo" | "epub" | "ibunko" | "reader" | "ibooks" => {
            Ok(Some(normalized))
        }
        _ => Err("invalid device".to_string()),
    }
}

#[allow(dead_code)]
pub(crate) fn removal_log_message(titles: &[String], with_file: bool) -> String {
    let suffix = if with_file {
        "（保存フォルダも削除）"
    } else {
        ""
    };
    match titles {
        [] => format!("削除対象の小説はありません{}", suffix),
        [title] => format!("小説を削除しました{}: {}", suffix, title),
        _ => {
            let shown = titles
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            let tail = if titles.len() > 5 { ", ..." } else { "" };
            format!(
                "小説を{}件削除しました{}: {}{}",
                titles.len(),
                suffix,
                shown,
                tail
            )
        }
    }
}

pub(crate) fn validate_web_target_value(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("target is required".to_string());
    }
    if trimmed.len() > MAX_WEB_TARGET_LENGTH {
        return Err("target is too long".to_string());
    }
    if trimmed.starts_with('-') {
        return Err("invalid target".to_string());
    }
    if trimmed.chars().any(|ch| ch.is_control()) {
        return Err("target contains invalid characters".to_string());
    }
    Ok(trimmed.to_string())
}

pub(crate) fn validate_web_tag_name(tag: &str) -> Result<String, String> {
    let trimmed = tag.trim();
    if trimmed.is_empty() {
        return Err("tag is required".to_string());
    }
    if trimmed.starts_with('-') {
        return Err("tag contains invalid characters".to_string());
    }
    if trimmed.len() > MAX_WEB_TAG_LENGTH {
        return Err("tag is too long".to_string());
    }
    if trimmed.chars().any(|ch| ch.is_control()) {
        return Err("tag contains invalid characters".to_string());
    }
    Ok(trimmed.to_string())
}

pub(crate) fn normalize_web_tag_name(tag: &str) -> Result<String, String> {
    let trimmed = tag.trim();
    let stripped = trimmed.strip_prefix("tag:").unwrap_or(trimmed);
    validate_web_tag_name(stripped)
}

pub(crate) fn validate_web_text_size(
    value: &str,
    max_bytes: usize,
    label: &str,
) -> Result<(), String> {
    if value.len() > max_bytes {
        return Err(format!("{} is too large", label));
    }
    Ok(())
}

pub(crate) fn safe_existing_novel_dir(
    archive_root: &Path,
    record: &crate::db::novel_record::NovelRecord,
) -> Result<PathBuf, String> {
    let candidate = crate::db::existing_novel_dir_for_record(archive_root, record);
    reject_symlink_ancestors(&candidate, archive_root)?;
    crate::db::paths::ensure_within_archive_root(&candidate, archive_root)
        .map_err(|_| "invalid novel storage path".to_string())
}

fn reject_symlink_ancestors(path: &Path, root: &Path) -> Result<(), String> {
    let mut current = root.to_path_buf();
    if current.exists() && is_symlink_like(&current)? {
        return Err("invalid novel storage path".to_string());
    }
    let remainder = path
        .strip_prefix(root)
        .map_err(|_| "invalid novel storage path".to_string())?;
    for component in remainder.components() {
        current.push(component.as_os_str());
        if !current.exists() {
            break;
        }
        if is_symlink_like(&current)? {
            return Err("invalid novel storage path".to_string());
        }
    }
    Ok(())
}

fn is_symlink_like(path: &Path) -> Result<bool, String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        Ok(metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
    }
    #[cfg(not(windows))]
    {
        Ok(metadata.file_type().is_symlink())
    }
}

#[allow(dead_code)]
pub(crate) fn remove_novel_storage_dir(
    archive_root: &Path,
    record: &crate::db::novel_record::NovelRecord,
) -> Result<(), String> {
    let dir = safe_existing_novel_dir(archive_root, record)?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub(crate) fn basic_auth_matches(headers: &HeaderMap, expected: Option<&str>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    let Some(actual) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    constant_time_str_eq(actual, expected)
}

pub(crate) fn request_host_allowed(
    headers: &HeaderMap,
    state: &AppState,
    expected_port: u16,
) -> bool {
    request_host_allowed_for_ports(headers, state, &[expected_port])
}

pub(crate) fn request_host_allowed_for_ports(
    headers: &HeaderMap,
    state: &AppState,
    expected_ports: &[u16],
) -> bool {
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    if state.reverse_proxy_mode {
        return parse_authority_host_and_port(host, false).is_some();
    }
    authority_matches_state(host, state, expected_ports, false)
}

pub(crate) fn origin_allowed(headers: &HeaderMap, state: &AppState, expected_port: u16) -> bool {
    origin_allowed_for_ports(headers, state, &[expected_port])
}

pub(crate) fn origin_allowed_for_ports(
    headers: &HeaderMap,
    state: &AppState,
    expected_ports: &[u16],
) -> bool {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .or_else(|| {
            headers
                .get(header::REFERER)
                .and_then(|value| value.to_str().ok())
        });
    let Some(origin) = origin else {
        return false;
    };
    if state.reverse_proxy_mode {
        return origin_matches_forwarded_host(headers, origin);
    }
    authority_matches_state(origin, state, expected_ports, true)
}

fn internal_control_token_matches(headers: &HeaderMap, state: &AppState) -> bool {
    let Some(actual) = headers
        .get(INTERNAL_CONTROL_HEADER)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    constant_time_str_eq(actual, state.control_token.as_str())
}

fn authority_matches_state(
    value: &str,
    state: &AppState,
    expected_ports: &[u16],
    is_url: bool,
) -> bool {
    let Some((host, port)) = parse_authority_host_and_port(value, is_url) else {
        return false;
    };
    if !host_allowed(host.as_str(), state) {
        return false;
    }
    match port {
        Some(port) => expected_ports.contains(&port),
        None => true,
    }
}

fn parse_authority_host_and_port(value: &str, is_url: bool) -> Option<(String, Option<u16>)> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
        return None;
    }
    let authority = if is_url {
        let (_, rest) = trimmed.split_once("://")?;
        rest.split(['/', '?', '#']).next()?
    } else {
        trimmed
    };
    split_host_and_port(authority)
}

fn split_host_and_port(authority: &str) -> Option<(String, Option<u16>)> {
    let trimmed = authority.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = rest[..end].to_ascii_lowercase();
        let tail = &rest[end + 1..];
        let port = tail
            .strip_prefix(':')
            .and_then(|value| value.parse::<u16>().ok());
        return Some((host, port));
    }
    if let Some((host, port)) = trimmed.rsplit_once(':') {
        if !host.contains(':') {
            let host = host.trim().to_ascii_lowercase();
            if host.is_empty() {
                return None;
            }
            let port = port.parse::<u16>().ok();
            return Some((host, port));
        }
    }
    Some((trimmed.to_ascii_lowercase(), None))
}

fn host_allowed(host: &str, state: &AppState) -> bool {
    let normalized = normalize_host_name(host);
    !normalized.is_empty()
        && state
            .allowed_request_hosts
            .iter()
            .any(|allowed| normalize_host_name(allowed) == normalized)
}

fn normalize_host_name(host: &str) -> String {
    host.trim().trim_matches('.').to_ascii_lowercase()
}

fn origin_matches_forwarded_host(headers: &HeaderMap, origin: &str) -> bool {
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Some((origin_host, origin_port)) = parse_authority_host_and_port(origin, true) else {
        return false;
    };
    let Some((host_name, host_port)) = parse_authority_host_and_port(host, false) else {
        return false;
    };
    if normalize_host_name(&origin_host) != normalize_host_name(&host_name) {
        return false;
    }
    match (origin_port, host_port) {
        (Some(origin_port), Some(host_port)) => origin_port == host_port,
        _ => true,
    }
}

fn constant_time_str_eq(actual: &str, expected: &str) -> bool {
    bool::from(actual.as_bytes().ct_eq(expected.as_bytes()))
}

fn current_hostname() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .map(|name| normalize_host_name(&name))
        .filter(|name| !name.is_empty())
}

pub fn default_allowed_request_hosts(bind_host: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    let normalized_bind = normalize_host_name(bind_host);
    if !normalized_bind.is_empty() && !matches!(normalized_bind.as_str(), "0.0.0.0" | "::") {
        hosts.push(normalized_bind);
    }
    hosts.extend([
        "127.0.0.1".to_string(),
        "localhost".to_string(),
        "::1".to_string(),
    ]);
    if let Some(hostname) = current_hostname() {
        hosts.push(hostname);
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

#[cfg(test)]
fn wildcard_host_match(pattern: &str, text: &str) -> bool {
    wildcard_host_match_bytes(pattern.as_bytes(), text.as_bytes())
}

#[cfg(test)]
fn wildcard_host_match_bytes(pattern: &[u8], text: &[u8]) -> bool {
    if pattern.is_empty() {
        return text.is_empty();
    }
    match pattern[0] {
        b'*' => {
            wildcard_host_match_bytes(&pattern[1..], text)
                || (!text.is_empty() && wildcard_host_match_bytes(pattern, &text[1..]))
        }
        b'?' => !text.is_empty() && wildcard_host_match_bytes(&pattern[1..], &text[1..]),
        c => {
            !text.is_empty() && c == text[0] && wildcard_host_match_bytes(&pattern[1..], &text[1..])
        }
    }
}

/// Validates that a wildcard pattern is safe against domain-boundary bypasses.
/// Returns false for patterns like `*.com`, `*`, or `example.com*` that would
/// match unintended domains.
#[cfg(test)]
fn is_safe_wildcard_pattern(pattern: &str) -> bool {
    let trimmed = pattern.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Reject bare `*` — would match every domain
    if trimmed == "*" {
        return false;
    }
    // Reject patterns containing `*` anywhere except as a leading subdomain wildcard.
    // Safe: `*.example.com`
    // Unsafe: `example.com*`, `*example.com`, `sub*.example.com`
    if let Some(pos) = trimmed.find('*') {
        if pos != 0 {
            return false;
        }
        // `*.example.com` is only safe if there are at least two dot-separated
        // labels after the `*` (e.g. `.example.com`). `*.com` has only one.
        let after = &trimmed[1..];
        let dot_count = after.chars().filter(|&c| c == '.').count();
        if dot_count < 2 {
            return false;
        }
    }
    true
}

/// Ensures that a `*.domain` wildcard matches exactly one subdomain level.
/// `*.example.com` matches `sub.example.com` but NOT `sub.sub.example.com`
/// nor `evil.example.com.attacker.com`.
#[cfg(test)]
fn is_exact_subdomain_wildcard_match(pattern: &str, text: &str) -> bool {
    if !pattern.starts_with("*.") {
        return true;
    }
    let pattern_labels = pattern.split('.').count();
    let text_labels = text.split('.').count();
    text_labels == pattern_labels
}

fn is_state_changing_method(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

pub fn create_router(state: AppState) -> Router {
    let auth_state = state.clone();
    let guard_state = state.clone();
    Router::new()
        .route("/", get(frontend::index))
        .route("/ws", get(push::ws_handler_with_app_state))
        .route("/assets/{*path}", get(frontend::asset))
        .route("/api/novels/count", get(novels::novels_count))
        .route(
            "/api/list",
            get(novels::api_list).post(novels::api_list_post),
        )
        .route("/api/version/current.json", get(misc::version_current))
        .route("/api/version/latest.json", get(misc::version_latest))
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
        .route(
            "/api/reorder_pending_tasks",
            post(jobs::reorder_pending_tasks),
        )
        .route("/api/get_queue_size", get(jobs::get_queue_size))
        .route("/api/log/recent", get(misc::recent_logs))
        .route("/api/history", get(misc::console_history))
        .route("/api/clear_history", post(misc::clear_history))
        .route("/api/sort_state", get(misc::get_sort_state))
        .route("/api/sort_state", post(misc::save_sort_state))
        .route("/api/story", get(novels::get_story))
        .route(
            "/api/global_setting",
            get(global_settings::get_global_settings),
        )
        .route(
            "/api/global_setting",
            post(global_settings::save_global_settings),
        )
        .route("/api/send", post(jobs::api_send))
        .route("/api/inspect", post(jobs::api_inspect))
        .route("/api/folder", post(jobs::api_folder))
        .route("/api/backup", post(jobs::api_backup))
        .route("/api/backup_bookmark", post(jobs::api_backup_bookmark))
        .route("/api/mail", post(jobs::api_mail))
        .route("/api/setting_burn", post(jobs::api_setting_burn))
        .route(
            "/api/diff_list",
            get(jobs::api_diff_list_get).post(jobs::api_diff_list),
        )
        .route("/api/diff", post(jobs::api_diff))
        .route("/api/diff_clean", post(jobs::api_diff_clean))
        .route("/api/csv/import", post(jobs::api_csv_import))
        .route("/api/csv/download", get(jobs::api_csv_download))
        .route(
            "/api/validate_url_regexp_list",
            get(misc::validate_url_regexp_list),
        )
        .route("/api/edit_tag", post(tags::edit_tag))
        .route("/api/update_by_tag", post(jobs::api_update_by_tag))
        .route("/api/taginfo.json", post(jobs::api_taginfo))
        .route(
            "/api/update_general_lastup",
            post(jobs::api_update_general_lastup),
        )
        .route(
            "/api/download_request",
            get(jobs::bookmarklet_download_request_post_required).post(jobs::api_download_request),
        )
        .route("/api/downloadable.gif", get(jobs::api_downloadable_gif))
        .route(
            "/api/download4ssl",
            get(jobs::bookmarklet_download4ssl_post_required).post(jobs::api_download4ssl),
        )
        .route(
            "/api/restore_pending_tasks",
            post(jobs::restore_pending_tasks),
        )
        .route(
            "/api/defer_restore_pending_tasks",
            post(jobs::defer_restore_pending_tasks),
        )
        .route(
            "/api/confirm_running_tasks",
            post(jobs::confirm_running_tasks),
        )
        .route("/api/shutdown", post(jobs::api_shutdown))
        .route("/api/reboot", post(jobs::api_reboot))
        .route("/settings", get(frontend::settings_page))
        .route("/help", get(frontend::help_page))
        .route("/about", get(frontend::about_page))
        .route("/bookmarklet", get(frontend::bookmarklet_page))
        .route("/novels/{id}/setting", get(frontend::novel_setting_page))
        .route("/_rebooting", get(frontend::rebooting_page))
        .route("/notepad", get(frontend::notepad_page))
        .route("/widget/drag_and_drop", get(frontend::dnd_window_page))
        .route("/edit_menu", get(frontend::edit_menu_page))
        .route(
            "/novels/{id}/author_comments",
            get(frontend::author_comments_page),
        )
        .route("/novels/{id}/download", get(novels::download_ebook))
        .route(
            "/api/novels/{id}/author_comments",
            get(novels::author_comments),
        )
        .layer(middleware::from_fn_with_state(
            auth_state,
            basic_auth_middleware,
        ))
        .layer(middleware::from_fn_with_state(
            guard_state,
            request_guard_middleware,
        ))
        .with_state(state)
}

async fn request_guard_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    if !request_host_allowed(request.headers(), &state, state.port) {
        return (StatusCode::BAD_REQUEST, "Invalid Host").into_response();
    }
    if is_state_changing_method(request.method())
        && !internal_control_token_matches(request.headers(), &state)
        && !origin_allowed(request.headers(), &state, state.port)
    {
        return (StatusCode::FORBIDDEN, "Forbidden").into_response();
    }
    next.run(request).await
}

async fn basic_auth_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    if state.basic_auth_header.is_none() {
        return next.run(request).await;
    }
    if basic_auth_matches(request.headers(), state.basic_auth_header.as_deref()) {
        return next.run(request).await;
    }

    let mut response = (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        header::HeaderValue::from_static("Basic realm=\"narou.rs\""),
    );
    response
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use std::sync::{Arc, atomic::AtomicBool};

    use super::{
        AppState, basic_auth_matches, is_exact_subdomain_wildcard_match, is_safe_wildcard_pattern,
        origin_allowed, origin_allowed_for_ports, removal_log_message, request_host_allowed, safe_existing_novel_dir,
        validate_web_tag_name, wildcard_host_match,
    };

    fn sample_record(file_title: &str) -> crate::db::novel_record::NovelRecord {
        crate::db::novel_record::NovelRecord {
            id: 1,
            author: "author".to_string(),
            title: "title".to_string(),
            file_title: file_title.to_string(),
            toc_url: "https://example.com".to_string(),
            sitename: "site".to_string(),
            novel_type: 0,
            end: false,
            last_update: Utc::now(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: Vec::new(),
            ncode: None,
            domain: None,
            general_all_no: None,
            length: None,
            suspend: false,
            is_narou: false,
            last_check_date: None,
            convert_failure: false,
            extra_fields: Default::default(),
        }
    }

    fn test_artifact_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("test-artifacts")
            .join(format!("web-mod-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_state() -> AppState {
        let queue_dir = test_artifact_dir("security-state");
        let mut push_server = crate::web::push::PushServer::new();
        push_server.set_accepted_domains(["127.0.0.1", "localhost", "*.example.com"]);
        AppState {
            port: 8080,
            ws_port: 8081,
            push_server: Arc::new(push_server),
            basic_auth_header: Some("Basic dXNlcjpwYXNz".to_string()),
            control_token: "control-token".to_string(),
            allowed_request_hosts: vec!["127.0.0.1".to_string(), "localhost".to_string()],
            reverse_proxy_mode: false,
            queue: Arc::new(
                crate::queue::PersistentQueue::new(&queue_dir.join("queue.yaml")).unwrap(),
            ),
            restore_prompt_pending: Arc::new(AtomicBool::new(false)),
            restorable_tasks_available: Arc::new(AtomicBool::new(false)),
            running_jobs: Arc::new(parking_lot::Mutex::new(Vec::new())),
            running_child_pids: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
            cancelled_job_ids: Arc::new(parking_lot::Mutex::new(std::collections::HashSet::new())),
            auto_update_scheduler: Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    #[test]
    fn removal_log_message_formats_single_title() {
        assert_eq!(
            removal_log_message(&["title".to_string()], false),
            "小説を削除しました: title"
        );
    }

    #[test]
    fn removal_log_message_formats_multiple_titles_with_file_suffix() {
        assert_eq!(
            removal_log_message(&["a".to_string(), "b".to_string()], true),
            "小説を2件削除しました（保存フォルダも削除）: a, b"
        );
    }

    #[test]
    fn validate_web_tag_name_rejects_control_characters() {
        assert!(validate_web_tag_name("tag").is_ok());
        assert!(validate_web_tag_name("bad\ttag").is_err());
    }

    #[test]
    fn safe_existing_novel_dir_keeps_suspicious_names_inside_archive_root() {
        let archive_root = test_artifact_dir("safe-existing-novel-dir");
        let path =
            safe_existing_novel_dir(&archive_root, &sample_record("..\\..\\escape")).unwrap();
        assert!(path.starts_with(&archive_root));
        assert!(path.ends_with(".._.._escape"));
        let _ = std::fs::remove_dir_all(archive_root);
    }

    #[test]
    fn basic_auth_matching_requires_expected_header() {
        let headers = axum::http::HeaderMap::new();
        assert!(basic_auth_matches(&headers, None));

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Basic dXNlcjpwYXNz"),
        );
        assert!(basic_auth_matches(&headers, Some("Basic dXNlcjpwYXNz")));
        assert!(!basic_auth_matches(&headers, Some("Basic other")));
    }

    #[test]
    fn request_host_validation_rejects_unexpected_domains() {
        let state = test_state();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("127.0.0.1:8080"),
        );
        assert!(request_host_allowed(&headers, &state, state.port));

        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("evil.test:8080"),
        );
        assert!(!request_host_allowed(&headers, &state, state.port));

        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("192.168.1.50:8080"),
        );
        assert!(!request_host_allowed(&headers, &state, state.port));
    }

    #[test]
    fn origin_validation_requires_same_host_and_port() {
        let state = test_state();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::ORIGIN,
            axum::http::HeaderValue::from_static("http://localhost:8080"),
        );
        assert!(origin_allowed(&headers, &state, state.port));

        headers.insert(
            axum::http::header::ORIGIN,
            axum::http::HeaderValue::from_static("http://localhost:9090"),
        );
        assert!(!origin_allowed(&headers, &state, state.port));

        headers.insert(
            axum::http::header::ORIGIN,
            axum::http::HeaderValue::from_static("http://evil.test:8080"),
        );
        assert!(!origin_allowed(&headers, &state, state.port));
    }

    #[test]
    fn multi_port_origin_validation_accepts_ws_port() {
        let state = test_state();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::ORIGIN,
            axum::http::HeaderValue::from_static("http://localhost:8081"),
        );
        assert!(origin_allowed_for_ports(
            &headers,
            &state,
            &[state.port, state.ws_port]
        ));
        assert!(!origin_allowed(&headers, &state, state.port));
    }

    #[test]
    fn reverse_proxy_mode_accepts_forwarded_same_origin_hosts() {
        let mut state = test_state();
        state.reverse_proxy_mode = true;
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("narou.example.com:8443"),
        );
        assert!(request_host_allowed(&headers, &state, state.port));
        headers.insert(
            axum::http::header::ORIGIN,
            axum::http::HeaderValue::from_static("https://narou.example.com:8443"),
        );
        assert!(origin_allowed(&headers, &state, state.port));

        headers.insert(
            axum::http::header::ORIGIN,
            axum::http::HeaderValue::from_static("https://evil.example.com:8443"),
        );
        assert!(!origin_allowed(&headers, &state, state.port));
    }

    #[test]
    fn wildcard_host_match_rejects_subdomain_bypass() {
        // `*.example.com` must NOT match `evil.example.com.attacker.com`
        assert!(!wildcard_host_match(
            "*.example.com",
            "evil.example.com.attacker.com"
        ));
        // `*.example.com` must match `sub.example.com`
        assert!(wildcard_host_match("*.example.com", "sub.example.com"));
        // Bare `*` must be rejected by is_safe_wildcard_pattern
        assert!(!is_safe_wildcard_pattern("*"));
        // `*.com` must be rejected (only one label after `*.`)
        assert!(!is_safe_wildcard_pattern("*.com"));
        // `*.co.jp` is accepted (two labels after `*.`)
        assert!(is_safe_wildcard_pattern("*.co.jp"));
        // Trailing wildcard (`example.com*`) must be rejected
        assert!(!is_safe_wildcard_pattern("example.com*"));
        // Embedded wildcard (`sub*.example.com`) must be rejected
        assert!(!is_safe_wildcard_pattern("sub*.example.com"));
        // Exact match still works
        assert!(is_safe_wildcard_pattern("localhost"));
        assert!(wildcard_host_match("localhost", "localhost"));
        // Exact subdomain level enforcement
        assert!(!is_exact_subdomain_wildcard_match(
            "*.example.com",
            "sub.sub.example.com"
        ));
        assert!(!is_exact_subdomain_wildcard_match(
            "*.example.com",
            "evil.example.com.attacker.com"
        ));
        assert!(is_exact_subdomain_wildcard_match(
            "*.example.com",
            "sub.example.com"
        ));
    }
}

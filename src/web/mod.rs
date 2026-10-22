pub mod batch;
pub mod authors;
pub mod sites;
pub mod feature_tour;
pub mod frontend;
pub mod global_settings;
pub mod jobs;
pub mod library_backup;
pub mod login;
pub mod misc;
pub mod novel_settings;
pub mod novels;
pub mod push;
pub mod scheduler;
pub mod server_security;
pub mod sort_state;
pub mod state;
pub mod storage;
pub mod tags;
pub mod update;
pub mod worker;

use axum::{
    Router,
    extract::DefaultBodyLimit,
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

// 可搬の Web UI ヘルパ (バリデーション・ソート状態・上限値) は Worker と共有する
// ため `crate::application::webui` が唯一の定義。ここでは名前を再エクスポートする。
pub(crate) use crate::application::webui::{
    MAX_WEB_TAGS_PER_REQUEST, MAX_WEB_TARGETS_PER_REQUEST,
    normalize_web_device_override, normalize_web_tag_name, targets_to_strings,
    validate_web_tag_name, validate_web_target_value,
};

/// Effective per-request target cap, resolved through the application
/// settings port.
pub(crate) async fn max_web_targets_per_request(state: &AppState) -> usize {
    state
        .services
        .settings
        .web_target_limit(MAX_WEB_TARGETS_PER_REQUEST)
        .await
}
pub(crate) use crate::application::settings_view::MAX_WEB_TEXT_INPUT_BYTES;
pub(crate) const MAX_WEB_CSV_IMPORT_BYTES: usize = 5 * 1024 * 1024;
/// `/api/csv/import` が受け取る HTTP ボディ全体の上限。
///
/// `CsvImportBody { csv }` は JSON なので、5 MiB の CSV 本体に JSON の
/// エスケープ (`\\n` 等で最大およそ 2 倍) とフレーム分の余裕を足す。
/// これより小さいと axum 既定の 2 MiB 制限 (413 "length limit exceeded")
/// が先に効き、アプリ側の「csv is too large」判定に到達しない。
pub(crate) const MAX_WEB_CSV_IMPORT_BODY_BYTES: usize =
    2 * MAX_WEB_CSV_IMPORT_BYTES + 4 * 1024;
pub(crate) const MAX_WEB_LOG_COUNT: usize = 1000;
pub const INTERNAL_CONTROL_HEADER: &str = "x-narou-internal-token";

/// ブラウザに配るローカル UI トークンの Cookie 名。
///
/// loopback バインド時は OS 上どのローカルプロセスからも HTTP ポートへ
/// 到達できるため、`Host` / `Origin` だけでは別ユーザーからの操作を防げない。
/// `narou web` 起動時に生成/永続化したトークンを、起動時の URL から一度だけ
/// ブラウザへ渡して HttpOnly Cookie に変換し、以後の保護 API で照合する。
pub const UI_AUTH_COOKIE: &str = "narou_ui_token";
/// 起動時にブラウザへ渡すトークンの query パラメータ名。
pub const UI_AUTH_QUERY_PARAM: &str = "narou_ui_token";

#[derive(Clone)]
pub struct AppState {
    pub port: u16,
    pub ws_port: u16,
    pub push_server: Arc<push::PushServer>,
    pub services: Arc<crate::application::AppServices>,
    /// Runtime-mutable `server-*` settings (basic-auth header, Host allow-list,
    /// reverse-proxy mode). Re-applied by `AppState::reload_server_security`.
    pub server_security: Arc<parking_lot::RwLock<server_security::ServerSecurity>>,
    /// Host the listeners were bound to; seeds `default_allowed_request_hosts`
    /// when the security settings are reloaded.
    pub bind_host: Arc<str>,
    /// CLI / 再起動・終了制御用のトークン。`x-narou-internal-token` ヘッダで送る。
    pub control_token: String,
    /// loopback ブラウザ UI 用のトークン。Cookie またはヘッダで送る。
    pub ui_token: String,
    /// 起動時の URL にだけ載せる一度きりのブートストラップ用チケット。
    /// Cookie へ変換したら無効化する (ブラウザのプロセス引数から後から
    /// 読み取られても再利用できない)。
    pub ui_bootstrap_ticket: String,
    /// ブートストラップチケットを消費済みか。
    pub ui_bootstrap_consumed: Arc<AtomicBool>,
    pub queue: Arc<crate::queue::PersistentQueue>,
    pub restore_prompt_pending: Arc<AtomicBool>,
    pub restorable_tasks_available: Arc<AtomicBool>,
    pub running_jobs: Arc<parking_lot::Mutex<Vec<crate::queue::QueueJob>>>,
    pub running_child_pids: Arc<parking_lot::Mutex<std::collections::HashMap<String, u32>>>,
    pub cancelled_job_ids: Arc<parking_lot::Mutex<HashSet<String>>>,
    pub library_backup: Arc<library_backup::LibraryBackupState>,
    pub auto_update_scheduler: Arc<parking_lot::Mutex<Option<JoinHandle<()>>>>,
}

#[cfg(test)]
pub(crate) fn default_app_services(
    novels: Arc<dyn crate::platform::NovelRepository>,
) -> Arc<crate::application::AppServices> {
    let objects: Arc<dyn crate::platform::ObjectStore> =
        Arc::new(crate::platform::mocks::MemoryObjectStore::new());
    let freeze_store: Arc<dyn crate::application::FreezeStore> =
        Arc::new(crate::application::SystemFreezeStore);
    let library = Arc::new(crate::application::LibraryService::new(
        novels.clone(),
        Arc::new(crate::platform::clock::SystemClock),
        freeze_store.clone(),
        Arc::new(crate::application::EmptySiteTimezoneProvider),
    ));
    let actions = Arc::new(crate::application::NovelActionService::new(
        novels.clone(),
        freeze_store,
        Arc::new(crate::application::NoopFreezeMutationStore),
        Some(objects.clone()),
    ));
    let settings_store: Arc<dyn crate::application::SettingsStore> =
        Arc::new(crate::application::MemorySettingsStore::new());
    let settings = Arc::new(crate::application::SettingsService::new(settings_store));
    let novel_settings = Arc::new(crate::application::NovelSettingsService::new(
        novels.clone(),
        objects.clone(),
    ));
    let content = Arc::new(crate::application::NovelContentService::new(
        novels,
        objects,
    ));
    let tag_colors = Arc::new(crate::application::TagColorService::new(Arc::new(
        crate::application::MemoryTagColorStore::default(),
    )));
    let scheduler = Arc::new(crate::application::SchedulerService::new(Arc::new(
        crate::platform::clock::SystemClock,
    )));
    Arc::new(crate::application::AppServices::new(
        crate::application::AppServiceDependencies {
            library,
            novel_actions: actions,
            novel_settings,
            content,
            settings,
            tag_colors,
            jobs: Arc::new(crate::application::JobService),
            scheduler,
            site_definitions: Arc::new(crate::application::EmptySiteDefinitionProvider),
            self_update: Arc::new(crate::application::NoopSelfUpdateService),
            web_actions: Arc::new(crate::application::EmptyWebActionService),
        },
    ))
}
#[cfg(test)]
pub(crate) fn test_server_security(
    basic_auth_header: Option<&str>,
    allowed_request_hosts: Vec<String>,
    reverse_proxy_mode: bool,
) -> Arc<parking_lot::RwLock<server_security::ServerSecurity>> {
    Arc::new(parking_lot::RwLock::new(server_security::ServerSecurity {
        basic_auth_header: basic_auth_header.map(str::to_string),
        allowed_request_hosts,
        accepted_ws_domains: Vec::new(),
        reverse_proxy_mode,
        require_basic_auth_for_external_bind: true,
    }))
}

pub(crate) async fn configured_tag_color(state: &AppState) -> Option<String> {
    state
        .services
        .settings
        .get(crate::tag_colors::NEW_TAG_COLOR_SETTING)
        .await
        .ok()
        .flatten()
        .and_then(|value| value.as_str().map(str::to_owned))
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| crate::tag_colors::is_valid_tag_color(value))
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

pub(crate) fn validate_web_text_size(
    content: &str,
    limit: usize,
    label: &str,
) -> Result<(), String> {
    crate::application::settings_view::validate_web_text_size(content, limit, label)
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
    // reverse proxy モードでも Host は許可リストで検証する。プロキシ配下の
    // 公開ホストは `server-add-accepted-hosts` に明示する。公開ポートは
    // 内部ポートと一致しないことがあるため、proxy モードだけ port を問わない。
    if state.server_security.read().reverse_proxy_mode {
        let Some((host, _port)) = parse_authority_host_and_port(host, false) else {
            return false;
        };
        return host_allowed(&host, state);
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
    if state.server_security.read().reverse_proxy_mode {
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
    if is_url {
        let url = url::Url::parse(trimmed).ok()?;
        if !matches!(url.scheme(), "http" | "https") || !url.username().is_empty() || url.password().is_some() {
            return None;
        }
        return Some((url.host_str()?.to_ascii_lowercase(), Some(url.port_or_known_default()?)));
    }
    split_host_and_port(trimmed)
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
        let port = if tail.is_empty() { None } else { Some(tail.strip_prefix(':')?.parse::<u16>().ok()?) };
        return Some((host, port));
    }
    if let Some((host, port)) = trimmed.rsplit_once(':')
        && !host.contains(':') {
            let host = host.trim().to_ascii_lowercase();
            if host.is_empty() {
                return None;
            }
            let port = Some(port.parse::<u16>().ok()?);
            return Some((host, port));
        }
    Some((trimmed.to_ascii_lowercase(), None))
}

fn host_allowed(host: &str, state: &AppState) -> bool {
    let normalized = normalize_host_name(host);
    if normalized.is_empty() {
        return false;
    }
    state.server_security.read().allowed_request_hosts.iter().any(|allowed| {
        let pattern = normalize_host_name(allowed);
        if pattern == normalized {
            return true;
        }
        if !is_safe_wildcard_pattern(&pattern) {
            return false;
        }
        is_exact_subdomain_wildcard_match(&pattern, &normalized)
            && wildcard_host_match(&pattern, &normalized)
    })
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
    // An omitted URL port means 80/443, never "any port". The public Host
    // without a port likewise denotes the default for the origin's scheme.
    let default_port = url::Url::parse(origin).ok().and_then(|url| match url.scheme() {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    });
    origin_port.is_some() && origin_port == host_port.or(default_port)
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

fn wildcard_host_match(pattern: &str, text: &str) -> bool {
    wildcard_host_match_bytes(pattern.as_bytes(), text.as_bytes())
}

fn wildcard_host_match_bytes(pattern: &[u8], text: &[u8]) -> bool {
    if pattern.is_empty() {
        return text.is_empty();
    }
    match pattern[0] {
        b'*' => {
            wildcard_host_match_bytes(&pattern[1..], text)
                || (!text.is_empty() && wildcard_host_match_bytes(pattern, &text[1..]))
        }
        // `?` はグロブにしない。hostname に `?` は現れないのでリテラル一致のみ。
        c => {
            !text.is_empty() && c == text[0] && wildcard_host_match_bytes(&pattern[1..], &text[1..])
        }
    }
}

/// Validates that a wildcard pattern is safe against domain-boundary bypasses.
/// Returns false for patterns like `*.com`, `*`, or `example.com*` that would
/// match unintended domains.
pub fn is_safe_wildcard_pattern(pattern: &str) -> bool {
    let trimmed = pattern.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Reject bare `*` — would match every domain
    if trimmed == "*" {
        return false;
    }
    // `?` はサポートしない (hostname に `?` は存在しないため、グロブとして
    // 働く余地を残さない)。
    if trimmed.contains('?') {
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
pub fn is_exact_subdomain_wildcard_match(pattern: &str, text: &str) -> bool {
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
    let local_auth_state = state.clone();
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
        .route("/api/feature_tour/pending", get(feature_tour::pending))
        .route("/api/feature_tour/all", get(feature_tour::all))
        .route("/api/feature_tour/seen", post(feature_tour::mark_seen))
        .route("/api/feature_tour/config", post(feature_tour::configure))
        .route("/api/storage/mode", get(storage::storage_mode_get).post(storage::storage_mode_set))
        .route("/api/webui/config", get(misc::webui_config))
        .route("/api/tag_list", get(misc::tag_list))
        .route("/api/tag/change_color", post(misc::tag_change_color))
        .route("/api/novels/all_ids", get(misc::all_novel_ids))
        .route("/api/sites", get(sites::sites_list))
        .route(
            "/api/sites/{name}",
            get(sites::site_show)
                .put(sites::site_put)
                .delete(sites::site_delete),
        )
        .route("/api/login", get(login::login_status))
        .route("/api/login", delete(login::login_clear_all))
        .route("/api/authors", get(authors::list).post(authors::add).delete(authors::remove))
        .route("/api/authors/check", post(authors::check))
        .route("/api/login/import", post(login::login_import))
        .route("/api/login/rename", post(login::login_rename))
        .route("/api/login/order", post(login::login_order))
        .route("/api/login/{site}", delete(login::login_clear_site))
        .route(
            "/api/login/{site}/{index}",
            delete(login::login_clear_group),
        )
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
        .route("/api/diff_history", get(jobs::api_diff_history))
        .route("/api/diff_show", get(jobs::api_diff_show))
        .route("/api/diff_restore", post(jobs::api_diff_restore))
        .route("/api/diff_merge", post(jobs::api_diff_merge))
        .route(
            "/api/csv/import",
            post(jobs::api_csv_import).layer(DefaultBodyLimit::max(
                MAX_WEB_CSV_IMPORT_BODY_BYTES,
            )),
        )
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
        .route("/api/downloadable.gif", get(jobs::api_downloadable_gif))
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
        .route("/api/update/start", post(update::api_update_start))
        .route("/api/library_backup", get(library_backup::api_library_backup_status))
        .route("/api/library_backup", post(library_backup::api_library_backup))
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
            local_auth_state,
            local_auth_middleware,
        ))
        .layer(middleware::from_fn_with_state(
            guard_state,
            request_guard_middleware,
        ))
        .layer(middleware::from_fn(security_headers_middleware))
        .with_state(state)
}

/// クリックジャッキング・MIME スニッフィング対策の共通ヘッダ。
///
/// `Origin` ガードは同一オリジンからのリクエストを許可するため、外部ページに
/// iframe で埋め込まれると UI 操作をクリックさせられる。フレーミングを禁止し、
/// 全レスポンスに nosniff / no-referrer を付ける。
async fn security_headers_middleware(
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        axum::http::HeaderName::from_static("x-frame-options"),
        header::HeaderValue::from_static("DENY"),
    );
    headers.insert(
        axum::http::HeaderName::from_static("content-security-policy"),
        header::HeaderValue::from_static("frame-ancestors 'none'"),
    );
    headers.insert(
        axum::http::HeaderName::from_static("x-content-type-options"),
        header::HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        axum::http::HeaderName::from_static("referrer-policy"),
        header::HeaderValue::from_static("no-referrer"),
    );
    response
}

/// loopback ブラウザ UI 用トークンを要求するか。
///
/// 設定 (`server-basic-auth.*` / `server-reverse-proxy.enable`) は稼働中にも
/// 再適用されるため、固定フラグではなく毎回現在値から判定する。
pub fn local_ui_auth_required(bind_host: &str, security: &server_security::ServerSecurity) -> bool {
    matches!(bind_host, "127.0.0.1" | "localhost" | "::1")
        && !security.reverse_proxy_mode
        && security.basic_auth_header.is_none()
}

impl AppState {
    /// 現在の設定で loopback UI トークンが必要か。
    pub fn local_ui_auth_required(&self) -> bool {
        local_ui_auth_required(&self.bind_host, &self.server_security.read())
    }
}

/// 起動時 URL のブートストラップチケットを検証して消費する。
///
/// チケットはプロセス起動ごとに作り直し、一度 Cookie 化したら無効になる。
pub(crate) fn consume_ui_bootstrap_ticket(state: &AppState, candidate: &str) -> bool {
    constant_time_str_eq(candidate, state.ui_bootstrap_ticket.as_str())
        && !state
            .ui_bootstrap_consumed
            .swap(true, std::sync::atomic::Ordering::SeqCst)
}

/// loopback UI トークンが必要なパスか。
///
/// 静的なページと埋め込みアセットはデータを持たないので素通しする。
/// データを返す API・WebSocket・ダウンロードだけを保護する。
pub(crate) fn is_ui_auth_protected_path(path: &str) -> bool {
    if path.starts_with("/assets/") {
        return false;
    }
    if matches!(
        path,
        "/"
            | "/index.html"
            | "/settings"
            | "/help"
            | "/about"
            | "/bookmarklet"
            | "/_rebooting"
            | "/notepad"
            | "/widget/drag_and_drop"
            | "/edit_menu"
    ) {
        return false;
    }
    if let Some(rest) = path.strip_prefix("/novels/") {
        return !(rest.ends_with("/setting") || rest.ends_with("/author_comments"));
    }
    true
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    for pair in cookies.split(';') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key.trim() == name {
            return Some(value.trim());
        }
    }
    None
}

/// UI トークン（Cookie または `x-narou-internal-token` ヘッダ）を照合する。
fn ui_token_matches(headers: &HeaderMap, state: &AppState) -> bool {
    if let Some(actual) = headers
        .get(INTERNAL_CONTROL_HEADER)
        .and_then(|value| value.to_str().ok())
    {
        if constant_time_str_eq(actual, state.ui_token.as_str())
            || constant_time_str_eq(actual, state.control_token.as_str())
        {
            return true;
        }
    }
    cookie_value(headers, UI_AUTH_COOKIE)
        .is_some_and(|actual| constant_time_str_eq(actual, state.ui_token.as_str()))
}

async fn local_auth_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    if !state.local_ui_auth_required() || !is_ui_auth_protected_path(request.uri().path()) {
        return next.run(request).await;
    }
    if ui_token_matches(request.headers(), &state) {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        "UI token required — open the URL printed when `narou web` started",
    )
        .into_response()
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
    let expected = state.server_security.read().basic_auth_header.clone();
    let Some(expected) = expected else {
        return next.run(request).await;
    };
    if basic_auth_matches(request.headers(), Some(expected.as_str())) {
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
        AppState, basic_auth_matches, host_allowed, is_exact_subdomain_wildcard_match,
        is_safe_wildcard_pattern, origin_allowed, origin_allowed_for_ports, removal_log_message,
        request_host_allowed, safe_existing_novel_dir, validate_web_tag_name,
        wildcard_host_match,
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
            requires_login: false,
            login_session: None,
            extra_fields: Default::default(),
        }
    }

    fn test_artifact_dir(name: &str) -> std::path::PathBuf {
        // 同名で呼ばれても衝突しないようカウンタで一意化する
        // (host_allowed_* 系のテストが並行で同じディレクトリを消し合うと
        // PersistentQueue::new が不安定になる)。
        static COUNTER: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("test-artifacts")
            .join(format!("web-mod-{name}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_state() -> AppState {
        let queue_dir = test_artifact_dir("security-state");
        let push_server = crate::web::push::PushServer::new();
        push_server.set_accepted_domains(["127.0.0.1", "localhost", "*.example.com"]);
        AppState {
            port: 8080,
            ws_port: 8081,
            push_server: Arc::new(push_server),
            services: super::default_app_services(Arc::new(
                crate::platform::mocks::MemoryNovelRepository::new(),
            )),
            server_security: super::test_server_security(
                Some("Basic dXNlcjpwYXNz"),
                vec!["127.0.0.1".to_string(), "localhost".to_string()],
                false,
            ),
            bind_host: "127.0.0.1".into(),
            control_token: "control-token".to_string(),
            ui_token: "ui-token".to_string(),
            ui_bootstrap_ticket: "bootstrap-ticket".to_string(),
            ui_bootstrap_consumed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            queue: Arc::new(
                crate::queue::PersistentQueue::new(&queue_dir.join("queue.yaml")).unwrap(),
            ),
            restore_prompt_pending: Arc::new(AtomicBool::new(false)),
            restorable_tasks_available: Arc::new(AtomicBool::new(false)),
            library_backup: Arc::new(crate::web::library_backup::LibraryBackupState::new()),
            running_jobs: Arc::new(parking_lot::Mutex::new(Vec::new())),
            running_child_pids: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
            cancelled_job_ids: Arc::new(parking_lot::Mutex::new(std::collections::HashSet::new())),
            auto_update_scheduler: Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    fn test_state_with_allowed_hosts(extra_hosts: Vec<String>) -> AppState {
        let queue_dir = test_artifact_dir("host-allowed");
        let push_server = crate::web::push::PushServer::new();
        push_server.set_accepted_domains(["127.0.0.1", "localhost"]);
        AppState {
            port: 8080,
            ws_port: 8081,
            push_server: Arc::new(push_server),
            services: super::default_app_services(Arc::new(
                crate::platform::mocks::MemoryNovelRepository::new(),
            )),
            server_security: super::test_server_security(
                Some("Basic dXNlcjpwYXNz"),
                extra_hosts,
                false,
            ),
            bind_host: "127.0.0.1".into(),
            control_token: "control-token".to_string(),
            ui_token: "ui-token".to_string(),
            ui_bootstrap_ticket: "bootstrap-ticket".to_string(),
            ui_bootstrap_consumed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            queue: Arc::new(
                crate::queue::PersistentQueue::new(&queue_dir.join("queue.yaml")).unwrap(),
            ),
            restore_prompt_pending: Arc::new(AtomicBool::new(false)),
            restorable_tasks_available: Arc::new(AtomicBool::new(false)),
            library_backup: Arc::new(crate::web::library_backup::LibraryBackupState::new()),
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

    #[tokio::test]
    async fn portless_cross_origin_post_cannot_clear_pending_jobs() {
        let temporary = tempfile::tempdir_in(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target"),
        ).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut state = test_state();
        state.port = listener.local_addr().unwrap().port();
        state.queue = Arc::new(crate::queue::PersistentQueue::new(
            &temporary.path().join("queue.yaml"),
        ).unwrap());
        state.queue.push(crate::queue::JobType::Update, "1").unwrap();
        let router = super::create_router(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{}/api/queue/clear", state.port);
        let response = client.post(&url)
            .header("Authorization", "Basic dXNlcjpwYXNz")
            .header("Origin", "http://127.0.0.1")
            .send().await.unwrap();
        let status = response.status();
        let pending = state.queue.pending_count();
        let same_origin = client.post(&url)
            .header("Authorization", "Basic dXNlcjpwYXNz")
            .header("Origin", format!("http://127.0.0.1:{}", state.port))
            .send().await.unwrap();
        let same_origin_status = same_origin.status();
        let remaining = state.queue.pending_count();
        server.abort();
        let _ = server.await;
        assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "pending after cross-origin POST: {pending}");
        assert_eq!(pending, 1);
        assert_eq!(same_origin_status, reqwest::StatusCode::OK);
        assert_eq!(remaining, 0);
    }

    #[test]
    fn reverse_proxy_mode_accepts_forwarded_same_origin_hosts() {
        let state = test_state_with_allowed_hosts(vec!["narou.example.com".to_string()]);
        state.server_security.write().reverse_proxy_mode = true;
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("narou.example.com:8443"),
        );
        assert!(request_host_allowed(&headers, &state, state.port));
        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("rebind.attacker.test:8443"),
        );
        assert!(
            !request_host_allowed(&headers, &state, state.port),
            "proxy mode must still verify the Host allow-list"
        );
        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("narou.example.com:8443"),
        );
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
        for (host, origin, expected) in [
            ("narou.example.com:8443", "https://narou.example.com", false),
            ("narou.example.com", "https://narou.example.com:8443", false),
            ("narou.example.com", "https://narou.example.com", true),
            ("narou.example.com", "https://narou.example.com:443", true),
        ] {
            headers.insert(axum::http::header::HOST, host.parse().unwrap());
            headers.insert(axum::http::header::ORIGIN, origin.parse().unwrap());
            assert_eq!(origin_allowed(&headers, &state, state.port), expected);
        }
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
        // `?` is not a single-character wildcard: `?.example.com` and
        // `?ocalhost` must not grant `a.example.com` / `localhost`.
        assert!(!is_safe_wildcard_pattern("?.example.com"));
        assert!(!is_safe_wildcard_pattern("?ocalhost"));
        assert!(!wildcard_host_match("?.example.com", "a.example.com"));
        assert!(!wildcard_host_match("?ocalhost", "localhost"));
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

    #[test]
    fn host_allowed_supports_safe_wildcard_patterns() {
        let state = test_state_with_allowed_hosts(vec![
            "127.0.0.1".to_string(),
            "localhost".to_string(),
            "*.example.com".to_string(),
        ]);

        // Exact match still works.
        assert!(host_allowed("127.0.0.1", &state));
        assert!(host_allowed("LOCALHOST", &state));

        // Safe subdomain wildcard matches exactly one label.
        assert!(host_allowed("sub.example.com", &state));

        // Sub-sub domains must not be allowed even if the simple wildcard
        // engine would otherwise match.
        assert!(!host_allowed("deep.sub.example.com", &state));

        // Domain boundary bypass must be rejected.
        assert!(!host_allowed("evil.example.com.attacker.com", &state));

        // Empty / unknown hosts are rejected.
        assert!(!host_allowed("", &state));
        assert!(!host_allowed("not-allowed.example.org", &state));
    }

    #[test]
    fn host_allowed_ignores_unsafe_wildcard_patterns() {
        // A pattern that fails `is_safe_wildcard_pattern` should never grant
        // access, even when present in `allowed_request_hosts`.
        let state = test_state_with_allowed_hosts(vec![
            "*.com".to_string(),       // too few labels after `*.`
            "example.com*".to_string(), // trailing wildcard
            "*".to_string(),            // bare wildcard
            "?.example.com".to_string(), // `?` is not a wildcard
            "?ocalhost".to_string(),
        ]);
        assert!(!host_allowed("foo.com", &state));
        assert!(!host_allowed("example.com", &state));
        assert!(!host_allowed("attacker.example.com", &state));
        assert!(!host_allowed("a.example.com", &state));
        assert!(!host_allowed("localhost", &state));
    }

    async fn post_json(
        client: &reqwest::Client,
        url: &str,
        body: &serde_json::Value,
    ) -> reqwest::Response {
        client
            .post(url)
            .header("authorization", "Basic dXNlcjpwYXNz")
            .header("x-narou-internal-token", "control-token")
            .header("content-type", "application/json")
            .body(serde_json::to_vec(body).unwrap())
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn csv_import_body_limit_reaches_app_validation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut state = test_state();
        state.port = port;
        let app = super::create_router(state);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let import_url = format!("http://127.0.0.1:{port}/api/csv/import");

        // axum 既定の 2 MiB を超える CSV でもアプリのハンドラまで届き、
        // 413 の英語メッセージではなくアプリの JSON 応答が返る。
        let csv = "a".repeat(2 * 1024 * 1024 + 512 * 1024);
        let response =
            post_json(&client, &import_url, &serde_json::json!({ "csv": csv })).await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_str(&response.text().await.unwrap()).unwrap();
        assert!(body.get("success").is_some(), "unexpected body: {body}");

        // アプリ側上限 (MAX_WEB_CSV_IMPORT_BYTES) 超過は
        // アプリの「csv is too large」で拒否される。
        let csv = "a".repeat(super::MAX_WEB_CSV_IMPORT_BYTES + 1);
        let response =
            post_json(&client, &import_url, &serde_json::json!({ "csv": csv })).await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_str(&response.text().await.unwrap()).unwrap();
        assert_eq!(body["success"], false);
        assert_eq!(body["message"], "csv is too large");

        // ボディ全体の上限 (JSON エスケープ分の余裕込み) を超えたものは
        // フレームワークが 413 で拒否する。
        let csv = "a".repeat(super::MAX_WEB_CSV_IMPORT_BODY_BYTES);
        let response =
            post_json(&client, &import_url, &serde_json::json!({ "csv": csv })).await;
        assert_eq!(response.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);

        // 他ルートの既定 2 MiB は緩めない: 1 MiB 超の JSON は従来どおり
        // アプリ側の検証で拒否され、2 MiB 超は 413 になる。
        let notepad_url = format!("http://127.0.0.1:{port}/api/notepad/save");
        let content = "a".repeat(1024 * 1024 + 1);
        let response =
            post_json(&client, &notepad_url, &serde_json::json!({ "content": content }))
                .await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_str(&response.text().await.unwrap()).unwrap();
        assert_eq!(body["success"], false);
        assert_eq!(body["message"], "notepad content is too large");

        let content = "a".repeat(2 * 1024 * 1024 + 1);
        let response =
            post_json(&client, &notepad_url, &serde_json::json!({ "content": content }))
                .await;
        assert_eq!(response.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);

        server.abort();
    }

    #[tokio::test]
    async fn loopback_ui_token_blocks_api_until_cookie_is_bootstrapped() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut state = test_state();
        state.port = listener.local_addr().unwrap().port();
        {
            let mut security = state.server_security.write();
            security.basic_auth_header = None;
            security.reverse_proxy_mode = false;
        }
        let router = super::create_router(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let base = format!("http://127.0.0.1:{}", state.port);

        let unauthorized = client.get(format!("{base}/api/list")).send().await.unwrap();
        assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

        let bootstrap = client
            .get(format!("{base}/?narou_ui_token=bootstrap-ticket"))
            .send()
            .await
            .unwrap();
        assert_eq!(bootstrap.status(), reqwest::StatusCode::SEE_OTHER);
        let set_cookie = bootstrap
            .headers()
            .get(reqwest::header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(set_cookie.contains("narou_ui_token=ui-token"));
        assert!(set_cookie.contains("HttpOnly"));

        let authorized = client
            .get(format!("{base}/api/list"))
            .header(reqwest::header::COOKIE, "narou_ui_token=ui-token")
            .send()
            .await
            .unwrap();
        assert_ne!(authorized.status(), reqwest::StatusCode::UNAUTHORIZED);

        // 同じチケットの再利用は Cookie 化できない。
        let replay = client
            .get(format!("{base}/?narou_ui_token=bootstrap-ticket"))
            .send()
            .await
            .unwrap();
        assert_eq!(replay.status(), reqwest::StatusCode::OK);
        assert!(replay.headers().get(reqwest::header::SET_COOKIE).is_none());

        server.abort();
    }

    #[tokio::test]
    async fn html_responses_deny_framing() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut state = test_state();
        state.port = listener.local_addr().unwrap().port();
        let router = super::create_router(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::new();
        let response = client
            .get(format!("http://127.0.0.1:{}/", state.port))
            .header("Authorization", "Basic dXNlcjpwYXNz")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("x-frame-options")
                .and_then(|value| value.to_str().ok()),
            Some("DENY")
        );
        assert_eq!(
            response
                .headers()
                .get("content-security-policy")
                .and_then(|value| value.to_str().ok()),
            Some("frame-ancestors 'none'")
        );
        server.abort();
    }

    #[test]
    fn ui_token_protects_data_paths_but_not_static_pages() {
        for public in [
            "/",
            "/settings",
            "/help",
            "/assets/js/main.js",
            "/novels/1/setting",
            "/novels/1/author_comments",
        ] {
            assert!(!super::is_ui_auth_protected_path(public), "{public}");
        }
        for protected in ["/api/list", "/api/update/start", "/ws", "/novels/1/download"] {
            assert!(super::is_ui_auth_protected_path(protected), "{protected}");
        }
    }

    #[test]
    fn ui_token_matching_accepts_cookie_and_internal_header_only() {
        let state = test_state();
        let mut headers = axum::http::HeaderMap::new();
        assert!(!super::ui_token_matches(&headers, &state));

        headers.insert(
            super::INTERNAL_CONTROL_HEADER,
            axum::http::HeaderValue::from_static("ui-token"),
        );
        assert!(super::ui_token_matches(&headers, &state));

        headers.insert(
            super::INTERNAL_CONTROL_HEADER,
            axum::http::HeaderValue::from_static("control-token"),
        );
        assert!(super::ui_token_matches(&headers, &state));

        headers.insert(
            super::INTERNAL_CONTROL_HEADER,
            axum::http::HeaderValue::from_static("wrong"),
        );
        assert!(!super::ui_token_matches(&headers, &state));

        let mut cookie_headers = axum::http::HeaderMap::new();
        cookie_headers.insert(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_static("a=b; narou_ui_token=ui-token"),
        );
        assert!(super::ui_token_matches(&cookie_headers, &state));
        cookie_headers.insert(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_static("narou_ui_token=wrong"),
        );
        assert!(!super::ui_token_matches(&cookie_headers, &state));
    }

    #[test]
    fn ui_bootstrap_ticket_is_single_use() {
        let state = test_state();
        assert!(!super::consume_ui_bootstrap_ticket(&state, "wrong"));
        assert!(super::consume_ui_bootstrap_ticket(
            &state,
            "bootstrap-ticket"
        ));
        assert!(!super::consume_ui_bootstrap_ticket(
            &state,
            "bootstrap-ticket"
        ));
        // 誤った試行では消費されない。
        let second = test_state();
        assert!(!super::consume_ui_bootstrap_ticket(&second, "wrong"));
        assert!(super::consume_ui_bootstrap_ticket(
            &second,
            "bootstrap-ticket"
        ));
    }

    #[test]
    fn local_ui_auth_requires_loopback_without_basic_auth_or_proxy() {
        let state = test_state();
        assert!(!state.local_ui_auth_required(), "basic auth is configured");
        {
            let mut security = state.server_security.write();
            security.basic_auth_header = None;
            security.reverse_proxy_mode = false;
        }
        assert!(state.local_ui_auth_required());
        state.server_security.write().reverse_proxy_mode = true;
        assert!(!state.local_ui_auth_required());
        state.server_security.write().reverse_proxy_mode = false;
        state.server_security.write().basic_auth_header = Some("Basic x".to_string());
        assert!(!state.local_ui_auth_required());
        assert!(!super::local_ui_auth_required(
            "0.0.0.0",
            &state.server_security.read().clone()
        ));
    }
}

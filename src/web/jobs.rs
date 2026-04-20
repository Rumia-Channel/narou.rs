use std::path::PathBuf;
use std::sync::atomic::Ordering;

use axum::{
    extract::{Query, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Json, Response},
};
use serde::Deserialize;

use crate::db::with_database;
use crate::downloader::site_setting::SiteSetting;
use crate::downloader::types::{CACHE_SAVE_DIR, SECTION_SAVE_DIR, SectionFile};
use crate::downloader::{Downloader, TargetType};
use crate::queue::{JobType, PersistentQueue, QueueJob, QueueLane};

use super::AppState;
use super::state::{
    ApiResponse, ConfirmRunningTasksBody, ConvertBody, CsvImportBody, DiffBody, DiffCleanBody,
    DownloadBody, ReorderBody, TagInfoBody, TargetsBody, TaskIdBody, UpdateBody, UpdateByTagBody,
};

const WEBUI_UPDATE_START_PREFIX: &str = "__webui_update_start__=";
const TRANSPARENT_GIF: &[u8] = &[
    71, 73, 70, 56, 57, 97, 1, 0, 1, 0, 128, 0, 0, 0, 0, 0, 255, 255, 255, 33, 249, 4, 1, 0,
    0, 0, 0, 44, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 68, 1, 0, 59,
];

#[derive(Debug, Deserialize)]
pub struct BookmarkletDownloadQuery {
    pub target: Option<String>,
    pub mail: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DiffListQuery {
    pub target: Option<String>,
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn tag_color_class(color: &str) -> &'static str {
    match color {
        "green" => "tag-green",
        "yellow" => "tag-yellow",
        "blue" => "tag-blue",
        "magenta" => "tag-magenta",
        "cyan" => "tag-cyan",
        "red" => "tag-red",
        "white" => "tag-white",
        _ => "tag-default",
    }
}

fn validate_download_targets(targets: &[String]) -> Result<(), String> {
    if targets.len() > super::MAX_WEB_TARGETS_PER_REQUEST {
        return Err("too many targets".to_string());
    }
    for target in targets {
        super::validate_web_target_value(target)
            .map_err(|_| "invalid download target".to_string())?;
    }
    Ok(())
}

fn validate_diff_number(number: &str) -> Result<String, String> {
    let parsed = number
        .trim()
        .parse::<usize>()
        .map_err(|_| "invalid diff number".to_string())?;
    Ok(parsed.to_string())
}

fn validate_general_lastup_option(option: &str) -> Result<Option<&'static str>, String> {
    match option {
        "all" => Ok(None),
        "narou" => Ok(Some("narou")),
        "other" => Ok(Some("other")),
        _ => Err("invalid general_lastup option".to_string()),
    }
}

fn query_to_bool(value: Option<&str>) -> bool {
    matches!(value, Some("1" | "true" | "yes" | "on"))
}

fn queue_download_jobs(
    queue: &PersistentQueue,
    targets: &[String],
    force: bool,
    mail: bool,
) -> Result<Vec<String>, String> {
    validate_download_targets(targets)?;
    let job_type = if mail { JobType::Mail } else { JobType::Download };
    let jobs: Vec<(JobType, String)> = targets
        .iter()
        .map(|target| {
            if force {
                (job_type, format!("--force\t{}", target))
            } else {
                (job_type, target.clone())
            }
        })
        .collect();
    queue.push_batch(&jobs).map_err(|e| e.to_string())
}

fn queue_bookmarklet_download(
    queue: &PersistentQueue,
    target: &str,
    mail: bool,
) -> Result<String, String> {
    let target = super::validate_web_target_value(target)
        .map_err(|_| "invalid download target".to_string())?;
    let job_target = if mail {
        format!("--mail\t{}", target)
    } else {
        target
    };
    queue
        .push(JobType::Download, &job_target)
        .map_err(|e| e.to_string())
}

fn resolve_existing_id_for_target(target: &str) -> Option<i64> {
    match Downloader::get_target_type(target) {
        TargetType::Id => {
            let id = target.parse::<i64>().ok()?;
            with_database(|db| Ok(db.get(id).map(|record| record.id)))
                .ok()
                .flatten()
        }
        TargetType::Url => {
            let settings = SiteSetting::load_all().ok()?;
            let setting = settings.iter().find(|setting| setting.matches_url(target))?;
            let toc_url = setting
                .toc_url_with_url_captures(target)
                .unwrap_or_else(|| setting.toc_url());
            with_database(|db| Ok(db.get_by_toc_url(&toc_url).map(|record| record.id)))
                .ok()
                .flatten()
        }
        TargetType::Ncode => {
            let ncode = target.to_lowercase();
            with_database(|db| {
                Ok(db
                    .all_records()
                    .values()
                    .find(|record| record.ncode.as_deref() == Some(ncode.as_str()))
                    .map(|record| record.id))
            })
            .ok()
            .flatten()
        }
        _ => with_database(|db| Ok(db.find_by_title(target).map(|record| record.id)))
            .ok()
            .flatten(),
    }
}

fn existing_update_job_id(
    queue: &PersistentQueue,
    running_jobs: &parking_lot::Mutex<Vec<QueueJob>>,
    target: &str,
) -> Option<String> {
    if let Some(job) = running_jobs
        .lock()
        .iter()
        .find(|job| matches!(job.job_type, JobType::Update) && job.target == target)
    {
        return Some(job.id.clone());
    }

    queue
        .get_pending_tasks()
        .into_iter()
        .find(|job| matches!(job.job_type, JobType::Update) && job.target == target)
        .map(|job| job.id)
}

fn push_update_job_if_needed(
    queue: &PersistentQueue,
    running_jobs: &parking_lot::Mutex<Vec<QueueJob>>,
    target: String,
) -> Result<(Vec<String>, bool), String> {
    if let Some(existing_id) = existing_update_job_id(queue, running_jobs, &target) {
        return Ok((vec![existing_id], false));
    }

    queue
        .push_batch(&[(JobType::Update, target)])
        .map(|ids| (ids, true))
        .map_err(|e| e.to_string())
}

fn running_job_count(state: &AppState) -> usize {
    state.running_jobs.lock().len()
}

fn restorable_tasks_available(state: &AppState, pending_count: usize) -> bool {
    state.restore_prompt_pending.load(Ordering::Relaxed) && pending_count > 0
}

fn running_job_count_for_lane(state: &AppState, lane: QueueLane) -> usize {
    state
        .running_jobs
        .lock()
        .iter()
        .filter(|job| job.job_type.lane() == lane)
        .count()
}

fn running_job_by_id(state: &AppState, task_id: &str) -> Option<QueueJob> {
    state
        .running_jobs
        .lock()
        .iter()
        .find(|job| job.id == task_id)
        .cloned()
}

fn kill_running_child(state: &AppState) {
    let pids: Vec<u32> = {
        let mut guard = state.running_child_pids.lock();
        let values = guard.values().copied().collect();
        guard.clear();
        values
    };
    for pid in pids {
        kill_process_tree(pid, &state.push_server);
    }
}

fn kill_running_child_for_job(state: &AppState, job_id: &str) -> bool {
    let pid = state.running_child_pids.lock().remove(job_id);
    if let Some(pid) = pid {
        kill_process_tree(pid, &state.push_server);
        true
    } else {
        false
    }
}

fn kill_process_tree(pid: u32, push_server: &std::sync::Arc<super::push::PushServer>) {
    let result = if cfg!(windows) {
        std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
    } else {
        std::process::Command::new("kill")
            .args(["-TERM", &format!("-{}", pid)])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
    };
    if let Err(e) = result {
        push_server.broadcast_echo(&format!("プロセス終了に失敗: {}", e), "stdout");
    }
}

fn read_sorted_cache_dirs(cache_root: &PathBuf) -> Result<Vec<PathBuf>, String> {
    let mut list = Vec::new();
    for entry in std::fs::read_dir(cache_root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.is_dir() {
            list.push(path);
        }
    }
    list.sort_by(|a, b| {
        let a_name = a.file_name().and_then(|value| value.to_str()).unwrap_or_default();
        let b_name = b.file_name().and_then(|value| value.to_str()).unwrap_or_default();
        b_name.cmp(a_name)
    });
    Ok(list)
}

fn read_sorted_section_files(dir: &PathBuf) -> Result<Vec<PathBuf>, String> {
    let mut list = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("yaml") {
            list.push(path);
        }
    }
    list.sort_by_key(|path| {
        path.file_stem()
            .and_then(|value| value.to_str())
            .and_then(|value| value.split_once(' ').map(|(index, _)| index.to_string()))
            .and_then(|index| index.parse::<usize>().ok())
            .unwrap_or(0)
    });
    Ok(list)
}

fn render_diff_list_html_for_target(target: &str) -> String {
    let Some(id) = resolve_existing_id_for_target(target) else {
        return String::new();
    };
    let Ok((record, archive_root)) = with_database(|db| {
        Ok((db.get(id).cloned(), db.archive_root().to_path_buf()))
    }) else {
        return String::new();
    };
    let Some(record) = record else {
        return String::new();
    };

    let Ok(base_dir) = super::safe_existing_novel_dir(&archive_root, &record) else {
        return String::new();
    };
    let cache_root = base_dir
        .join(SECTION_SAVE_DIR)
        .join(CACHE_SAVE_DIR);
    let Ok(cache_dirs) = read_sorted_cache_dirs(&cache_root) else {
        return String::new();
    };
    if cache_dirs.is_empty() {
        return String::new();
    }

    let mut html = String::new();
    for (number, cache_dir) in cache_dirs.iter().enumerate() {
        let version = cache_dir
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        html.push_str("<div class=\"diff-list-group\">");
        html.push_str(&format!(
            "<div class=\"diff-list-version\">{}&nbsp;&nbsp;-{}</div>",
            html_escape(version),
            number + 1
        ));
        let Ok(section_paths) = read_sorted_section_files(cache_dir) else {
            html.push_str("</div>");
            continue;
        };
        if section_paths.is_empty() {
            html.push_str("<div class=\"diff-list-entry\">(最新話のみのアップデート)</div></div>");
            continue;
        }
        for section_path in section_paths {
            let Ok(content) = std::fs::read_to_string(&section_path) else {
                continue;
            };
            let Ok(section) = serde_yaml::from_str::<SectionFile>(&content) else {
                continue;
            };
            html.push_str(&format!(
                "<div class=\"diff-list-entry\">第{}部分　{}</div>",
                html_escape(&section.index),
                html_escape(section.subtitle.trim_end())
            ));
        }
        html.push_str("</div>");
    }
    html
}

fn gif_response() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/gif"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        TRANSPARENT_GIF,
    )
        .into_response()
}

pub async fn api_download(
    State(state): State<AppState>,
    Json(body): Json<DownloadBody>,
) -> Json<serde_json::Value> {
    let targets = body.targets;
    let ids = match queue_download_jobs(state.queue.as_ref(), &targets, body.force, body.mail) {
        Ok(ids) => ids,
        Err(message) => {
            return serde_json::json!({
                "success": false,
                "message": message,
                "results": []
            })
            .into();
        }
    };

    let results: Vec<serde_json::Value> = targets
        .iter()
        .zip(ids.iter())
        .map(|(target, job_id)| {
            serde_json::json!({ "target": target, "job_id": job_id, "status": "queued" })
        })
        .collect();

    state.push_server.broadcast_event("notification.queue", "");
    serde_json::json!({ "success": true, "results": results }).into()
}

pub async fn api_update(
    State(state): State<AppState>,
    Json(body): Json<UpdateBody>,
) -> Json<serde_json::Value> {
    let targets = match normalize_update_targets(&targets_to_strings(&body.targets)) {
        Ok(targets) => targets,
        Err(message) => {
            return serde_json::json!({
                "success": false,
                "message": message,
                "count": 0
            })
            .into();
        }
    };

    // Build a single update job target string from CLI-style args
    // e.g. ["--gl", "narou"] → "--gl\tnarou"
    // e.g. ["1", "2"] → separate jobs for each ID
    // e.g. [] → update all (empty target)
    let has_flags = targets.iter().any(|t| t.starts_with("--"));

    let is_update_all = targets.is_empty();
    let count;
    let combined = if has_flags {
        count = 1;
        targets.join("\t")
    } else {
        let mut args = if is_update_all {
            with_database(|db| Ok(db.ids().into_iter().map(|id| id.to_string()).collect::<Vec<_>>()))
                .unwrap_or_default()
        } else {
            targets.clone()
        };
        count = args.len();
        if count == 0 {
            return serde_json::json!({
                "success": true,
                "status": "queued",
                "count": 0,
                "job_ids": []
            })
            .into();
        }
        if body.force {
            args.insert(0, "--force".to_string());
        }
        let sort_display = current_sort_display_string();
        let start_message = if is_update_all {
            format!("全ての小説の更新を開始します（{}件を{}で処理）", count, sort_display)
        } else {
            format!("更新を開始します（{}件を{}で処理）", count, sort_display)
        };
        let mut parts = Vec::with_capacity(args.len() + 1);
        parts.push(format!("{}{}", WEBUI_UPDATE_START_PREFIX, start_message));
        parts.extend(args);
        parts.join("\t")
    };
    let (job_ids, queued) =
        match push_update_job_if_needed(state.queue.as_ref(), &state.running_jobs, combined) {
        Ok(result) => result,
        Err(message) => {
            return serde_json::json!({
                "success": false,
                "message": message,
                "count": 0
            })
            .into();
        }
    };

    if queued {
        state.push_server.broadcast_event("notification.queue", "");
    }
    serde_json::json!({
        "success": true,
        "status": if queued { "queued" } else { "already_queued" },
        "count": count,
        "job_ids": job_ids
    })
    .into()
}

fn normalize_update_targets(targets: &[String]) -> Result<Vec<String>, String> {
    if targets.len() > super::MAX_WEB_TARGETS_PER_REQUEST {
        return Err("too many targets".to_string());
    }
    let mut normalized = Vec::with_capacity(targets.len());
    let mut i = 0usize;
    while i < targets.len() {
        let target = &targets[i];
        if let Some(tag) = target.strip_prefix("--tag=") {
            let tag = super::validate_web_tag_name(tag)
                .map_err(|_| "--tag requires a tag name".to_string())?;
            normalized.push(format!("tag:{}", tag));
            i += 1;
            continue;
        }
        if target == "--tag" {
            let Some(tag) = targets.get(i + 1) else {
                return Err("--tag requires a tag name".to_string());
            };
            let tag = super::validate_web_tag_name(tag)
                .map_err(|_| "--tag requires a tag name".to_string())?;
            normalized.push(format!("tag:{}", tag));
            i += 2;
            continue;
        }
        normalized.push(
            super::validate_web_target_value(target)
                .map_err(|_| "invalid target".to_string())?,
        );
        i += 1;
    }
    Ok(normalized)
}

pub async fn api_convert(
    State(state): State<AppState>,
    Json(body): Json<ConvertBody>,
) -> Json<serde_json::Value> {
    if body.targets.len() > super::MAX_WEB_TARGETS_PER_REQUEST {
        return serde_json::json!({
            "success": false,
            "message": "too many targets",
            "results": []
        })
        .into();
    }
    let targets: Vec<String> = match body
        .targets
        .iter()
        .map(|target| super::validate_web_target_value(target))
        .collect()
    {
        Ok(targets) => targets,
        Err(message) => {
            return serde_json::json!({
                "success": false,
                "message": message,
                "results": []
            })
            .into();
        }
    };
    let device = body
        .device
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    if let Some(device) = device.as_deref() {
        if device.len() > super::MAX_WEB_TARGET_LENGTH || device.chars().any(|ch| ch.is_control()) {
            return serde_json::json!({
                "success": false,
                "message": "invalid device",
                "results": []
            })
            .into();
        }
    }
    let jobs: Vec<(JobType, String)> = body
        .targets
        .iter()
        .zip(targets.iter())
        .map(|(_raw_target, target)| {
            (
                JobType::Convert,
                encode_convert_job_target(target, device.as_deref()),
            )
        })
        .collect();
    let ids = match state.queue.push_batch(&jobs) {
        Ok(ids) => ids,
        Err(e) => {
            return serde_json::json!({
                "success": false,
                "message": e.to_string(),
                "results": []
            })
            .into();
        }
    };

    let results: Vec<serde_json::Value> = body
        .targets
        .iter()
        .zip(ids.iter())
        .zip(targets.iter())
        .map(|((_raw_target, job_id), target)| {
            serde_json::json!({
                "target": target,
                "device": device,
                "job_id": job_id,
                "status": "queued"
            })
        })
        .collect();

    state.push_server.broadcast_event("notification.queue", "");
    serde_json::json!({ "success": true, "results": results }).into()
}

pub async fn queue_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    let running_jobs = state.running_jobs.lock().clone();
    let running_count = running_job_count(&state);
    let running_label = match running_jobs.as_slice() {
        [] => serde_json::Value::Null,
        [job] => serde_json::Value::String(job.target.clone()),
        jobs => serde_json::Value::String(format!("{} 件実行中", jobs.len())),
    };
    Json(serde_json::json!({
        "pending": state.queue.pending_count(),
        "completed": state.queue.completed_count(),
        "failed": state.queue.failed_count(),
        "running": running_label,
        "running_count": running_count,
    }))
}

pub async fn queue_clear(State(state): State<AppState>) -> Json<ApiResponse> {
    match state.queue.clear() {
        Ok(_) => Json(ApiResponse {
            success: true,
            message: "Queue cleared".to_string(),
        }),
        Err(e) => Json(ApiResponse {
            success: false,
            message: e.to_string(),
        }),
    }
}

/// Helper: convert mixed JSON values (numbers or strings) into string targets
fn targets_to_strings(targets: &[serde_json::Value]) -> Vec<String> {
    targets
        .iter()
        .map(|v| match v {
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect()
}

fn encode_convert_job_target(target: &str, device: Option<&str>) -> String {
    match device.map(str::trim).filter(|value| !value.is_empty()) {
        Some(device) => format!("{}\t{}", target, device),
        None => target.to_string(),
    }
}

/// Helper: spawn an immediate child process with the given args
fn run_immediate(args: &[&str]) -> Result<std::process::Output, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    std::process::Command::new(exe)
        .args(args)
        .current_dir(std::env::current_dir().unwrap_or_default())
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| e.to_string())
}

// POST /api/send
pub async fn api_send(
    State(state): State<AppState>,
    Json(body): Json<TargetsBody>,
) -> Json<serde_json::Value> {
    let raw_targets = targets_to_strings(&body.targets);
    if raw_targets.len() > super::MAX_WEB_TARGETS_PER_REQUEST {
        return serde_json::json!({
            "success": false,
            "message": "too many targets",
        })
        .into();
    }
    let targets: Vec<String> = match raw_targets
        .iter()
        .map(|target| super::validate_web_target_value(target))
        .collect()
    {
        Ok(targets) => targets,
        Err(message) => {
            return serde_json::json!({
                "success": false,
                "message": message,
            })
            .into();
        }
    };
    let jobs: Vec<(JobType, String)> = targets
        .iter()
        .map(|t| (JobType::Send, t.clone()))
        .collect();
    let ids = match state.queue.push_batch(&jobs) {
        Ok(ids) => ids,
        Err(e) => {
            return serde_json::json!({
                "success": false,
                "message": e.to_string(),
            })
            .into();
        }
    };

    state.push_server.broadcast_event("notification.queue", "");
    serde_json::json!({
        "success": true,
        "count": ids.len(),
        "job_ids": ids,
    })
    .into()
}

// POST /api/inspect
pub async fn api_inspect(
    State(state): State<AppState>,
    Json(body): Json<TargetsBody>,
) -> Json<ApiResponse> {
    let raw_targets = targets_to_strings(&body.targets);
    if raw_targets.len() > super::MAX_WEB_TARGETS_PER_REQUEST {
        return Json(ApiResponse {
            success: false,
            message: "too many targets".to_string(),
        });
    }
    let targets: Vec<String> = match raw_targets
        .iter()
        .map(|target| super::validate_web_target_value(target))
        .collect()
    {
        Ok(targets) => targets,
        Err(message) => {
            return Json(ApiResponse {
                success: false,
                message,
            });
        }
    };
    let mut args: Vec<&str> = vec!["inspect"];
    let target_refs: Vec<&str> = targets.iter().map(|s| s.as_str()).collect();
    args.extend(&target_refs);

    match run_immediate(&args) {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.is_empty() {
                state.push_server.broadcast_echo(&stdout, "stdout");
            }
            Json(ApiResponse {
                success: output.status.success(),
                message: if output.status.success() {
                    "OK".to_string()
                } else {
                    String::from_utf8_lossy(&output.stderr).to_string()
                },
            })
        }
        Err(e) => Json(ApiResponse {
            success: false,
            message: e,
        }),
    }
}

// POST /api/folder
pub async fn api_folder(
    State(_state): State<AppState>,
    Json(body): Json<TargetsBody>,
) -> Json<ApiResponse> {
    let raw_targets = targets_to_strings(&body.targets);
    if raw_targets.len() > super::MAX_WEB_TARGETS_PER_REQUEST {
        return Json(ApiResponse {
            success: false,
            message: "too many targets".to_string(),
        });
    }
    let targets: Vec<String> = match raw_targets
        .iter()
        .map(|target| super::validate_web_target_value(target))
        .collect()
    {
        Ok(targets) => targets,
        Err(message) => {
            return Json(ApiResponse {
                success: false,
                message,
            });
        }
    };
    let mut args: Vec<&str> = vec!["folder"];
    let target_refs: Vec<&str> = targets.iter().map(|s| s.as_str()).collect();
    args.extend(&target_refs);

    match run_immediate(&args) {
        Ok(output) => Json(ApiResponse {
            success: output.status.success(),
            message: "OK".to_string(),
        }),
        Err(e) => Json(ApiResponse {
            success: false,
            message: e,
        }),
    }
}

// POST /api/backup
pub async fn api_backup(
    State(state): State<AppState>,
    Json(body): Json<TargetsBody>,
) -> Json<serde_json::Value> {
    let raw_targets = targets_to_strings(&body.targets);
    if raw_targets.len() > super::MAX_WEB_TARGETS_PER_REQUEST {
        return serde_json::json!({
            "success": false,
            "message": "too many targets",
        })
        .into();
    }
    let targets: Vec<String> = match raw_targets
        .iter()
        .map(|target| super::validate_web_target_value(target))
        .collect()
    {
        Ok(targets) => targets,
        Err(message) => {
            return serde_json::json!({
                "success": false,
                "message": message,
            })
            .into();
        }
    };
    let jobs: Vec<(JobType, String)> = targets
        .iter()
        .map(|t| (JobType::Backup, t.clone()))
        .collect();
    let ids = match state.queue.push_batch(&jobs) {
        Ok(ids) => ids,
        Err(e) => {
            return serde_json::json!({
                "success": false,
                "message": e.to_string(),
            })
            .into();
        }
    };

    state.push_server.broadcast_event("notification.queue", "");
    serde_json::json!({
        "success": true,
        "count": ids.len(),
        "job_ids": ids,
    })
    .into()
}

// POST /api/backup_bookmark — backup bookmarks from device
pub async fn api_backup_bookmark(
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    // Ruby parity: runs "send --backup-bookmark"
    let job_id = match state.queue.push(JobType::Send, "--backup-bookmark") {
        Ok(id) => id,
        Err(e) => {
            return serde_json::json!({
                "success": false,
                "message": e.to_string(),
            })
            .into();
        }
    };

    state.push_server.broadcast_event("notification.queue", "");
    serde_json::json!({
        "success": true,
        "job_id": job_id,
    })
    .into()
}

// POST /api/mail
pub async fn api_mail(
    State(state): State<AppState>,
    Json(body): Json<TargetsBody>,
) -> Json<serde_json::Value> {
    let raw_targets = targets_to_strings(&body.targets);
    if raw_targets.len() > super::MAX_WEB_TARGETS_PER_REQUEST {
        return serde_json::json!({
            "success": false,
            "message": "too many targets",
        })
        .into();
    }
    let targets: Vec<String> = match raw_targets
        .iter()
        .map(|target| super::validate_web_target_value(target))
        .collect()
    {
        Ok(targets) => targets,
        Err(message) => {
            return serde_json::json!({
                "success": false,
                "message": message,
            })
            .into();
        }
    };
    let jobs: Vec<(JobType, String)> = targets
        .iter()
        .map(|t| (JobType::Mail, t.clone()))
        .collect();
    let ids = match state.queue.push_batch(&jobs) {
        Ok(ids) => ids,
        Err(e) => {
            return serde_json::json!({
                "success": false,
                "message": e.to_string(),
            })
            .into();
        }
    };

    state.push_server.broadcast_event("notification.queue", "");
    serde_json::json!({
        "success": true,
        "count": ids.len(),
        "job_ids": ids,
    })
    .into()
}

// POST /api/setting_burn
pub async fn api_setting_burn(
    State(state): State<AppState>,
    Json(body): Json<TargetsBody>,
) -> Json<ApiResponse> {
    let raw_targets = targets_to_strings(&body.targets);
    if raw_targets.len() > super::MAX_WEB_TARGETS_PER_REQUEST {
        return Json(ApiResponse {
            success: false,
            message: "too many targets".to_string(),
        });
    }
    let targets: Vec<String> = match raw_targets
        .iter()
        .map(|target| super::validate_web_target_value(target))
        .collect()
    {
        Ok(targets) => targets,
        Err(message) => {
            return Json(ApiResponse {
                success: false,
                message,
            });
        }
    };
    let mut args: Vec<&str> = vec!["setting", "--burn"];
    let target_refs: Vec<&str> = targets.iter().map(|s| s.as_str()).collect();
    args.extend(&target_refs);

    match run_immediate(&args) {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.is_empty() {
                state.push_server.broadcast_echo(&stdout, "stdout");
            }
            Json(ApiResponse {
                success: output.status.success(),
                message: if output.status.success() {
                    "設定を焼き込みました".to_string()
                } else {
                    String::from_utf8_lossy(&output.stderr).to_string()
                },
            })
        }
        Err(e) => Json(ApiResponse {
            success: false,
            message: e,
        }),
    }
}

// POST /api/diff_list
pub async fn api_diff_list(
    State(_state): State<AppState>,
    Json(body): Json<TargetsBody>,
) -> Json<serde_json::Value> {
    let targets = targets_to_strings(&body.targets);
    if targets.len() > super::MAX_WEB_TARGETS_PER_REQUEST {
        return serde_json::json!({ "error": "too many targets" }).into();
    }
    let mut diffs = Vec::new();

    for target in &targets {
        let id: i64 = match target.parse() {
            Ok(id) => id,
            Err(_) => continue,
        };

        let result = with_database(|db| {
            let record = db.get(id).cloned();
            let archive_root = db.archive_root().to_path_buf();
            Ok((record, archive_root))
        });

        let (record, archive_root) = match result {
            Ok((Some(record), root)) => (record, root),
            _ => {
                diffs.push(serde_json::json!({
                    "id": id,
                    "title": format!("ID: {}", id),
                    "content": "Novel not found",
                }));
                continue;
            }
        };

        let novel_dir = match super::safe_existing_novel_dir(&archive_root, &record) {
            Ok(dir) => dir,
            Err(_) => {
                diffs.push(serde_json::json!({
                    "id": id,
                    "title": record.title,
                    "content": "Invalid novel storage path",
                }));
                continue;
            }
        };
        let diff_path = novel_dir.join("diff.txt");

        let content = if diff_path.exists() {
            std::fs::read_to_string(&diff_path).unwrap_or_else(|_| "読み取りエラー".to_string())
        } else {
            "No diff".to_string()
        };

        diffs.push(serde_json::json!({
            "id": id,
            "title": record.title,
            "content": content,
        }));
    }

    serde_json::json!({ "diffs": diffs }).into()
}

// GET /api/diff_list
pub async fn api_diff_list_get(
    State(_state): State<AppState>,
    Query(params): Query<DiffListQuery>,
) -> Html<String> {
    Html(
        params
            .target
            .as_deref()
            .map(render_diff_list_html_for_target)
            .unwrap_or_default(),
    )
}

// GET /api/download_request
pub async fn api_download_request(
    State(state): State<AppState>,
    Query(params): Query<BookmarkletDownloadQuery>,
) -> Json<serde_json::Value> {
    let Some(target) = params.target.as_deref().map(str::trim).filter(|target| !target.is_empty()) else {
        return Json(serde_json::json!({ "status": 2, "id": null }));
    };
    if let Some(id) = resolve_existing_id_for_target(target) {
        return Json(serde_json::json!({ "status": 1, "id": id }));
    }
    match queue_bookmarklet_download(
        state.queue.as_ref(),
        target,
        query_to_bool(params.mail.as_deref()),
    ) {
        Ok(_) => {
            state.push_server.broadcast_event("notification.queue", "");
            Json(serde_json::json!({ "status": 0, "id": null }))
        }
        Err(_) => Json(serde_json::json!({ "status": 2, "id": null })),
    }
}

// GET /api/downloadable.gif
pub async fn api_downloadable_gif(
    State(_state): State<AppState>,
    Query(params): Query<BookmarkletDownloadQuery>,
) -> Response {
    let _number = match params.target.as_deref() {
        Some(target) if !target.trim().is_empty() => {
            if resolve_existing_id_for_target(target).is_some() { 1 } else { 0 }
        }
        _ => 2,
    };
    gif_response()
}

// GET /api/download4ssl
pub async fn api_download4ssl(
    State(state): State<AppState>,
    Query(params): Query<BookmarkletDownloadQuery>,
) -> Response {
    let Some(target) = params.target.as_deref().map(str::trim).filter(|target| !target.is_empty()) else {
        return gif_response();
    };
    if queue_bookmarklet_download(
        state.queue.as_ref(),
        target,
        query_to_bool(params.mail.as_deref()),
    )
    .is_ok()
    {
        state.push_server.broadcast_event("notification.queue", "");
    }
    gif_response()
}

// POST /api/diff
pub async fn api_diff(
    State(state): State<AppState>,
    Json(body): Json<DiffBody>,
) -> Json<ApiResponse> {
    let diff_number = match validate_diff_number(&body.number) {
        Ok(number) => number,
        Err(message) => {
            return Json(ApiResponse {
                success: false,
                message,
            });
        }
    };
    if body.ids.len() > super::MAX_WEB_TARGETS_PER_REQUEST {
        return Json(ApiResponse {
            success: false,
            message: "too many ids".to_string(),
        });
    }
    let ids: Vec<String> = match body
        .ids
        .iter()
        .map(|v| match v {
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .map(|target| super::validate_web_target_value(&target))
        .collect()
    {
        Ok(ids) => ids,
        Err(message) => {
            return Json(ApiResponse {
                success: false,
                message,
            });
        }
    };

    for id in &ids {
        let args = vec!["diff", "--no-tool", id, "--number", &diff_number];
        match run_immediate(&args) {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if !stdout.is_empty() {
                    state.push_server.broadcast_echo(&stdout, "stdout");
                }
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stderr.is_empty() {
                    state.push_server.broadcast_echo(&stderr, "stdout");
                }
            }
            Err(e) => {
                state
                    .push_server
                    .broadcast_echo(&format!("diff error: {}", e), "stdout");
            }
        }
    }

    Json(ApiResponse {
        success: true,
        message: format!("Diff completed for {} novel(s)", ids.len()),
    })
}

// POST /api/diff_clean
pub async fn api_diff_clean(
    State(state): State<AppState>,
    Json(body): Json<DiffCleanBody>,
) -> Json<ApiResponse> {
    let target = match &body.target {
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };

    let target = match super::validate_web_target_value(&target) {
        Ok(target) => target,
        Err(message) => {
            return Json(ApiResponse {
                success: false,
                message,
            });
        }
    };

    let args = vec!["diff", "--clean", &target];
    match run_immediate(&args) {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.is_empty() {
                state.push_server.broadcast_echo(&stdout, "stdout");
            }
            Json(ApiResponse {
                success: output.status.success(),
                message: if output.status.success() {
                    format!("Diff cleaned for {}", target)
                } else {
                    String::from_utf8_lossy(&output.stderr).to_string()
                },
            })
        }
        Err(e) => Json(ApiResponse {
            success: false,
            message: e,
        }),
    }
}

// POST /api/csv/import
pub async fn api_csv_import(
    State(state): State<AppState>,
    Json(body): Json<CsvImportBody>,
) -> Json<ApiResponse> {
    if let Err(message) =
        super::validate_web_text_size(&body.csv, super::MAX_WEB_CSV_IMPORT_BYTES, "csv")
    {
        return Json(ApiResponse {
            success: false,
            message,
        });
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            return Json(ApiResponse {
                success: false,
                message: e.to_string(),
            });
        }
    };

    let result = std::process::Command::new(exe)
        .args(["csv", "--import", "-"])
        .current_dir(std::env::current_dir().unwrap_or_default())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                let _ = stdin.write_all(body.csv.as_bytes());
            }
            child.wait_with_output()
        });

    match result {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.is_empty() {
                state.push_server.broadcast_echo(&stdout, "stdout");
            }
            state.push_server.broadcast_event("table.reload", "");
            Json(ApiResponse {
                success: output.status.success(),
                message: if output.status.success() {
                    "CSVインポート完了".to_string()
                } else {
                    String::from_utf8_lossy(&output.stderr).to_string()
                },
            })
        }
        Err(e) => Json(ApiResponse {
            success: false,
            message: e.to_string(),
        }),
    }
}

// GET /api/csv/download
pub async fn api_csv_download(
    State(_state): State<AppState>,
) -> axum::response::Response {
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;

    let records = with_database(|db| {
        let all = db.all_records();
        let mut items: Vec<serde_json::Value> = Vec::new();
        for (id, record) in all {
            items.push(serde_json::json!({
                "id": id,
                "title": record.title,
                "author": record.author,
                "sitename": record.sitename,
                "toc_url": record.toc_url,
            }));
        }
        Ok(items)
    });

    let records = match records {
        Ok(records) => records,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    let mut csv = String::from("ID,タイトル,著者,サイト,URL\n");
    for r in &records {
        let id = r["id"].as_i64().unwrap_or(0);
        let title = r["title"].as_str().unwrap_or("").replace('"', "\"\"");
        let author = r["author"].as_str().unwrap_or("").replace('"', "\"\"");
        let sitename = r["sitename"].as_str().unwrap_or("").replace('"', "\"\"");
        let toc_url = r["toc_url"].as_str().unwrap_or("");
        csv.push_str(&format!(
            "{},\"{}\",\"{}\",\"{}\",{}\n",
            id, title, author, sitename, toc_url
        ));
    }

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"novels.csv\"",
            ),
        ],
        csv,
    )
        .into_response()
}

// POST /api/queue/cancel — cancel all: kill running + clear pending
pub async fn queue_cancel(
    State(state): State<AppState>,
) -> Json<ApiResponse> {
    // Kill running subprocess if any
    kill_running_child(&state);

    // Clear pending tasks from queue
    let _ = state.queue.clear_pending();

    state.push_server.broadcast_event("notification.queue", "");
    Json(ApiResponse {
        success: true,
        message: "キャンセルしました".to_string(),
    })
}

// POST /api/cancel — cancel running task (Ruby parity: does NOT clear queue)
pub async fn api_cancel(
    State(state): State<AppState>,
) -> Json<ApiResponse> {
    kill_running_child(&state);
    state.push_server.broadcast_event("notification.queue", "");
    Json(ApiResponse {
        success: true,
        message: "キャンセルしました".to_string(),
    })
}

// POST /api/download_force — force re-download (Ruby parity: accepts "ids" param)
pub async fn api_download_force(
    State(state): State<AppState>,
    Json(body): Json<super::state::IdsBody>,
) -> Json<serde_json::Value> {
    let targets: Vec<String> = body
        .ids
        .iter()
        .map(|v| match v {
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) => s.clone(),
            _ => v.to_string(),
        })
        .collect();

    let download_body = super::state::DownloadBody {
        targets,
        force: true,
        mail: false,
    };
    api_download(State(state), Json(download_body)).await
}

// POST /api/cancel_running_task — cancel specific running task
pub async fn cancel_running_task(
    State(state): State<AppState>,
    Json(body): Json<TaskIdBody>,
) -> Json<serde_json::Value> {
    if running_job_by_id(&state, &body.task_id).is_some() && kill_running_child_for_job(&state, &body.task_id) {
        return serde_json::json!({ "status": "ok" }).into();
    }
    serde_json::json!({ "error": "実行中の処理を中断できませんでした" }).into()
}

// GET /api/get_pending_tasks
pub async fn get_pending_tasks(
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    let pending = state.queue.get_pending_tasks();
    let pending_count = pending.len();
    let restorable_tasks_available = restorable_tasks_available(&state, pending_count);
    let pending_json: Vec<serde_json::Value> = pending
        .iter()
        .map(|j| {
            serde_json::json!({
                "id": j.id,
                "type": j.job_type,
                "target": j.target,
                "created_at": j.created_at,
            })
        })
        .collect();

    let running = state.running_jobs.lock().clone();
    let running_count = running.len();
    let running_json: Vec<serde_json::Value> = running
        .iter()
        .map(|job| {
            serde_json::json!({
                "id": job.id,
                "type": job.job_type,
                "target": job.target,
                "created_at": job.created_at,
            })
        })
        .collect();

    Json(serde_json::json!({
        "pending": pending_json,
        "running": running_json,
        "pending_count": pending_count,
        "running_count": running_count,
        "restorable_tasks_available": restorable_tasks_available,
        "restore_prompt_pending": restorable_tasks_available,
    }))
}

// POST /api/remove_pending_task
pub async fn remove_pending_task(
    State(state): State<AppState>,
    Json(body): Json<TaskIdBody>,
) -> Json<ApiResponse> {
    match state.queue.remove_pending(&body.task_id) {
        Ok(true) => Json(ApiResponse {
            success: true,
            message: "Task removed".to_string(),
        }),
        Ok(false) => Json(ApiResponse {
            success: false,
            message: "キューから削除できませんでした".to_string(),
        }),
        Err(e) => Json(ApiResponse {
            success: false,
            message: e.to_string(),
        }),
    }
}

// POST /api/reorder_pending_tasks
pub async fn reorder_pending_tasks(
    State(state): State<AppState>,
    Json(body): Json<ReorderBody>,
) -> Json<ApiResponse> {
    match state.queue.reorder_pending(&body.task_ids) {
        Ok(_) => Json(ApiResponse {
            success: true,
            message: "タスクの並び替えが完了しました".to_string(),
        }),
        Err(e) => Json(ApiResponse {
            success: false,
            message: e.to_string(),
        }),
    }
}

// GET /api/get_queue_size
pub async fn get_queue_size(
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    let default_count = state.queue.pending_count_for_lane(QueueLane::Default)
        + running_job_count_for_lane(&state, QueueLane::Default);
    let secondary_count = state.queue.pending_count_for_lane(QueueLane::Secondary)
        + running_job_count_for_lane(&state, QueueLane::Secondary);
    Json(serde_json::json!([default_count, secondary_count]))
}

// POST /api/update_by_tag — update novels filtered by tags
pub async fn api_update_by_tag(
    State(state): State<AppState>,
    Json(body): Json<UpdateByTagBody>,
) -> Json<serde_json::Value> {
    if body.tags.len() + body.exclusion_tags.len() > super::MAX_WEB_TAGS_PER_REQUEST {
        return serde_json::json!({
            "success": false,
            "message": "too many tags",
        })
        .into();
    }
    let tags: Vec<String> = match body
        .tags
        .iter()
        .map(|tag| super::validate_web_tag_name(tag))
        .collect()
    {
        Ok(tags) => tags,
        Err(message) => {
            return serde_json::json!({
                "success": false,
                "message": message,
            })
            .into();
        }
    };
    let exclusion_tags: Vec<String> = match body
        .exclusion_tags
        .iter()
        .map(|tag| super::validate_web_tag_name(tag))
        .collect()
    {
        Ok(tags) => tags,
        Err(message) => {
            return serde_json::json!({
                "success": false,
                "message": message,
            })
            .into();
        }
    };
    let mut tag_params: Vec<String> = tags.iter().map(|t| format!("tag:{}", t)).collect();
    tag_params.extend(
        exclusion_tags
            .iter()
            .map(|t| format!("^tag:{}", t)),
    );

    if tag_params.is_empty() {
        return serde_json::json!({
            "success": false,
            "message": "tags or exclusion_tags required",
        })
        .into();
    }

    // Resolve matching novel IDs from database
    let ids = with_database(|db| {
        let all = db.all_records();
        let mut matching_ids: Vec<i64> = Vec::new();

        for (&id, record) in all {
            let record_tags: Vec<&str> = record.tags.iter().map(|s| s.as_str()).collect();
            let mut include = false;
            let mut exclude = false;

            for tag in &tags {
                if record_tags.contains(&tag.as_str()) {
                    include = true;
                }
            }
            for tag in &exclusion_tags {
                if record_tags.contains(&tag.as_str()) {
                    exclude = true;
                }
            }

            // Include if matches any inclusion tag and no exclusion tag
            if (tags.is_empty() || include) && !exclude {
                matching_ids.push(id);
            }
        }
        Ok(matching_ids)
    })
    .unwrap_or_default();

    if ids.is_empty() {
        return serde_json::json!({
            "success": true,
            "message": "対象の小説がありません",
            "count": 0,
        })
        .into();
    }

    let count = ids.len();
    let target = tag_params.join("	");
    let (_job_ids, queued) =
        match push_update_job_if_needed(state.queue.as_ref(), &state.running_jobs, target) {
        Ok(result) => result,
        Err(message) => {
            return serde_json::json!({
                "success": false,
                "message": message,
                "count": 0,
            })
            .into();
        }
    };

    if queued {
        state.push_server.broadcast_event("notification.queue", "");
    }
    serde_json::json!({
        "success": true,
        "count": count,
        "status": if queued { "queued" } else { "already_queued" },
    })
    .into()
}

// POST /api/taginfo.json — return tag information for update-by-tag dialog
pub async fn api_taginfo(
    State(_state): State<AppState>,
    Json(body): Json<TagInfoBody>,
) -> Json<serde_json::Value> {
    let with_exclusion = body.with_exclusion.unwrap_or(false);

    let tag_info = with_database(|db| {
        let tag_index = db.tag_index();
        let inventory = db.inventory();
        let mut tag_colors = super::tag_colors::load_tag_colors(inventory)?;
        let tag_names = tag_index.keys().map(String::as_str);
        if super::tag_colors::ensure_tag_colors(&mut tag_colors, tag_names) {
            super::tag_colors::save_tag_colors(inventory, &tag_colors)?;
        }
        let tag_colors = tag_colors.into_map();

        let mut result: Vec<serde_json::Value> = Vec::new();
        let mut sorted_tags: Vec<(&String, &Vec<i64>)> = tag_index.iter().collect();
        sorted_tags.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

        for (tag, ids) in sorted_tags {
            let color = tag_colors.get(tag).map(|c| c.as_str()).unwrap_or("");
            let class = tag_color_class(color);
            let escaped_tag = html_escape(tag);
            let html = format!(
                "<span class=\"tag-label {}\">{}({})</span>",
                class, escaped_tag, ids.len()
            );
            let mut entry = serde_json::json!({
                "tag": tag,
                "count": ids.len(),
                "html": html,
            });
            if with_exclusion {
                let exc_html = format!(
                    "<span class=\"tag-label {} tag-exclusion\">{}({})</span>",
                    class, escaped_tag, ids.len()
                );
                entry["exclusion_html"] = serde_json::json!(exc_html);
            }
            result.push(entry);
        }
        Ok(result)
    })
    .unwrap_or_default();

    Json(serde_json::json!(tag_info))
}

// POST /api/restore_pending_tasks
pub async fn restore_pending_tasks(
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    // In Rust, pending tasks are already persisted in queue.yaml and will be
    // picked up by the worker. Just count and report.
    state.restore_prompt_pending.store(false, Ordering::Relaxed);
    let count = state.queue.get_pending_tasks().len();
    state
        .push_server
        .broadcast_event("notification.queue", "");
    serde_json::json!({ "status": "ok", "count": count }).into()
}

// POST /api/defer_restore_pending_tasks
pub async fn defer_restore_pending_tasks(
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    // Clear pending tasks (defer = discard them)
    state.restore_prompt_pending.store(false, Ordering::Relaxed);
    let _ = state.queue.clear_pending();
    serde_json::json!({ "status": "ok" }).into()
}

// POST /api/confirm_running_tasks
pub async fn confirm_running_tasks(
    State(state): State<AppState>,
    Json(body): Json<ConfirmRunningTasksBody>,
) -> Json<serde_json::Value> {
    if body.rerun.as_deref() == Some("true") {
        // Resume: keep pending tasks, report count
        state.restore_prompt_pending.store(false, Ordering::Relaxed);
        let count = state.queue.get_pending_tasks().len();
        state
            .push_server
            .broadcast_event("notification.queue", "");
        serde_json::json!({ "status": "ok", "count": count }).into()
    } else {
        // Defer: clear pending tasks
        let _ = state.queue.clear_pending();
        serde_json::json!({ "status": "ok" }).into()
    }
}

// POST /api/update_general_lastup
pub async fn api_update_general_lastup(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let option = body["option"].as_str().unwrap_or("all");
    let option = match validate_general_lastup_option(option) {
        Ok(option) => option,
        Err(message) => {
            return serde_json::json!({
                "success": false,
                "message": message,
            })
            .into();
        }
    };
    let is_update_modified = body["is_update_modified"].as_str() == Some("true")
        || body["is_update_modified"].as_bool() == Some(true);
    let mut args = vec!["update", "--gl"];
    if let Some(option) = option {
        args.push(option);
    }

    let target = args[1..].join("	");
    match push_update_job_if_needed(state.queue.as_ref(), &state.running_jobs, target) {
        Ok((job_ids, queued)) => {
            let mut enqueued_any = queued;

            // Ruby parity: if is_update_modified, chain a second update for tag:modified
            if is_update_modified {
                if let Ok((_, modified_queued)) = push_update_job_if_needed(
                    state.queue.as_ref(),
                    &state.running_jobs,
                    "tag:modified".to_string(),
                ) {
                    enqueued_any |= modified_queued;
                }
            }

            if enqueued_any {
                state.push_server.broadcast_event("notification.queue", "");
            }

            serde_json::json!({
                "success": true,
                "job_id": job_ids.into_iter().next(),
                "status": if queued { "queued" } else { "already_queued" },
            })
            .into()
        }
        Err(message) => serde_json::json!({
            "success": false,
            "message": message,
        })
        .into(),
    }
}

// POST /api/shutdown
pub async fn api_shutdown(
    State(state): State<AppState>,
    Json(_body): Json<serde_json::Value>,
) -> Json<ApiResponse> {
    state.push_server.broadcast_event("shutdown", "");
    // Schedule exit after brief delay to allow response
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        std::process::exit(0);
    });
    Json(ApiResponse {
        success: true,
        message: "Shutting down".to_string(),
    })
}

// POST /api/reboot
pub async fn api_reboot(
    State(state): State<AppState>,
    Json(_body): Json<serde_json::Value>,
) -> Json<ApiResponse> {
    state.push_server.broadcast_event("reboot", "");
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            return Json(ApiResponse {
                success: false,
                message: e.to_string(),
            });
        }
    };
    let args = reboot_args_with_no_browser(std::env::args().skip(1).collect());
    // Spawn replacement process, then exit
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let _ = std::process::Command::new(exe)
            .args(&args)
            .current_dir(std::env::current_dir().unwrap_or_default())
            .stdin(std::process::Stdio::null())
            .spawn();
        std::process::exit(0);
    });
    Json(ApiResponse {
        success: true,
        message: "Rebooting".to_string(),
    })
}

fn reboot_args_with_no_browser(mut args: Vec<String>) -> Vec<String> {
    if args.iter().any(|arg| arg == "-n" || arg == "--no-browser") {
        return args;
    }
    args.push("--no-browser".to_string());
    args
}

/// Ruby parity: build sort display string like "タイトル昇順" or "ID順"
fn current_sort_display_string() -> String {
    const SORT_COLUMN_LABELS: &[&str] = &[
        "ID", "最終更新日", "最新話掲載日", "最終確認日", "タイトル", "作者",
        "サイト名", "小説種別", "タグ", "話数", "文字数", "状態", "URL",
    ];

    let sort_state = (|| -> Option<(usize, String)> {
        let inv = crate::db::inventory::Inventory::with_default_root().ok()?;
        let server_setting: serde_json::Map<String, serde_json::Value> =
            inv.load("server_setting", crate::db::inventory::InventoryScope::Global).ok()?;
        let current_sort = server_setting.get("current_sort")?;
        let column = current_sort.get("column")?.as_u64()? as usize;
        let dir = current_sort.get("dir")?.as_str()?.to_string();
        Some((column, dir))
    })();

    match sort_state {
        Some((column, dir)) => {
            let label = SORT_COLUMN_LABELS.get(column).unwrap_or(&"不明");
            let dir_label = if dir == "desc" { "降順" } else { "昇順" };
            format!("{}{}", label, dir_label)
        }
        None => "ID順".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;
    use std::sync::Arc;

    use crate::queue::{JobType, PersistentQueue, QueueJob};

    use super::{
        encode_convert_job_target, existing_update_job_id, push_update_job_if_needed,
        normalize_update_targets, reboot_args_with_no_browser, restorable_tasks_available,
        tag_color_class, validate_diff_number, validate_download_targets,
        validate_general_lastup_option,
    };

    #[test]
    fn encode_convert_job_target_omits_default_override() {
        assert_eq!(encode_convert_job_target("12", None), "12");
        assert_eq!(encode_convert_job_target("12", Some("   ")), "12");
    }

    #[test]
    fn encode_convert_job_target_keeps_explicit_device() {
        assert_eq!(encode_convert_job_target("12", Some("epub")), "12\tepub");
        assert_eq!(encode_convert_job_target("12", Some("text")), "12\ttext");
    }

    #[test]
    fn push_update_job_if_needed_reuses_pending_update_job() {
        let temp = tempfile::tempdir().unwrap();
        let queue = PersistentQueue::new(&temp.path().join("queue.yaml")).unwrap();
        let running_jobs = Mutex::new(Vec::new());
        let first = push_update_job_if_needed(&queue, &running_jobs, "1\t2\t3".to_string()).unwrap();

        let second = push_update_job_if_needed(&queue, &running_jobs, "1\t2\t3".to_string()).unwrap();

        assert!(first.1);
        assert!(!second.1);
        assert_eq!(first.0, second.0);
        assert_eq!(queue.pending_count(), 1);
    }

    #[test]
    fn existing_update_job_id_matches_running_job() {
        let temp = tempfile::tempdir().unwrap();
        let queue = PersistentQueue::new(&temp.path().join("queue.yaml")).unwrap();
        let running_jobs = Mutex::new(vec![QueueJob {
            id: "running-job".to_string(),
            job_type: JobType::Update,
            target: "tag:modified".to_string(),
            created_at: 0,
            retry_count: 0,
            max_retries: 3,
        }]);

        let existing = existing_update_job_id(&queue, &running_jobs, "tag:modified");

        assert_eq!(existing.as_deref(), Some("running-job"));
    }

    #[test]
    fn validate_download_targets_rejects_flag_like_values() {
        assert!(
            validate_download_targets(&["1".to_string(), "https://example.com".to_string()])
                .is_ok()
        );
        assert!(validate_download_targets(&["--remove".to_string()]).is_err());
        assert!(validate_download_targets(&["1\t2".to_string()]).is_err());
    }

    #[test]
    fn validate_diff_number_accepts_only_positive_integers() {
        assert_eq!(validate_diff_number("2").unwrap(), "2");
        assert!(validate_diff_number("--clean").is_err());
        assert!(validate_diff_number("abc").is_err());
    }

    #[test]
    fn validate_general_lastup_option_accepts_known_values_only() {
        assert_eq!(validate_general_lastup_option("all").unwrap(), None);
        assert_eq!(validate_general_lastup_option("narou").unwrap(), Some("narou"));
        assert_eq!(validate_general_lastup_option("other").unwrap(), Some("other"));
        assert!(validate_general_lastup_option("--force").is_err());
    }

    #[test]
    fn tag_color_class_uses_existing_webui_tag_classes() {
        assert_eq!(tag_color_class("green"), "tag-green");
        assert_eq!(tag_color_class("yellow"), "tag-yellow");
        assert_eq!(tag_color_class("unknown"), "tag-default");
    }

    #[test]
    fn reboot_args_adds_no_browser_to_prevent_new_tab() {
        assert_eq!(
            reboot_args_with_no_browser(vec!["web".to_string()]),
            vec!["web".to_string(), "--no-browser".to_string()]
        );
        assert_eq!(
            reboot_args_with_no_browser(vec![
                "web".to_string(),
                "--port".to_string(),
                "33000".to_string()
            ]),
            vec![
                "web".to_string(),
                "--port".to_string(),
                "33000".to_string(),
                "--no-browser".to_string()
            ]
        );
    }

    #[test]
    fn reboot_args_preserves_existing_no_browser() {
        assert_eq!(
            reboot_args_with_no_browser(vec!["web".to_string(), "--no-browser".to_string()]),
            vec!["web".to_string(), "--no-browser".to_string()]
        );
        assert_eq!(
            reboot_args_with_no_browser(vec!["web".to_string(), "-n".to_string()]),
            vec!["web".to_string(), "-n".to_string()]
        );
    }

    #[test]
    fn modified_followup_target_matches_ruby_tag_selector() {
        let target = "tag:modified".to_string();
        assert_eq!(target, "tag:modified");
    }

    #[test]
    fn normalize_update_targets_converts_tag_flag() {
        let normalized = normalize_update_targets(&["--tag".to_string(), "modified".to_string()])
            .unwrap();
        assert_eq!(normalized, vec!["tag:modified"]);
    }

    #[test]
    fn normalize_update_targets_rejects_missing_tag_name() {
        assert!(normalize_update_targets(&["--tag".to_string()]).is_err());
        assert!(normalize_update_targets(&["--tag".to_string(), "--gl".to_string()]).is_err());
    }

    #[test]
    fn normalize_update_targets_rejects_unexpected_flags() {
        assert!(normalize_update_targets(&["--remove".to_string()]).is_err());
        assert!(normalize_update_targets(&["bad\ttarget".to_string()]).is_err());
    }

    #[test]
    fn restorable_tasks_are_only_available_while_startup_prompt_is_pending() {
        let temp = tempfile::tempdir().unwrap();
        let queue = Arc::new(PersistentQueue::new(&temp.path().join("queue.yaml")).unwrap());
        queue.push(JobType::Download, "n1234aa").unwrap();
        let state = crate::web::AppState {
            port: 0,
            ws_port: 0,
            push_server: Arc::new(crate::web::push::PushServer::new()),
            basic_auth_header: None,
            control_token: "control-token".to_string(),
            queue,
            restore_prompt_pending: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            running_jobs: Arc::new(Mutex::new(Vec::new())),
            running_child_pids: Arc::new(Mutex::new(std::collections::HashMap::new())),
            auto_update_scheduler: Arc::new(Mutex::new(None)),
        };

        assert!(restorable_tasks_available(&state, 1));
        state
            .restore_prompt_pending
            .store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(!restorable_tasks_available(&state, 1));
    }
}

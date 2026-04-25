use std::path::PathBuf;
use std::sync::atomic::Ordering;

use axum::{
    extract::{Form, Query, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Json, Response},
};
use serde::Deserialize;
use serde_yaml::{Mapping, Value};

use crate::db::{with_database, with_database_mut};
use crate::downloader::site_setting::SiteSetting;
use crate::downloader::types::{CACHE_SAVE_DIR, SECTION_SAVE_DIR, SectionFile};
use crate::downloader::{Downloader, TargetType};
use crate::queue::{JobType, PersistentQueue, QueueExecutionSpec, QueueJob, QueueLane};

use super::AppState;
use super::sort_state::{
    CurrentSortState, current_sort_from_server_setting, load_current_sort_state, request_sort_state,
    sort_column_key, sort_column_label, sort_ids_for_request, sort_records,
};
use super::state::{
    ApiResponse, ConfirmRunningTasksBody, ConvertBody, CsvImportBody, DiffBody, DiffCleanBody,
    DownloadBody, ReorderBody, TagInfoBody, TargetsBody, TaskIdBody, UpdateBody, UpdateByTagBody,
};

const WEBUI_UPDATE_START_PREFIX: &str = "__webui_update_start__=";
const TRANSPARENT_GIF: &[u8] = &[
    71, 73, 70, 56, 57, 97, 1, 0, 1, 0, 128, 0, 0, 0, 0, 0, 255, 255, 255, 33, 249, 4, 1, 0, 0, 0,
    0, 44, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 68, 1, 0, 59,
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

fn log_web_failure(context: &str, error: impl std::fmt::Display) {
    eprintln!("web {} failed: {}", context, error);
}

fn log_immediate_command_failure(context: &str, output: &std::process::Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.trim().is_empty() {
        eprintln!("web {} failed with status {}", context, output.status);
    } else {
        eprintln!(
            "web {} failed with status {}: {}",
            context,
            output.status,
            stderr.trim()
        );
    }
}

fn update_job_requests_modified(meta: &Mapping) -> bool {
    matches!(
        meta.get(Value::String("update_modified".to_string())),
        Some(Value::Bool(true))
    )
}

fn update_job_matches_request(
    queue: &PersistentQueue,
    job: &QueueJob,
    target: &str,
    legacy_cmd: &str,
    update_modified: bool,
) -> bool {
    if !matches!(job.job_type, JobType::Update) || job.target != target {
        return false;
    }

    let Some(spec) = queue.execution_spec(&job.id) else {
        return legacy_cmd != "update_general_lastup" || !update_modified;
    };
    if spec.cmd != legacy_cmd {
        return false;
    }

    legacy_cmd != "update_general_lastup" || update_job_requests_modified(&spec.meta) == update_modified
}

fn queue_target_fallback_text(target: &str) -> String {
    target
        .split('\t')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" / ")
}

fn describe_update_targets(targets: &[String]) -> String {
    match targets {
        [] => "全ての小説".to_string(),
        [target] => {
            if let Some(tag) = target.strip_prefix("tag:") {
                format!("タグ「{}」の小説", tag)
            } else if target.chars().all(|ch| ch.is_ascii_digit()) {
                format!("ID {} の小説", target)
            } else {
                target.to_string()
            }
        }
        _ => format!("{}件の小説", targets.len()),
    }
}

fn format_update_queue_target(args: &[String]) -> String {
    if let Some(message) = args
        .iter()
        .find_map(|arg| arg.strip_prefix(WEBUI_UPDATE_START_PREFIX))
    {
        return message.to_string();
    }

    if args.first().map(String::as_str) == Some("--gl") {
        return format_general_lastup_queue_target(&args[1..]);
    }

    let mut force = false;
    let mut targets = Vec::new();
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--force" {
            force = true;
            continue;
        }
        if arg == "--sort-by" {
            skip_next = true;
            continue;
        }
        if arg.starts_with(WEBUI_UPDATE_START_PREFIX) {
            continue;
        }
        targets.push(arg.clone());
    }

    let target_text = describe_update_targets(&targets);
    if force {
        format!("{}を凍結済みも含めて更新", target_text)
    } else {
        format!("{}を更新", target_text)
    }
}

fn format_general_lastup_queue_target(args: &[String]) -> String {
    match args.first().map(String::as_str) {
        Some("narou") => "なろうAPIで最新話掲載日を確認".to_string(),
        Some("other") => "その他サイトの最新話掲載日を確認".to_string(),
        _ => "最新話掲載日を確認".to_string(),
    }
}

fn format_queue_job_target(job: &QueueJob, spec: Option<&QueueExecutionSpec>) -> String {
    let Some(spec) = spec else {
        return queue_target_fallback_text(&job.target);
    };

    match spec.cmd.as_str() {
        "update_general_lastup" => format_general_lastup_queue_target(&spec.args),
        "update" | "update_by_tag" => format_update_queue_target(&spec.args),
        _ => queue_target_fallback_text(&job.target),
    }
}

fn job_type_key(job_type: JobType) -> &'static str {
    match job_type {
        JobType::Download => "download",
        JobType::Update => "update",
        JobType::AutoUpdate => "auto_update",
        JobType::Convert => "convert",
        JobType::Send => "send",
        JobType::Backup => "backup",
        JobType::Mail => "mail",
    }
}

fn format_queue_job_type(job: &QueueJob, spec: Option<&QueueExecutionSpec>) -> String {
    spec.map(|spec| spec.cmd.clone())
        .unwrap_or_else(|| job_type_key(job.job_type).to_string())
}

fn queue_display_target(queue: &PersistentQueue, job: &QueueJob) -> String {
    format_queue_job_target(job, queue.execution_spec(&job.id).as_ref())
}

fn queue_display_type(queue: &PersistentQueue, job: &QueueJob) -> String {
    format_queue_job_type(job, queue.execution_spec(&job.id).as_ref())
}

fn queue_lane_sizes(queue: &PersistentQueue) -> [usize; 2] {
    [
        queue.pending_count_for_lane(QueueLane::Default) + queue.running_count_for_lane(QueueLane::Default),
        queue.pending_count_for_lane(QueueLane::Secondary)
            + queue.running_count_for_lane(QueueLane::Secondary),
    ]
}

fn sort_records_for_web_update(records: &mut Vec<&crate::db::NovelRecord>, sort_state: &CurrentSortState) {
    sort_records(records, sort_state);
}

fn current_web_update_sort_state() -> CurrentSortState {
    load_current_sort_state()
}

fn web_update_sort_key_for_cli(sort_state: &CurrentSortState) -> Option<&'static str> {
    let key = sort_column_key(sort_state)?;
    matches!(
        key,
        "id" | "last_update" | "title" | "author" | "general_lastup" | "last_check_date"
    )
    .then_some(key)
}

fn current_web_update_sort_key_for_cli() -> Option<&'static str> {
    web_update_sort_key_for_cli(&current_web_update_sort_state())
}

fn sorted_update_all_ids() -> Vec<String> {
    let sort_state = current_web_update_sort_state();
    with_database(|db| {
        let mut records: Vec<_> = db.all_records().values().collect();
        sort_records_for_web_update(&mut records, &sort_state);
        Ok(records.into_iter().map(|record| record.id.to_string()).collect())
    })
    .unwrap_or_default()
}

fn sort_numeric_targets_for_request(
    targets: &[String],
    sort_state: Option<&serde_json::Value>,
    timestamp: Option<u64>,
) -> Vec<String> {
    let Some(ids) = targets
        .iter()
        .map(|target| target.parse::<i64>().ok())
        .collect::<Option<Vec<_>>>()
    else {
        return targets.to_vec();
    };
    let sorted_ids = sort_ids_for_request(&ids, sort_state, timestamp);
    sorted_ids
        .into_iter()
        .map(|id| id.to_string())
        .collect()
}

fn build_update_by_tag_queue_payload(tag_params: &[String], snapshot_ids: &[i64]) -> (String, Vec<Value>, Mapping) {
    let target = tag_params.join("\t");
    let legacy_args = tag_params
        .iter()
        .cloned()
        .map(Value::String)
        .collect::<Vec<_>>();
    let mut meta = Mapping::new();
    meta.insert(
        Value::String("snapshot_ids".to_string()),
        Value::Sequence(
            snapshot_ids
                .iter()
                .map(|id| Value::String(id.to_string()))
                .collect::<Vec<_>>(),
        ),
    );
    meta.insert(
        Value::String("snapshot_count".to_string()),
        Value::Number(serde_yaml::Number::from(snapshot_ids.len() as u64)),
    );
    (target, legacy_args, meta)
}

fn build_webui_update_start_message(is_update_all: bool, count: usize, sort_display: &str) -> String {
    if is_update_all {
        format!("全ての小説の更新を開始します（{}件を{}で処理）", count, sort_display)
    } else {
        format!("更新を開始します（{}件を{}で処理）", count, sort_display)
    }
}

fn queue_download_jobs(
    queue: &PersistentQueue,
    targets: &[String],
    force: bool,
    mail: bool,
) -> Result<Vec<String>, String> {
    validate_download_targets(targets)?;
    let jobs: Vec<(JobType, String)> = targets
        .iter()
        .map(|target| {
            let mut parts = Vec::new();
            if force {
                parts.push("--force".to_string());
            }
            if mail {
                parts.push("--mail".to_string());
            }
            parts.push(target.clone());
            (JobType::Download, parts.join("\t"))
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

fn bookmarklet_download_response(
    state: &AppState,
    params: &BookmarkletDownloadQuery,
) -> serde_json::Value {
    let Some(target) = params
        .target
        .as_deref()
        .map(str::trim)
        .filter(|target| !target.is_empty())
    else {
        return serde_json::json!({ "status": 2, "id": null });
    };
    if let Some(id) = resolve_existing_id_for_target(target) {
        return serde_json::json!({ "status": 1, "id": id });
    }
    match queue_bookmarklet_download(
        state.queue.as_ref(),
        target,
        query_to_bool(params.mail.as_deref()),
    ) {
        Ok(_) => {
            state.push_server.broadcast_event("notification.queue", "");
            serde_json::json!({ "status": 0, "id": null })
        }
        Err(error) => {
            log_web_failure("bookmarklet download", error);
            serde_json::json!({ "status": 2, "id": null })
        }
    }
}

fn render_post_only_notice(path: &str) -> Html<String> {
    let escaped_path = html_escape(path);
    Html(format!(
        r#"<!DOCTYPE html>
<html lang="ja">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>narou.rs</title>
</head>
<body>
  <p>{escaped_path} は POST 専用になりました。古いブックマークレットは利用できません。</p>
  <p><a href="/bookmarklet">/bookmarklet</a> から最新版を再登録してください。</p>
</body>
</html>"#
    ))
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
            let setting = settings
                .iter()
                .find(|setting| setting.matches_url(target))?;
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
    legacy_cmd: &str,
    update_modified: bool,
) -> Option<String> {
    if let Some(job) = running_jobs
        .lock()
        .iter()
        .find(|job| update_job_matches_request(queue, job, target, legacy_cmd, update_modified))
    {
        return Some(job.id.clone());
    }

    queue
        .get_running_tasks()
        .into_iter()
        .chain(queue.get_pending_tasks())
        .find(|job| update_job_matches_request(queue, job, target, legacy_cmd, update_modified))
        .map(|job| job.id)
}

fn push_update_job_if_needed(
    queue: &PersistentQueue,
    running_jobs: &parking_lot::Mutex<Vec<QueueJob>>,
    target: String,
) -> Result<(Vec<String>, bool), String> {
    if let Some(existing_id) = existing_update_job_id(queue, running_jobs, &target, "update", false)
    {
        return Ok((vec![existing_id], false));
    }

    queue
        .push_batch(&[(JobType::Update, target)])
        .map(|ids| (ids, true))
        .map_err(|e| e.to_string())
}

fn push_update_job_with_legacy_if_needed(
    queue: &PersistentQueue,
    running_jobs: &parking_lot::Mutex<Vec<QueueJob>>,
    target: String,
    legacy_cmd: &str,
    legacy_args: Vec<Value>,
    meta: Mapping,
) -> Result<(Vec<String>, bool), String> {
    let update_modified = update_job_requests_modified(&meta);
    if let Some(existing_id) =
        existing_update_job_id(queue, running_jobs, &target, legacy_cmd, update_modified)
    {
        return Ok((vec![existing_id], false));
    }
    queue
        .push_with_legacy(
            JobType::Update,
            &target,
            legacy_cmd,
            legacy_args,
            meta,
        )
        .map(|id| (vec![id], true))
        .map_err(|e| e.to_string())
}

fn build_update_general_lastup_meta(is_update_modified: bool) -> Mapping {
    let mut meta = Mapping::new();
    if is_update_modified {
        meta.insert(
            Value::String("update_modified".to_string()),
            Value::Bool(true),
        );
        if let Some(sort_key) = current_web_update_sort_key_for_cli() {
            meta.insert(
                Value::String("sort_by".to_string()),
                Value::String(sort_key.to_string()),
            );
        }
    }
    meta
}

fn restorable_tasks_available(state: &AppState) -> bool {
    state.restorable_tasks_available.load(Ordering::Relaxed) && state.queue.has_restorable_tasks()
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
    let jobs: Vec<(String, u32)> = {
        let mut guard = state.running_child_pids.lock();
        let values = guard.iter().map(|(job_id, pid)| (job_id.clone(), *pid)).collect();
        guard.clear();
        values
    };
    for (job_id, pid) in jobs {
        state.cancelled_job_ids.lock().insert(job_id);
        kill_process_tree(pid, &state.push_server);
    }
}

fn kill_running_child_for_job(state: &AppState, job_id: &str) -> bool {
    let pid = state.running_child_pids.lock().remove(job_id);
    if let Some(pid) = pid {
        state.cancelled_job_ids.lock().insert(job_id.to_string());
        kill_process_tree(pid, &state.push_server);
        true
    } else {
        false
    }
}

fn kill_process_tree(pid: u32, push_server: &std::sync::Arc<super::push::PushServer>) {
    let result = crate::compat::terminate_process(pid);
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
        let a_name = a
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        let b_name = b
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
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
    let Ok((record, archive_root)) =
        with_database(|db| Ok((db.get(id).cloned(), db.archive_root().to_path_buf())))
    else {
        return String::new();
    };
    let Some(record) = record else {
        return String::new();
    };

    let Ok(base_dir) = super::safe_existing_novel_dir(&archive_root, &record) else {
        return String::new();
    };
    let cache_root = base_dir.join(SECTION_SAVE_DIR).join(CACHE_SAVE_DIR);
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

    notify_queue_changed(&state);
    serde_json::json!({ "success": true, "results": results }).into()
}

pub async fn api_update(
    State(state): State<AppState>,
    Json(body): Json<UpdateBody>,
) -> Json<serde_json::Value> {
    let raw_targets = targets_to_strings(&body.targets);
    let targets = match normalize_update_targets(&raw_targets) {
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

    let is_update_all = body.update_all || targets.is_empty();
    let count;
    let combined = if has_flags {
        count = 1;
        targets.join("\t")
    } else {
        let mut args = if is_update_all {
            sorted_update_all_ids()
        } else {
            sort_numeric_targets_for_request(&targets, body.sort_state.as_ref(), body.timestamp)
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
        let sort_display = if is_update_all {
            current_sort_display_string()
        } else {
            requested_sort_display_string(body.sort_state.as_ref(), body.timestamp)
        };
        let start_message = build_webui_update_start_message(is_update_all, count, &sort_display);
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
            super::validate_web_target_value(target).map_err(|_| "invalid target".to_string())?,
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
    let ordered_targets =
        sort_numeric_targets_for_request(&targets, body.sort_state.as_ref(), body.timestamp);
    let mut meta = Mapping::new();
    if let Some(device) = device.as_deref() {
        meta.insert(
            Value::String("device".to_string()),
            Value::String(device.to_string()),
        );
    }
    let legacy_args = ordered_targets
        .iter()
        .cloned()
        .map(Value::String)
        .collect::<Vec<_>>();
    let job_id = match state.queue.push_with_legacy(
        JobType::Convert,
        &encode_convert_job_target(&ordered_targets),
        "convert",
        legacy_args,
        meta,
    ) {
        Ok(id) => id,
        Err(e) => {
            return serde_json::json!({
                "success": false,
                "message": e.to_string(),
                "results": []
            })
            .into();
        }
    };

    let results: Vec<serde_json::Value> = ordered_targets
        .iter()
        .map(|target| {
            serde_json::json!({
                "target": target,
                "device": device,
                "job_id": job_id,
                "status": "queued"
            })
        })
        .collect();

    notify_queue_changed(&state);
    serde_json::json!({ "success": true, "results": results }).into()
}

pub async fn queue_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    let running_jobs = state.queue.get_running_tasks();
    let running_count = running_jobs.len();
    let lane_sizes = queue_lane_sizes(state.queue.as_ref());
    let running_label = match running_jobs.as_slice() {
        [] => serde_json::Value::Null,
        [job] => serde_json::Value::String(queue_display_target(state.queue.as_ref(), job)),
        jobs => serde_json::Value::String(format!("{} 件実行中", jobs.len())),
    };
    Json(serde_json::json!({
        "pending": state.queue.pending_count(),
        "completed": state.queue.completed_count(),
        "partial": state.queue.partial_count(),
        "failed": state.queue.failed_count(),
        "cancelled": state.queue.cancelled_count(),
        "running": running_label,
        "running_count": running_count,
        "lane_sizes": lane_sizes,
    }))
}

pub async fn queue_clear(State(state): State<AppState>) -> Json<ApiResponse> {
    match state.queue.clear_non_running() {
        Ok(_) => {
            state.restore_prompt_pending.store(false, Ordering::Relaxed);
            state
                .restorable_tasks_available
                .store(false, Ordering::Relaxed);
            notify_queue_changed(&state);
            Json(ApiResponse {
                success: true,
                message: if state.queue.running_count() > 0 {
                    "Queue cleared (running tasks kept)".to_string()
                } else {
                    "Queue cleared".to_string()
                },
            })
        }
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

fn encode_convert_job_target(targets: &[String]) -> String {
    targets.join("\t")
}

fn run_immediate_blocking(args: &[String]) -> Result<std::process::Output, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut command = std::process::Command::new(exe);
    command
        .args(args)
        .current_dir(std::env::current_dir().unwrap_or_default())
        .stdin(std::process::Stdio::null());
    crate::compat::configure_web_subprocess_command(&mut command);
    command.output().map_err(|e| e.to_string())
}

async fn run_immediate(args: Vec<String>) -> Result<std::process::Output, String> {
    tokio::task::spawn_blocking(move || run_immediate_blocking(&args))
        .await
        .map_err(|e| e.to_string())?
}

pub(crate) async fn run_cli_and_broadcast(
    state: &AppState,
    args: Vec<String>,
    target_console: &str,
) -> Result<std::process::Output, String> {
    let output = run_immediate(args).await?;
    broadcast_captured_web_output(&state.push_server, &output.stdout, target_console);
    broadcast_captured_web_output(&state.push_server, &output.stderr, target_console);
    Ok(output)
}

fn broadcast_captured_web_output(
    push_server: &std::sync::Arc<super::push::PushServer>,
    output: &[u8],
    target_console: &str,
) {
    for line in String::from_utf8_lossy(output).lines() {
        let routed = crate::compat::reroute_web_line_to_console(line, target_console);
        if let Some(json) = routed.strip_prefix(crate::progress::WS_LINE_PREFIX)
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(json)
        {
            push_server.broadcast_raw(&value);
        }
    }
}

fn notify_queue_changed(state: &AppState) {
    state.push_server.broadcast_event("notification.queue", "");
}

async fn prepare_process_shutdown(state: &AppState) {
    kill_running_child(state);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while state.queue.running_count() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    if let Err(e) = state.queue.flush() {
        state
            .push_server
            .broadcast_echo(&format!("キュー保存に失敗: {}", e), "stdout");
    }
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
    let ordered_targets =
        sort_numeric_targets_for_request(&targets, body.sort_state.as_ref(), body.timestamp);
    let legacy_args = ordered_targets
        .iter()
        .cloned()
        .map(Value::String)
        .collect::<Vec<_>>();
    let job_id = match state.queue.push_with_legacy(
        JobType::Send,
        &ordered_targets.join("\t"),
        "send",
        legacy_args,
        Mapping::new(),
    ) {
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
        "count": ordered_targets.len(),
        "job_ids": [job_id],
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
    let mut args = vec!["inspect".to_string()];
    args.extend(targets);
    match run_cli_and_broadcast(&state, args, super::non_external_console_target()).await {
        Ok(output) => {
            Json(ApiResponse {
                success: output.status.success(),
                message: if output.status.success() {
                    "OK".to_string()
                } else {
                    log_immediate_command_failure("inspect", &output);
                    "inspect の実行に失敗しました".to_string()
                },
            })
        }
        Err(e) => {
            log_web_failure("inspect", &e);
            Json(ApiResponse {
                success: false,
                message: "inspect の実行に失敗しました".to_string(),
            })
        }
    }
}

// POST /api/folder
pub async fn api_folder(
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
    let mut args = vec!["folder".to_string()];
    args.extend(targets);
    match run_cli_and_broadcast(&state, args, super::non_external_console_target()).await {
        Ok(output) => {
            Json(ApiResponse {
                success: output.status.success(),
                message: if output.status.success() {
                    "OK".to_string()
                } else {
                    log_immediate_command_failure("folder", &output);
                    "folder の実行に失敗しました".to_string()
                },
            })
        }
        Err(e) => {
            log_web_failure("folder", &e);
            Json(ApiResponse {
                success: false,
                message: "folder の実行に失敗しました".to_string(),
            })
        }
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
    let ordered_targets =
        sort_numeric_targets_for_request(&targets, body.sort_state.as_ref(), body.timestamp);
    let legacy_args = ordered_targets
        .iter()
        .cloned()
        .map(Value::String)
        .collect::<Vec<_>>();
    let job_id = match state.queue.push_with_legacy(
        JobType::Backup,
        &ordered_targets.join("\t"),
        "backup",
        legacy_args,
        Mapping::new(),
    ) {
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
        "count": ordered_targets.len(),
        "job_ids": [job_id],
    })
    .into()
}

// POST /api/backup_bookmark — backup bookmarks from device
pub async fn api_backup_bookmark(State(state): State<AppState>) -> Json<serde_json::Value> {
    // Ruby parity: runs "send --backup-bookmark"
    let job_id = match state.queue.push_with_legacy(
        JobType::Send,
        "--backup-bookmark",
        "backup_bookmark",
        Vec::new(),
        Mapping::new(),
    ) {
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
    let jobs: Vec<(JobType, String)> = targets.iter().map(|t| (JobType::Mail, t.clone())).collect();
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
    let mut args = vec!["setting".to_string(), "--burn".to_string()];
    args.extend(targets);
    match run_cli_and_broadcast(&state, args, super::non_external_console_target()).await {
        Ok(output) => {
            Json(ApiResponse {
                success: output.status.success(),
                message: if output.status.success() {
                    "設定を焼き込みました".to_string()
                } else {
                    log_immediate_command_failure("setting --burn", &output);
                    "設定の焼き込みに失敗しました".to_string()
                },
            })
        }
        Err(e) => {
            log_web_failure("setting --burn", &e);
            Json(ApiResponse {
                success: false,
                message: "設定の焼き込みに失敗しました".to_string(),
            })
        }
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
pub async fn bookmarklet_download_request_post_required(
    Query(_params): Query<BookmarkletDownloadQuery>,
) -> Html<String> {
    render_post_only_notice("/api/download_request")
}

// GET /api/download4ssl
pub async fn bookmarklet_download4ssl_post_required(
    Query(_params): Query<BookmarkletDownloadQuery>,
) -> Html<String> {
    render_post_only_notice("/api/download4ssl")
}

// POST /api/download_request
pub async fn api_download_request(
    State(state): State<AppState>,
    Form(params): Form<BookmarkletDownloadQuery>,
) -> Json<serde_json::Value> {
    Json(bookmarklet_download_response(&state, &params))
}

// GET /api/downloadable.gif
pub async fn api_downloadable_gif(
    State(_state): State<AppState>,
    Query(params): Query<BookmarkletDownloadQuery>,
) -> Response {
    let _number = match params.target.as_deref() {
        Some(target) if !target.trim().is_empty() => {
            if resolve_existing_id_for_target(target).is_some() {
                1
            } else {
                0
            }
        }
        _ => 2,
    };
    gif_response()
}

// POST /api/download4ssl
pub async fn api_download4ssl(
    State(state): State<AppState>,
    Form(params): Form<BookmarkletDownloadQuery>,
) -> Response {
    let _ = bookmarklet_download_response(&state, &params);
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
        let args = vec![
            "diff".to_string(),
            "--no-tool".to_string(),
            id.clone(),
            "--number".to_string(),
            diff_number.clone(),
        ];
        match run_cli_and_broadcast(&state, args, super::non_external_console_target()).await {
            Ok(output) => {
                if !output.status.success() {
                    log_immediate_command_failure("diff", &output);
                    return Json(ApiResponse {
                        success: false,
                        message: format!("diff の実行に失敗しました ({id})"),
                    });
                }
            }
            Err(e) => {
                log_web_failure("diff", &e);
                state.push_server.broadcast_echo(
                    &format!("diff error: {}", e),
                    super::non_external_console_target(),
                );
                return Json(ApiResponse {
                    success: false,
                    message: format!("diff の実行に失敗しました ({id})"),
                });
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

    let args = vec!["diff".to_string(), "--clean".to_string(), target.clone()];
    match run_cli_and_broadcast(&state, args, super::non_external_console_target()).await {
        Ok(output) => {
            Json(ApiResponse {
                success: output.status.success(),
                message: if output.status.success() {
                    format!("Diff cleaned for {}", target)
                } else {
                    log_immediate_command_failure("diff --clean", &output);
                    "差分の削除に失敗しました".to_string()
                },
            })
        }
        Err(e) => {
            log_web_failure("diff --clean", &e);
            Json(ApiResponse {
                success: false,
                message: "差分の削除に失敗しました".to_string(),
            })
        }
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
            log_web_failure("csv import", &e);
            return Json(ApiResponse {
                success: false,
                message: "CSVインポートに失敗しました".to_string(),
            });
        }
    };

    let mut command = std::process::Command::new(exe);
    command
        .args(["csv", "--import", "-"])
        .current_dir(std::env::current_dir().unwrap_or_default())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::compat::configure_web_subprocess_command(&mut command);
    let result = command.spawn().and_then(|mut child| {
        use std::io::Write;
        if let Some(ref mut stdin) = child.stdin {
            let _ = stdin.write_all(body.csv.as_bytes());
        }
        drop(child.stdin.take());
        child.wait_with_output()
    });

    match result {
        Ok(output) => {
            broadcast_captured_web_output(
                &state.push_server,
                &output.stdout,
                super::non_external_console_target(),
            );
            broadcast_captured_web_output(
                &state.push_server,
                &output.stderr,
                super::non_external_console_target(),
            );
            if output.status.success() {
                match with_database_mut(|db| db.refresh()) {
                    Ok(()) => {
                        state.push_server.broadcast_event("table.reload", "");
                        state.push_server.broadcast_event("tag.updateCanvas", "");
                        Json(ApiResponse {
                            success: true,
                            message: "CSVインポート完了".to_string(),
                        })
                    }
                    Err(e) => {
                        state
                            .push_server
                            .broadcast_error(&format!("DB更新エラー: {}", e));
                        Json(ApiResponse {
                            success: false,
                            message: "CSVインポート後のDB更新に失敗しました".to_string(),
                        })
                    }
                }
            } else {
                log_immediate_command_failure("csv import", &output);
                Json(ApiResponse {
                    success: false,
                    message: "CSVインポートに失敗しました".to_string(),
                })
            }
        }
        Err(e) => {
            log_web_failure("csv import", &e);
            Json(ApiResponse {
                success: false,
                message: "CSVインポートに失敗しました".to_string(),
            })
        }
    }
}

// GET /api/csv/download
pub async fn api_csv_download(State(_state): State<AppState>) -> axum::response::Response {
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
pub async fn queue_cancel(State(state): State<AppState>) -> Json<ApiResponse> {
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
pub async fn api_cancel(State(state): State<AppState>) -> Json<ApiResponse> {
    kill_running_child(&state);
    notify_queue_changed(&state);
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
    if running_job_by_id(&state, &body.task_id).is_some()
        && kill_running_child_for_job(&state, &body.task_id)
    {
        notify_queue_changed(&state);
        return serde_json::json!({ "status": "ok" }).into();
    }
    serde_json::json!({ "error": "実行中の処理を中断できませんでした" }).into()
}

// GET /api/get_pending_tasks
pub async fn get_pending_tasks(State(state): State<AppState>) -> Json<serde_json::Value> {
    let pending = state.queue.get_pending_tasks();
    let pending_count = pending.len();
    let restorable_tasks_available = restorable_tasks_available(&state);
    let pending_json: Vec<serde_json::Value> = pending
        .iter()
        .map(|j| {
            serde_json::json!({
                "id": j.id,
                "type": queue_display_type(state.queue.as_ref(), j),
                "target": j.target,
                "display_target": queue_display_target(state.queue.as_ref(), j),
                "created_at": j.created_at,
            })
        })
        .collect();

    let running = state.queue.get_running_tasks();
    let running_count = running.len();
    let running_json: Vec<serde_json::Value> = running
        .iter()
        .map(|job| {
            serde_json::json!({
                "id": job.id,
                "type": queue_display_type(state.queue.as_ref(), job),
                "target": job.target,
                "display_target": queue_display_target(state.queue.as_ref(), job),
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
        "restore_prompt_pending": state.restore_prompt_pending.load(Ordering::Relaxed),
    }))
}

// POST /api/remove_pending_task
pub async fn remove_pending_task(
    State(state): State<AppState>,
    Json(body): Json<TaskIdBody>,
) -> Json<ApiResponse> {
    match state.queue.remove_pending(&body.task_id) {
        Ok(true) => {
            state
                .restorable_tasks_available
                .store(state.queue.has_restorable_tasks(), Ordering::Relaxed);
            notify_queue_changed(&state);
            Json(ApiResponse {
                success: true,
                message: "Task removed".to_string(),
            })
        }
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
        Ok(true) => {
            notify_queue_changed(&state);
            Json(ApiResponse {
                success: true,
                message: "タスクの並び替えが完了しました".to_string(),
            })
        }
        Ok(false) => Json(ApiResponse {
            success: false,
            message: "キューの並び替えに失敗しました".to_string(),
        }),
        Err(e) => Json(ApiResponse {
            success: false,
            message: e.to_string(),
        }),
    }
}

// GET /api/get_queue_size
pub async fn get_queue_size(State(state): State<AppState>) -> Json<serde_json::Value> {
    let [default_count, secondary_count] = queue_lane_sizes(state.queue.as_ref());
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
    tag_params.extend(exclusion_tags.iter().map(|t| format!("^tag:{}", t)));

    if tag_params.is_empty() {
        return serde_json::json!({
            "success": false,
            "message": "tags or exclusion_tags required",
        })
        .into();
    }

    // Snapshot current matching novel IDs for UI feedback only.
    let snapshot_ids = with_database(|db| {
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

    let snapshot_ids =
        sort_ids_for_request(&snapshot_ids, body.sort_state.as_ref(), body.timestamp);
    let count = snapshot_ids.len();
    let (target, legacy_args, meta) =
        build_update_by_tag_queue_payload(&tag_params, &snapshot_ids);
    let (_job_ids, queued) = match push_update_job_with_legacy_if_needed(
        state.queue.as_ref(),
        &state.running_jobs,
        target.clone(),
        "update_by_tag",
        legacy_args,
        meta,
    ) {
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
    let selected_ids: Vec<i64> = body
        .ids
        .iter()
        .filter_map(|value| match value {
            serde_json::Value::Number(n) => n.as_i64(),
            serde_json::Value::String(s) => s.parse::<i64>().ok(),
            _ => None,
        })
        .collect();
    let selected_ids = sort_ids_for_request(&selected_ids, body.sort_state.as_ref(), body.timestamp);

    let tag_info = with_database(|db| {
        let tag_index = db.tag_index();
        let inventory = db.inventory();
        let mut tag_colors = super::tag_colors::load_tag_colors(inventory)?;
        let tag_names = tag_index.keys().map(String::as_str);
        if super::tag_colors::ensure_tag_colors(&mut tag_colors, tag_names) {
            super::tag_colors::save_tag_colors(inventory, &tag_colors)?;
        }
        let tag_colors = tag_colors.into_map();

        let mut selected_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for id in selected_ids.iter().copied() {
            let Some(record) = db.get(id) else {
                continue;
            };
            for tag in &record.tags {
                *selected_counts.entry(tag.clone()).or_insert(0) += 1;
            }
        }

        let mut sorted_tags: Vec<(&String, &Vec<i64>)> = tag_index.iter().collect();
        sorted_tags.sort_by(|a, b| a.0.cmp(b.0));

        let mut result: Vec<serde_json::Value> = Vec::with_capacity(sorted_tags.len());
        for (tag, ids) in sorted_tags {
            let color = tag_colors.get(tag).map(|c| c.as_str()).unwrap_or("");
            let class = tag_color_class(color);
            let escaped_tag = html_escape(tag);
            let html = format!(
                "<span class=\"tag-label {}\">{}</span>",
                class, escaped_tag
            );
            let mut entry = serde_json::json!({
                "tag": tag,
                "count": selected_counts.get(tag.as_str()).copied().unwrap_or(0),
                "total_count": ids.len(),
                "html": html,
            });
            if with_exclusion {
                let exc_html = format!(
                    "<span class=\"tag-label {} tag-exclusion\">{}</span>",
                    class, escaped_tag
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
pub async fn restore_pending_tasks(State(state): State<AppState>) -> Json<serde_json::Value> {
    let count = match state.queue.activate_restorable_tasks() {
        Ok(count) => count,
        Err(e) => return serde_json::json!({ "error": e.to_string() }).into(),
    };
    state.restore_prompt_pending.store(false, Ordering::Relaxed);
    state
        .restorable_tasks_available
        .store(false, Ordering::Relaxed);
    state.push_server.broadcast_event("notification.queue", "");
    serde_json::json!({ "status": "ok", "count": count }).into()
}

// POST /api/defer_restore_pending_tasks
pub async fn defer_restore_pending_tasks(State(state): State<AppState>) -> Json<serde_json::Value> {
    state.restore_prompt_pending.store(false, Ordering::Relaxed);
    match state.queue.defer_restorable_tasks() {
        Ok(_) => {
            state
                .restorable_tasks_available
                .store(state.queue.has_restorable_tasks(), Ordering::Relaxed);
            notify_queue_changed(&state);
            serde_json::json!({ "status": "ok" }).into()
        }
        Err(e) => serde_json::json!({ "error": e.to_string() }).into(),
    }
}

// POST /api/confirm_running_tasks
pub async fn confirm_running_tasks(
    State(state): State<AppState>,
    Json(body): Json<ConfirmRunningTasksBody>,
) -> Json<serde_json::Value> {
    if body.rerun.as_deref() == Some("true") {
        let count = match state.queue.activate_restorable_tasks() {
            Ok(count) => count,
            Err(e) => return serde_json::json!({ "error": e.to_string() }).into(),
        };
        state.restore_prompt_pending.store(false, Ordering::Relaxed);
        state
            .restorable_tasks_available
            .store(false, Ordering::Relaxed);
        notify_queue_changed(&state);
        serde_json::json!({ "status": "ok", "count": count }).into()
    } else {
        state.restore_prompt_pending.store(false, Ordering::Relaxed);
        match state.queue.defer_restorable_tasks() {
            Ok(_) => {
                state
                    .restorable_tasks_available
                    .store(state.queue.has_restorable_tasks(), Ordering::Relaxed);
                notify_queue_changed(&state);
                serde_json::json!({ "status": "ok" }).into()
            }
            Err(e) => serde_json::json!({ "error": e.to_string() }).into(),
        }
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
    let legacy_args: Vec<Value> = option
        .into_iter()
        .map(|value| Value::String(value.to_string()))
        .collect();
    let meta = build_update_general_lastup_meta(is_update_modified);
    match push_update_job_with_legacy_if_needed(
        state.queue.as_ref(),
        &state.running_jobs,
        target,
        "update_general_lastup",
        legacy_args,
        meta,
    ) {
        Ok((job_ids, queued)) => {
            if queued {
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
    let shutdown_state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        prepare_process_shutdown(&shutdown_state).await;
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
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            return Json(ApiResponse {
                success: false,
                message: e.to_string(),
            });
        }
    };
    let current_dir = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            return Json(ApiResponse {
                success: false,
                message: e.to_string(),
            });
        }
    };
    let hide_console = crate::compat::inherited_hide_console_requested();
    let args = reboot_args_with_no_browser(std::env::args().skip(1).collect(), hide_console);
    let (tx, rx) = tokio::sync::oneshot::channel();
    let reboot_state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        prepare_process_shutdown(&reboot_state).await;
        let mut command = std::process::Command::new(exe);
        command
            .args(&args)
            .current_dir(current_dir)
            .stdin(std::process::Stdio::null());
        crate::compat::configure_hidden_console_command(&mut command);
        crate::compat::configure_process_group_command(&mut command);
        let result = command.spawn().map(|_| ()).map_err(|e| e.to_string());
        let should_exit = result.is_ok();
        let _ = tx.send(result);
        if should_exit {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            std::process::exit(0);
        }
    });
    match rx.await {
        Ok(Ok(())) => {
            state.push_server.broadcast_event("reboot", "");
            Json(ApiResponse {
                success: true,
                message: "Rebooting".to_string(),
            })
        }
        Ok(Err(message)) => Json(ApiResponse {
            success: false,
            message,
        }),
        Err(_) => Json(ApiResponse {
            success: false,
            message: "failed to schedule reboot".to_string(),
        }),
    }
}

fn reboot_args_with_no_browser(mut args: Vec<String>, hide_console: bool) -> Vec<String> {
    if !args.iter().any(|arg| arg == "-n" || arg == "--no-browser") {
        args.push("--no-browser".to_string());
    }
    if hide_console && !args.iter().any(|arg| arg == "--hide-console") {
        args.push("--hide-console".to_string());
    }
    args
}

/// Ruby parity: build sort display string like "タイトル昇順" or "ID順"
fn current_sort_display_string() -> String {
    let sort_state = (|| {
        let inv = crate::db::inventory::Inventory::with_default_root().ok()?;
        let server_setting: serde_yaml::Value = inv
            .load(
                "server_setting",
                crate::db::inventory::InventoryScope::Global,
            )
            .ok()?;
        current_sort_from_server_setting(&server_setting)
    })();

    match sort_state {
        Some(sort_state) => {
            let label = sort_column_label(&sort_state).unwrap_or("不明");
            let dir_label = if sort_state.dir == "desc" {
                "降順"
            } else {
                "昇順"
            };
            format!("{}{}", label, dir_label)
        }
        None => "ID順".to_string(),
    }
}

fn requested_sort_display_string(
    sort_state: Option<&serde_json::Value>,
    timestamp: Option<u64>,
) -> String {
    match request_sort_state(sort_state, timestamp) {
        Some(sort_state) => {
            let label = sort_column_label(&sort_state).unwrap_or("不明");
            let dir_label = if sort_state.dir == "desc" {
                "降順"
            } else {
                "昇順"
            };
            format!("{}{}", label, dir_label)
        }
        None => current_sort_display_string(),
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;
    use std::sync::Arc;

    use chrono::{TimeZone, Utc};
    use serde_yaml::{Mapping, Value};
    use crate::db::NovelRecord;
    use crate::queue::{JobType, PersistentQueue, QueueExecutionSpec, QueueJob};
    use crate::web::sort_state::CurrentSortState;

    use super::{
        broadcast_captured_web_output, build_update_by_tag_queue_payload,
        build_update_general_lastup_meta, build_webui_update_start_message,
        encode_convert_job_target, existing_update_job_id, format_general_lastup_queue_target,
        format_queue_job_type, format_update_queue_target, normalize_update_targets,
        push_update_job_if_needed, push_update_job_with_legacy_if_needed, queue_lane_sizes,
        reboot_args_with_no_browser, restorable_tasks_available, sort_records_for_web_update,
        tag_color_class, validate_diff_number, validate_download_targets,
        validate_general_lastup_option, web_update_sort_key_for_cli,
    };

    fn sample_record(id: i64, general_lastup_ts: i64) -> NovelRecord {
        NovelRecord {
            id,
            author: format!("author-{id}"),
            title: format!("title-{id}"),
            file_title: format!("file-{id}"),
            toc_url: format!("https://example.com/{id}/"),
            sitename: "site".to_string(),
            novel_type: 1,
            end: false,
            last_update: Utc.timestamp_opt(1_700_000_000 + id, 0).unwrap(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: Some(Utc.timestamp_opt(general_lastup_ts, 0).unwrap()),
            last_mail_date: None,
            tags: Vec::new(),
            ncode: None,
            domain: None,
            general_all_no: Some(id),
            length: Some(id),
            suspend: false,
            is_narou: true,
            last_check_date: None,
            convert_failure: false,
            extra_fields: Default::default(),
        }
    }

    #[test]
    fn encode_convert_job_target_omits_default_override() {
        assert_eq!(encode_convert_job_target(&["12".to_string()]), "12");
    }

    #[test]
    fn encode_convert_job_target_batches_targets() {
        assert_eq!(
            encode_convert_job_target(&["12".to_string(), "9".to_string()]),
            "12\t9"
        );
    }

    #[test]
    fn push_update_job_if_needed_reuses_pending_update_job() {
        let temp = tempfile::tempdir().unwrap();
        let queue = PersistentQueue::new(&temp.path().join("queue.yaml")).unwrap();
        let running_jobs = Mutex::new(Vec::new());
        let first =
            push_update_job_if_needed(&queue, &running_jobs, "1\t2\t3".to_string()).unwrap();

        let second =
            push_update_job_if_needed(&queue, &running_jobs, "1\t2\t3".to_string()).unwrap();

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

        let existing = existing_update_job_id(&queue, &running_jobs, "tag:modified", "update", false);

        assert_eq!(existing.as_deref(), Some("running-job"));
    }

    #[test]
    fn update_general_lastup_dedupe_distinguishes_modified_followup() {
        let temp = tempfile::tempdir().unwrap();
        let queue = PersistentQueue::new(&temp.path().join("queue.yaml")).unwrap();
        let running_jobs = Mutex::new(Vec::new());

        let first = push_update_job_with_legacy_if_needed(
            &queue,
            &running_jobs,
            "--gl".to_string(),
            "update_general_lastup",
            Vec::new(),
            Mapping::new(),
        )
        .unwrap();
        let modified = push_update_job_with_legacy_if_needed(
            &queue,
            &running_jobs,
            "--gl".to_string(),
            "update_general_lastup",
            Vec::new(),
            build_update_general_lastup_meta(true),
        )
        .unwrap();
        let modified_duplicate = push_update_job_with_legacy_if_needed(
            &queue,
            &running_jobs,
            "--gl".to_string(),
            "update_general_lastup",
            Vec::new(),
            build_update_general_lastup_meta(true),
        )
        .unwrap();

        assert!(first.1);
        assert!(modified.1);
        assert_ne!(first.0, modified.0);
        assert!(!modified_duplicate.1);
        assert_eq!(modified.0, modified_duplicate.0);
        assert_eq!(queue.pending_count(), 2);
    }

    #[test]
    fn queue_lane_sizes_split_default_and_secondary_jobs() {
        let temp = tempfile::tempdir().unwrap();
        let queue = PersistentQueue::new(&temp.path().join("queue.yaml")).unwrap();

        queue.push(JobType::Download, "1").unwrap();
        queue.push(JobType::Convert, "2").unwrap();

        assert_eq!(queue_lane_sizes(&queue), [1, 1]);
    }

    #[test]
    fn sort_records_for_web_update_matches_general_lastup_descending() {
        let first = sample_record(1, 1_700_000_100);
        let second = sample_record(2, 1_700_000_300);
        let third = sample_record(3, 1_700_000_200);
        let sort_state = CurrentSortState {
            column: 2,
            dir: "desc".to_string(),
        };
        let mut records = vec![&first, &second, &third];

        sort_records_for_web_update(&mut records, &sort_state);

        assert_eq!(
            records.into_iter().map(|record| record.id).collect::<Vec<_>>(),
            vec![2, 3, 1]
        );
    }

    #[test]
    fn update_start_message_uses_all_novels_label_for_update_all() {
        assert_eq!(
            build_webui_update_start_message(true, 1, "最新話掲載日降順"),
            "全ての小説の更新を開始します（1件を最新話掲載日降順で処理）"
        );
        assert_eq!(
            build_webui_update_start_message(false, 1, "最新話掲載日降順"),
            "更新を開始します（1件を最新話掲載日降順で処理）"
        );
    }

    #[test]
    fn build_update_general_lastup_meta_sets_followup_flag_only_when_enabled() {
        let disabled = build_update_general_lastup_meta(false);
        assert!(disabled.is_empty());

        let enabled = build_update_general_lastup_meta(true);
        assert_eq!(
            enabled.get(Value::String("update_modified".to_string())),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn format_queue_job_type_prefers_legacy_command_name() {
        let job = QueueJob {
            id: "task-1".to_string(),
            job_type: JobType::Update,
            target: "narou".to_string(),
            created_at: 0,
            retry_count: 0,
            max_retries: 0,
        };
        let spec = QueueExecutionSpec {
            cmd: "update_general_lastup".to_string(),
            args: vec!["narou".to_string()],
            meta: Mapping::new(),
        };

        assert_eq!(
            format_queue_job_type(&job, Some(&spec)),
            "update_general_lastup"
        );
        assert_eq!(format_queue_job_type(&job, None), "update");
    }

    #[test]
    fn web_update_sort_key_for_cli_accepts_only_update_supported_columns() {
        assert_eq!(
            web_update_sort_key_for_cli(&CurrentSortState {
                column: 2,
                dir: "desc".to_string(),
            }),
            Some("general_lastup")
        );
        assert_eq!(
            web_update_sort_key_for_cli(&CurrentSortState {
                column: 3,
                dir: "desc".to_string(),
            }),
            Some("last_check_date")
        );
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
        assert_eq!(
            validate_general_lastup_option("narou").unwrap(),
            Some("narou")
        );
        assert_eq!(
            validate_general_lastup_option("other").unwrap(),
            Some("other")
        );
        assert!(validate_general_lastup_option("--force").is_err());
    }

    #[test]
    fn general_lastup_queue_target_uses_human_readable_labels() {
        assert_eq!(
            format_general_lastup_queue_target(&["narou".to_string()]),
            "なろうAPIで最新話掲載日を確認"
        );
        assert_eq!(
            format_general_lastup_queue_target(&["other".to_string()]),
            "その他サイトの最新話掲載日を確認"
        );
        assert_eq!(
            format_general_lastup_queue_target(&[]),
            "最新話掲載日を確認"
        );
    }

    #[test]
    fn update_queue_target_prefers_webui_start_message() {
        assert_eq!(
            format_update_queue_target(&[
                "__webui_update_start__=全ての小説の更新を開始します（3件をID順で処理）"
                    .to_string(),
                "1".to_string(),
                "2".to_string(),
                "3".to_string(),
            ]),
            "全ての小説の更新を開始します（3件をID順で処理）"
        );
    }

    #[test]
    fn update_queue_target_describes_tag_and_force_jobs() {
        assert_eq!(
            format_update_queue_target(&["tag:modified".to_string()]),
            "タグ「modified」の小説を更新"
        );
        assert_eq!(
            format_update_queue_target(&[
                "--sort-by".to_string(),
                "general_lastup".to_string(),
                "tag:modified".to_string(),
            ]),
            "タグ「modified」の小説を更新"
        );
        assert_eq!(
            format_update_queue_target(&["--force".to_string(), "tag:modified".to_string()]),
            "タグ「modified」の小説を凍結済みも含めて更新"
        );
    }

    #[test]
    fn update_by_tag_queue_payload_keeps_runtime_tag_args_with_snapshot_meta() {
        let (target, legacy_args, meta) = build_update_by_tag_queue_payload(
            &["tag:modified".to_string(), "^tag:end".to_string()],
            &[42, 9],
        );

        assert_eq!(target, "tag:modified\t^tag:end");
        assert_eq!(
            legacy_args,
            vec![
                Value::String("tag:modified".to_string()),
                Value::String("^tag:end".to_string())
            ]
        );
        assert_eq!(
            meta.get(Value::String("snapshot_ids".to_string())),
            Some(&Value::Sequence(vec![
                Value::String("42".to_string()),
                Value::String("9".to_string())
            ]))
        );
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
            reboot_args_with_no_browser(vec!["web".to_string()], false),
            vec!["web".to_string(), "--no-browser".to_string()]
        );
        assert_eq!(
            reboot_args_with_no_browser(
                vec!["web".to_string(), "--port".to_string(), "33000".to_string()],
                false
            ),
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
            reboot_args_with_no_browser(vec!["web".to_string(), "--no-browser".to_string()], false),
            vec!["web".to_string(), "--no-browser".to_string()]
        );
        assert_eq!(
            reboot_args_with_no_browser(vec!["web".to_string(), "-n".to_string()], false),
            vec!["web".to_string(), "-n".to_string()]
        );
    }

    #[test]
    fn reboot_args_adds_hide_console_for_hidden_web_restart() {
        assert_eq!(
            reboot_args_with_no_browser(vec!["web".to_string()], true),
            vec![
                "web".to_string(),
                "--no-browser".to_string(),
                "--hide-console".to_string()
            ]
        );
    }

    #[test]
    fn reboot_args_preserves_existing_hide_console() {
        assert_eq!(
            reboot_args_with_no_browser(
                vec![
                    "web".to_string(),
                    "--hide-console".to_string(),
                    "--no-browser".to_string()
                ],
                true
            ),
            vec![
                "web".to_string(),
                "--hide-console".to_string(),
                "--no-browser".to_string()
            ]
        );
    }

    #[test]
    fn modified_followup_target_matches_ruby_tag_selector() {
        let target = "tag:modified".to_string();
        assert_eq!(target, "tag:modified");
    }

    #[test]
    fn broadcast_captured_web_output_wraps_plain_text_as_echo() {
        let push_server = Arc::new(crate::web::push::PushServer::new());
        let mut receiver = push_server.channel().subscribe();

        broadcast_captured_web_output(&push_server, b"hello\n", "stdout2");

        let message = receiver.try_recv().unwrap();
        let value: serde_json::Value = serde_json::from_str(&message).unwrap();
        assert_eq!(value["type"], "echo");
        assert_eq!(value["body"], "hello");
        assert_eq!(value["target_console"], "stdout2");
    }

    #[test]
    fn broadcast_captured_web_output_retargets_structured_messages() {
        let push_server = Arc::new(crate::web::push::PushServer::new());
        let mut receiver = push_server.channel().subscribe();
        let line = format!(
            "{}{}",
            crate::progress::WS_LINE_PREFIX,
            serde_json::json!({
                "type": "progressbar.step",
                "data": { "topic": "download", "current": 1, "total": 2, "percent": 50.0 }
            })
        );

        broadcast_captured_web_output(&push_server, line.as_bytes(), "stdout2");

        let message = receiver.try_recv().unwrap();
        let value: serde_json::Value = serde_json::from_str(&message).unwrap();
        assert_eq!(value["type"], "progressbar.step");
        assert_eq!(value["target_console"], "stdout2");
        assert_eq!(value["data"]["topic"], "download");
    }

    #[test]
    fn normalize_update_targets_converts_tag_flag() {
        let normalized =
            normalize_update_targets(&["--tag".to_string(), "modified".to_string()]).unwrap();
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
    fn restorable_tasks_follow_explicit_availability_flag() {
        let temp = tempfile::tempdir().unwrap();
        let queue_path = temp.path().join("queue.yaml");
        std::fs::write(
            &queue_path,
            "---\npending:\n  - id: task-1\n    cmd: download\n    args:\n      - n1234aa\n    meta: {}\n    status: pending\n    created_at: '2026-04-19T15:13:58+09:00'\nrunning: []\nupdated_at: '2026-04-19T15:16:58+09:00'\n",
        )
        .unwrap();
        let queue = Arc::new(PersistentQueue::new(&queue_path).unwrap());
        let state = crate::web::AppState {
            port: 0,
            ws_port: 0,
            push_server: Arc::new(crate::web::push::PushServer::new()),
            basic_auth_header: None,
            control_token: "control-token".to_string(),
            allowed_request_hosts: vec!["localhost".to_string()],
            reverse_proxy_mode: false,
            queue,
            restore_prompt_pending: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            restorable_tasks_available: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            running_jobs: Arc::new(Mutex::new(Vec::new())),
            running_child_pids: Arc::new(Mutex::new(std::collections::HashMap::new())),
            cancelled_job_ids: Arc::new(Mutex::new(std::collections::HashSet::new())),
            auto_update_scheduler: Arc::new(Mutex::new(None)),
        };

        assert!(restorable_tasks_available(&state));
        state
            .restorable_tasks_available
            .store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(!restorable_tasks_available(&state));
    }
}

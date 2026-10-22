use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use axum::{
    extract::{Query, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Json, Response},
};
use serde::Deserialize;
use serde_yaml::{Mapping, Value};

use crate::application::webui::{diff, queue_view, tag_views};
use crate::db::inventory::{Inventory, InventoryScope};
use crate::db::with_database_mut;
use crate::downloader::types::SECTION_SAVE_DIR;
use crate::queue::{
    JobType, PersistentQueue, QueueExecutionSpec, QueueJob, QueueLane,
    WEBUI_MESSAGE_TEXT_META_KEY, WEBUI_MESSAGE_TYPE_META_KEY, WEBUI_UPDATE_START_MESSAGE_TYPE,
};

use super::AppState;
use super::sort_state::{
    CurrentSortState, current_sort_from_server_setting, load_current_sort_state, request_sort_state,
    sort_column_key, sort_display_label, sort_ids_from_records, sort_records,
};
use super::state::{
    ApiResponse, ConfirmRunningTasksBody, ConvertBody, CsvImportBody, DiffBody, DiffCleanBody,
    DownloadBody, ReorderBody, TagInfoBody, TargetsBody, TaskIdBody, UpdateBody, UpdateByTagBody,
};

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

use super::targets_to_strings;

fn validate_download_targets(targets: &[String], max_targets: usize) -> Result<(), String> {
    if targets.len() > max_targets {
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

fn update_start_message(meta: &Mapping) -> Option<String> {
    match (
        meta.get(Value::String(WEBUI_MESSAGE_TYPE_META_KEY.to_string())),
        meta.get(Value::String(WEBUI_MESSAGE_TEXT_META_KEY.to_string())),
    ) {
        (Some(Value::String(message_type)), Some(Value::String(message)))
            if message_type == WEBUI_UPDATE_START_MESSAGE_TYPE && !message.is_empty() =>
        {
            Some(message.clone())
        }
        _ => None,
    }
}

fn log_web_failure(context: &str, error: impl std::fmt::Display) {
    eprintln!("web {} failed: {}", context, error);
}

fn log_immediate_command_failure(
    context: &str,
    output: &crate::application::WebActionOutput,
) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.trim().is_empty() {
        eprintln!("web {} failed", context);
    } else {
        eprintln!("web {} failed: {}", context, stderr.trim());
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

/// native の meta にある開始メッセージを共有実装へ渡すアダプタ。
fn format_update_queue_target(args: &[String], meta: Option<&Mapping>) -> String {
    queue_view::format_update_queue_target(args, meta.and_then(update_start_message).as_deref(), None)
}

fn format_queue_job_target(job: &QueueJob, spec: Option<&QueueExecutionSpec>) -> String {
    let Some(spec) = spec else {
        return queue_view::queue_target_fallback_text(&job.target);
    };

    match spec.cmd.as_str() {
        "update_general_lastup" => queue_view::general_lastup_queue_target(&spec.args),
        "update" | "update_by_tag" => format_update_queue_target(&spec.args, Some(&spec.meta)),
        _ => queue_view::queue_target_fallback_text(&job.target),
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
    queue_view::queue_type_label(spec.map(|spec| spec.cmd.as_str()), job_type_key(job.job_type))
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

fn sort_records_for_web_update(records: &mut [crate::db::NovelRecord], sort_state: &CurrentSortState) {
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

fn sorted_update_all_ids(mut records: Vec<crate::db::NovelRecord>) -> Vec<String> {
    let sort_state = current_web_update_sort_state();
    sort_records_for_web_update(&mut records, &sort_state);
    records.into_iter().map(|record| record.id.to_string()).collect()
}


async fn sort_numeric_targets_for_state(
    state: &AppState,
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
    let records = state.services.library.records().await.unwrap_or_default();
    sort_ids_from_records(&ids, &records, sort_state, timestamp)
        .into_iter()
        .map(|id| id.to_string())
        .collect()
}
fn build_update_by_tag_queue_payload(
    tag_params: &[String],
    snapshot_ids: &[i64],

    sort_display: &str,
) -> (String, Vec<Value>, Mapping) {
    let target_args = snapshot_ids
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>();
    let target = target_args.join("\t");
    let legacy_args = target_args
        .iter()
        .cloned()
        .map(Value::String)
        .collect::<Vec<_>>();
    let mut meta = build_update_start_message_meta(build_webui_update_start_message(
        false,
        snapshot_ids.len(),
        sort_display,
    ));
    meta.insert(
        Value::String("tag_params".to_string()),
        Value::Sequence(
            tag_params
                .iter()
                .cloned()
                .map(Value::String)
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
    // 文言は Worker (`worker_entry` の update 投入経路) と同じ共有実装。
    crate::application::messages::jobs::update_started(is_update_all, count, sort_display)
}

fn build_update_start_message_meta(message: String) -> Mapping {
    let mut meta = Mapping::new();
    meta.insert(
        Value::String(WEBUI_MESSAGE_TYPE_META_KEY.to_string()),
        Value::String(WEBUI_UPDATE_START_MESSAGE_TYPE.to_string()),
    );
    meta.insert(
        Value::String(WEBUI_MESSAGE_TEXT_META_KEY.to_string()),
        Value::String(message),
    );
    meta
}

fn queue_download_jobs(
    queue: &PersistentQueue,
    targets: &[String],
    force: bool,
    mail: bool,
    max_targets: usize,
) -> Result<Vec<String>, String> {
    validate_download_targets(targets, max_targets)?;
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
async fn resolve_existing_id_for_target_with_library(
    state: &AppState,
    target: &str,
) -> Option<i64> {
    state
        .services
        .library
        .resolve_target(
            crate::application::target_lookup::TargetLookupPlan::READ,
            &std::collections::HashMap::new(),
            crate::application::target_lookup::UrlSource::Provider(
                &*state.services.site_definitions,
            ),
            target,
        )
        .await
        .ok()
        .flatten()
        .map(|record| record.id)
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

/// 中断した子プロセスでは `NovelLockGuard::drop` が走らないため、子が書いた
/// `.narou/lock.yaml` のエントリだけが残る。`worker::pop_next_job` は新鮮な
/// ロックを持つ小説のジョブを延期するので、中断したジョブの対象小説だけを
/// ここで解放する。子が先に正常終了して既に消している場合もあるため冪等。
fn release_novel_locks(novel_ids: &HashSet<i64>) {
    if novel_ids.is_empty() {
        return;
    }
    let released = Inventory::with_default_root().and_then(|inventory| {
        inventory.update_yaml::<(), HashMap<i64, serde_yaml::Value>, _>(
            "lock",
            InventoryScope::Local,
            |mut locked| {
                for id in novel_ids {
                    locked.remove(id);
                }
                Ok((locked, ()))
            },
        )
    });
    if let Err(error) = released {
        log_web_failure("novel lock release", error);
    }
}

/// `running_jobs` のうち `job_ids` のジョブが対象にしている小説 ID を集める。
fn cancelled_novel_ids(
    running_jobs: &parking_lot::Mutex<Vec<QueueJob>>,
    job_ids: &[String],
) -> HashSet<i64> {
    let running = running_jobs.lock();
    running
        .iter()
        .filter(|job| job_ids.contains(&job.id))
        .flat_map(|job| crate::application::extract_novel_ids(&job.target))
        .collect()
}

fn release_cancelled_novel_locks(state: &AppState, job_ids: &[String]) {
    release_novel_locks(&cancelled_novel_ids(&state.running_jobs, job_ids));
}

fn kill_running_child(state: &AppState) {
    let jobs: Vec<(String, u32)> = {
        let mut guard = state.running_child_pids.lock();
        let values = guard.iter().map(|(job_id, pid)| (job_id.clone(), *pid)).collect();
        guard.clear();
        values
    };
    let cancelled_ids: Vec<String> = jobs.iter().map(|(job_id, _)| job_id.clone()).collect();
    for (job_id, pid) in jobs {
        state.cancelled_job_ids.lock().insert(job_id);
        kill_process_tree(pid, &state.push_server);
    }
    // kill は子の後始末 (`NovelLockGuard::drop`) を走らせない。残ったロックを
    // 解放しないと同じ小説のジョブが 30 分間延期され続ける。
    release_cancelled_novel_locks(state, &cancelled_ids);
}

fn kill_running_child_for_job(state: &AppState, job_id: &str) -> bool {
    let Some(lane) = state
        .running_jobs
        .lock()
        .iter()
        .find(|job| job.id == job_id)
        .map(|job| job.job_type.lane())
    else {
        return false;
    };
    let lane_job_ids = state
        .running_jobs
        .lock()
        .iter()
        .filter(|running| running.job_type.lane() == lane)
        .map(|running| running.id.clone())
        .collect::<Vec<_>>();
    let running_pids = {
        let mut guard = state.running_child_pids.lock();
        let lane_job_ids_set = lane_job_ids.iter().cloned().collect::<std::collections::HashSet<_>>();
        let running = guard
            .iter()
            .filter(|(running_job_id, _)| lane_job_ids_set.contains(*running_job_id))
            .map(|(running_job_id, pid)| (running_job_id.clone(), *pid))
            .collect::<Vec<_>>();
        for (running_job_id, _) in &running {
            guard.remove(running_job_id);
        }
        running
    };
    let cancelled_pending = state
        .queue
        .cancel_pending_in_lane(lane)
        .map(|ids| !ids.is_empty())
        .unwrap_or(false);
    if running_pids.is_empty() {
        return cancelled_pending;
    }
    let killed_job_ids: Vec<String> = running_pids
        .iter()
        .map(|(job_id, _)| job_id.clone())
        .collect();
    let mut cancelled_job_ids = state.cancelled_job_ids.lock();
    for (running_job_id, pid) in running_pids {
        cancelled_job_ids.insert(running_job_id);
        kill_process_tree(pid, &state.push_server);
    }
    drop(cancelled_job_ids);
    // レーン内で kill した子のロックを解放する (子の後始末は走らない)。
    release_cancelled_novel_locks(state, &killed_job_ids);
    true
}

fn kill_process_tree(pid: u32, push_server: &std::sync::Arc<super::push::PushServer>) {
    let result = crate::compat::terminate_process(pid);
    if let Err(e) = result {
        push_server.broadcast_echo(&format!("プロセス終了に失敗: {}", e), "stdout");
    }
}

fn cache_version_dirs(archive_root: &std::path::Path, base_dir: &std::path::Path) -> Result<Vec<PathBuf>, String> {
    let mut dirs = Vec::new();
    for name in diff::DIFF_CACHE_DIR_NAMES {
        let root = base_dir.join(SECTION_SAVE_DIR).join(name);
        super::reject_symlink_ancestors(&root, archive_root)?;
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        for entry in entries {
            let path = entry.map_err(|error| error.to_string())?.path();
            super::reject_symlink_ancestors(&path, archive_root)?;
            if path.is_dir() {
                dirs.push(path);
            }
        }
    }
    Ok(dirs)
}

fn native_cache_version_hints(
    archive_root: &std::path::Path,
    base_dir: &std::path::Path,
) -> Result<Vec<diff::CacheVersionHint>, String> {
    let mut hints = Vec::new();
    for dir in cache_version_dirs(archive_root, base_dir)? {
        let Some(version) = dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let enumeration_succeeded = std::fs::read_dir(&dir)
            .and_then(|entries| {
                for entry in entries {
                    entry?;
                }
                Ok(())
            })
            .is_ok();
        hints.push(diff::CacheVersionHint {
            version: version.to_string(),
            enumeration_succeeded,
        });
    }
    Ok(hints)
}

async fn render_diff_list_html_for_target(state: &AppState, target: &str) -> String {
    let Some(id) = resolve_existing_id_for_target_with_library(state, target).await else {
        return String::new();
    };
    let Ok(Some(record)) = state.services.library.get(id.into()).await else {
        return String::new();
    };
    let Ok(archive_root) =
        crate::db::with_database(|db| Ok(db.archive_root().to_path_buf()))
    else {
        return String::new();
    };

    let Ok(base_dir) = super::safe_existing_novel_dir(&archive_root, &record) else {
        return String::new();
    };
    let Some(prefix) = crate::native::object_store::novel_object_prefix(&archive_root, &base_dir)
    else {
        return String::new();
    };
    let Ok(hints) = native_cache_version_hints(&archive_root, &base_dir) else {
        return String::new();
    };
    let Ok(versions) =
        diff::read_diff_cache_catalog(state.services.content.object_store(), &prefix, &hints).await
    else {
        return String::new();
    };
    diff::render_diff_list_html(&versions)
}

#[cfg(test)]
mod cache_catalog_tests {
    use super::*;
    use crate::native::object_store::NativeObjectStore;

    #[test]
    fn legacy_empty_cache_directory_is_read_without_creating_a_marker() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path();
        let base = archive.join("site/title");
        let version_dir = base.join("本文/.cache/2026.10.13@01.02.03");
        std::fs::create_dir_all(&version_dir).unwrap();
        let objects = NativeObjectStore::from_root(archive.to_path_buf()).unwrap();
        let prefix = crate::native::object_store::novel_object_prefix(archive, &base).unwrap();
        let hints = native_cache_version_hints(archive, &base).unwrap();
        let versions = futures::executor::block_on(
            diff::read_diff_cache_catalog(&objects, &prefix, &hints),
        ).unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].version, "2026.10.13@01.02.03");
        assert!(versions[0].only_latest_update);
        assert!(std::fs::read_dir(&version_dir).unwrap().next().is_none(),
            "GET-time catalog reads must not create .version or YAML objects");
    }
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
    let max_targets = super::max_web_targets_per_request(&state).await;
    let targets = body.targets;
    let ids = match queue_download_jobs(
        state.queue.as_ref(),
        &targets,
        body.force,
        body.mail,
        max_targets,
    ) {
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
    let max_targets = super::max_web_targets_per_request(&state).await;
    let targets = match normalize_update_targets(&raw_targets, max_targets) {
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
    let (combined, update_start_meta) = if has_flags {
        count = 1;
        (targets.join("\t"), None)
    } else {
        let mut args = if is_update_all {
            let records = state.services.library.records().await.unwrap_or_default();
            sorted_update_all_ids(records)
        } else {
            sort_numeric_targets_for_state(
                &state,
                &targets,
                body.sort_state.as_ref(),
                body.timestamp,
            )
            .await
        };
        count = args.len();
        let author_only = if count == 0 && is_update_all {
            let disabled = state.services.settings.get_many(&["update.disabled-author-tracking"]).await;
            match disabled {
                Ok(values) if values.first().and_then(Option::as_ref).and_then(crate::setting_core::setting_bool_value).unwrap_or(false) => false,
                Ok(_) => match crate::author::authors_for_current_root() {
                    Ok(authors) => !authors.is_empty(),
                    Err(error) => return Json(serde_json::json!({"success":false,"message":error.to_string(),"count":0})),
                },
                Err(error) => return Json(serde_json::json!({"success":false,"message":error.to_string(),"count":0})),
            }
        } else { false };
        if count == 0 && !author_only {
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
        (
            args.join("\t"),
            Some(build_update_start_message_meta(build_webui_update_start_message(
                is_update_all,
                count,
                &sort_display,
            ))),
        )
    };
    let queued_result = match update_start_meta {
        Some(meta) => {
            let legacy_args = combined
                .split('\t')
                .filter(|arg| !arg.is_empty())
                .map(|arg| Value::String(arg.to_string()))
                .collect::<Vec<_>>();
            push_update_job_with_legacy_if_needed(
                state.queue.as_ref(),
                &state.running_jobs,
                combined,
                "update",
                legacy_args,
                meta,
            )
        }
        None => push_update_job_if_needed(state.queue.as_ref(), &state.running_jobs, combined),
    };
    let (job_ids, queued) = match queued_result {
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
fn normalize_update_targets(targets: &[String], max_targets: usize) -> Result<Vec<String>, String> {
    if targets.len() > max_targets {
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
    if body.targets.len() > super::max_web_targets_per_request(&state).await {
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
    let device = match super::normalize_web_device_override(body.device.as_deref()) {
        Ok(device) => device,
        Err(message) => {
            return serde_json::json!({
                "success": false,
                "message": message,
                "results": []
            })
            .into();
        }
    };
    let ordered_targets =
        sort_numeric_targets_for_state(&state, &targets, body.sort_state.as_ref(), body.timestamp)
            .await;
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
    let running_label = queue_view::queue_running_label(
        running_count,
        running_jobs
            .first()
            .filter(|_| running_count == 1)
            .map(|job| queue_display_target(state.queue.as_ref(), job)),
    );
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
            state
                .restore_prompt_pending
                .store(state.queue.restore_prompt_pending(), Ordering::Relaxed);
            state
                .restorable_tasks_available
                .store(state.queue.has_restorable_tasks(), Ordering::Relaxed);
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

fn encode_convert_job_target(targets: &[String]) -> String {
    targets.join("\t")
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
            push_server.broadcast_raw(value);
        }
    }
}

fn notify_queue_changed(state: &AppState) {
    state.push_server.broadcast_event("notification.queue", "");
}

pub(super) async fn prepare_process_shutdown(state: &AppState) {
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
    if raw_targets.len() > super::max_web_targets_per_request(&state).await {
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
        sort_numeric_targets_for_state(&state, &targets, body.sort_state.as_ref(), body.timestamp)
            .await;
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
    if raw_targets.len() > super::max_web_targets_per_request(&state).await {
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
    match state.services.web_actions.inspect(&targets).await {
        Ok(output) => {
            broadcast_captured_web_output(
                &state.push_server,
                &output.stdout,
                crate::native::non_external_console_target(),
            );
            broadcast_captured_web_output(
                &state.push_server,
                &output.stderr,
                crate::native::non_external_console_target(),
            );
            Json(ApiResponse {
                success: output.success,
                message: if output.success {
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
    if raw_targets.len() > super::max_web_targets_per_request(&state).await {
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
    match state.services.web_actions.folder(&targets).await {
        Ok(output) => {
            broadcast_captured_web_output(
                &state.push_server,
                &output.stdout,
                crate::native::non_external_console_target(),
            );
            broadcast_captured_web_output(
                &state.push_server,
                &output.stderr,
                crate::native::non_external_console_target(),
            );
            Json(ApiResponse {
                success: output.success,
                message: if output.success {
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
    if raw_targets.len() > super::max_web_targets_per_request(&state).await {
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
        sort_numeric_targets_for_state(&state, &targets, body.sort_state.as_ref(), body.timestamp)
            .await;
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
    if raw_targets.len() > super::max_web_targets_per_request(&state).await {
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
    if raw_targets.len() > super::max_web_targets_per_request(&state).await {
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
    match state.services.web_actions.setting_burn(&targets).await {
        Ok(output) => {
            broadcast_captured_web_output(
                &state.push_server,
                &output.stdout,
                crate::native::non_external_console_target(),
            );
            broadcast_captured_web_output(
                &state.push_server,
                &output.stderr,
                crate::native::non_external_console_target(),
            );
            Json(ApiResponse {
                success: output.success,
                message: if output.success {
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
    State(state): State<AppState>,
    Json(body): Json<TargetsBody>,
) -> Json<serde_json::Value> {
    let targets = targets_to_strings(&body.targets);
    if targets.len() > super::max_web_targets_per_request(&state).await {
        return serde_json::json!({ "error": "too many targets" }).into();
    }
    let mut diffs = Vec::new();
    for target in &targets {
        let id: i64 = match target.parse() {
            Ok(id) => id,
            Err(_) => continue,
        };
        let record = state.services.library.get(id.into()).await.ok().flatten();
        let Some(record) = record else {
            diffs.push(diff::diff_missing_entry(id));
            continue;
        };
        let content = diff::diff_content_label(state.services.content.diff(id.into()).await);
        diffs.push(diff::diff_list_entry(id, &record.title, &content));
    }
    serde_json::json!({ "diffs": diffs }).into()
}

// GET /api/diff_list
pub async fn api_diff_list_get(
    State(state): State<AppState>,
    Query(params): Query<DiffListQuery>,
) -> Html<String> {
    let html = match params.target.as_deref() {
        Some(target) => render_diff_list_html_for_target(&state, target).await,
        None => String::new(),
    };
    Html(html)
}

// GET /api/downloadable.gif
pub async fn api_downloadable_gif(
    State(state): State<AppState>,
    Query(params): Query<BookmarkletDownloadQuery>,
) -> Response {
    let _number = match params.target.as_deref() {
        Some(target) if !target.trim().is_empty() => {
            if resolve_existing_id_for_target_with_library(&state, target)
                .await
                .is_some()
            {
                1
            } else {
                0
            }
        }
        _ => 2,
    };
    gif_response()
}

// POST /api/diff
pub async fn api_diff(
    State(state): State<AppState>,
    Json(body): Json<DiffBody>,
) -> Json<serde_json::Value> {
    let diff_number = match validate_diff_number(&body.number) {
        Ok(number) => number,
        Err(message) => {
            return serde_json::json!({ "success": false, "message": message }).into();
        }
    };
    if body.ids.len() > super::max_web_targets_per_request(&state).await {
        return serde_json::json!({ "success": false, "message": "too many ids" }).into();
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
            return serde_json::json!({ "success": false, "message": message }).into();
        }
    };

    for id in &ids {
        match state.services.web_actions.diff(id, &diff_number).await {
            Ok(output) => {
                broadcast_captured_web_output(
                    &state.push_server,
                    &output.stdout,
                    crate::native::non_external_console_target(),
                );
                broadcast_captured_web_output(
                    &state.push_server,
                    &output.stderr,
                    crate::native::non_external_console_target(),
                );
                if !output.success {
                    log_immediate_command_failure("diff", &output);
                    return serde_json::json!({
                        "success": false,
                        "message": format!("diff の実行に失敗しました ({id})"),
                    }).into();
                }
            }
            Err(e) => {
                log_web_failure("diff", &e);
                state.push_server.broadcast_echo(
                    &format!("diff error: {}", e),
                    crate::native::non_external_console_target(),
                );
                return serde_json::json!({
                    "success": false,
                    "message": format!("diff の実行に失敗しました ({id})"),
                }).into();
            }
        }
    }

    let history: serde_json::Map<String, serde_json::Value> = ids
        .iter()
        .filter_map(|id| id.parse::<i64>().ok())
        .map(|id| (id.to_string(), serde_json::json!(diff_history_versions(id))))
        .collect();
    serde_json::json!({
        "success": true,
        "message": format!("Diff completed for {} novel(s)", ids.len()),
        "history": history,
    })
    .into()
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

    match state.services.web_actions.diff_clean(&target).await {
        Ok(output) => {
            broadcast_captured_web_output(
                &state.push_server,
                &output.stdout,
                crate::native::non_external_console_target(),
            );
            broadcast_captured_web_output(
                &state.push_server,
                &output.stderr,
                crate::native::non_external_console_target(),
            );
            Json(ApiResponse {
                success: output.success,
                message: if output.success {
                    diff::diff_cleaned_message(&target)
                } else {
                    log_immediate_command_failure("diff --clean", &output);
                    diff::DIFF_CLEAN_FAILED.to_string()
                },
            })
        }
        Err(e) => {
            log_web_failure("diff --clean", &e);
            Json(ApiResponse {
                success: false,
                message: diff::DIFF_CLEAN_FAILED.to_string(),
            })
        }
    }
}

/// Shared SQLite connection for diff-history endpoints; `None` when the
/// library still runs on the legacy YAML backend.
fn diff_history_conn(
) -> Option<std::sync::Arc<parking_lot::Mutex<rusqlite::Connection>>> {
    let narou_dir = crate::db::inventory::Inventory::with_default_root()
        .ok()?
        .root_dir()
        .join(".narou");
    crate::native::sqlite::state::active_for(&narou_dir).map(|state| state.conn())
}

fn diff_history_versions(id: i64) -> Vec<serde_json::Value> {
    let Some(conn) = diff_history_conn() else {
        return Vec::new();
    };
    let guard = conn.lock();
    match crate::native::sqlite::versions::list_versions(&guard, id) {
        Ok(versions) => versions
            .into_iter()
            .map(|version| {
                serde_json::json!({
                    "id": version.id,
                    "parent_id": version.parent_id,
                    "origin": version.origin,
                    "note": version.note,
                    "created_at": version.created_at,
                    "section_count": version.section_count,
                })
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[derive(Debug, Deserialize)]
pub struct DiffHistoryQuery {
    pub id: Option<i64>,
}

// GET /api/diff_history?id=N — P4b version history (newest first).
pub async fn api_diff_history(
    State(_state): State<AppState>,
    Query(params): Query<DiffHistoryQuery>,
) -> Json<serde_json::Value> {
    let Some(id) = params.id else {
        return serde_json::json!({ "success": false, "message": "id is required" }).into();
    };
    serde_json::json!({
        "success": true,
        "id": id,
        "versions": diff_history_versions(id),
    })
    .into()
}

#[derive(Debug, Deserialize)]
pub struct DiffShowQuery {
    pub id: Option<i64>,
    pub version: Option<i64>,
}

// GET /api/diff_show?id=N&version=V — stored unified diff for one version.
pub async fn api_diff_show(
    State(_state): State<AppState>,
    Query(params): Query<DiffShowQuery>,
) -> Json<serde_json::Value> {
    let (Some(id), Some(version_id)) = (params.id, params.version) else {
        return serde_json::json!({ "success": false, "message": "id and version are required" })
            .into();
    };
    let Some(conn) = diff_history_conn() else {
        return serde_json::json!({ "success": false, "message": "SQLite backend is not active" })
            .into();
    };
    let guard = conn.lock();
    match crate::native::sqlite::versions::version_diff(&guard, id, version_id) {
        Ok(Some(diff)) => serde_json::json!({
            "success": true,
            "id": id,
            "version": version_id,
            "diff": diff,
        })
        .into(),
        Ok(None) => serde_json::json!({
            "success": false,
            "message": format!("バージョン{version_id}の差分はありません"),
        })
        .into(),
        Err(error) => serde_json::json!({
            "success": false,
            "message": error.to_string(),
        })
        .into(),
    }
}

#[derive(Debug, Deserialize)]
pub struct DiffVersionBody {
    pub target: serde_json::Value,
    pub version: i64,
    #[serde(default)]
    pub sections: Option<String>,
}

fn diff_version_target(body: &DiffVersionBody) -> std::result::Result<String, String> {
    let target = match &body.target {
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    super::validate_web_target_value(&target)
}

async fn run_diff_version_action(
    state: &AppState,
    body: DiffVersionBody,
    action: &str,
) -> Json<ApiResponse> {
    let target = match diff_version_target(&body) {
        Ok(target) => target,
        Err(message) => {
            return Json(ApiResponse {
                success: false,
                message,
            })
        }
    };
    let result = match action {
        "restore" => {
            state
                .services
                .web_actions
                .diff_restore(&target, body.version)
                .await
        }
        _ => {
            state
                .services
                .web_actions
                .diff_merge(&target, body.version, body.sections.as_deref())
                .await
        }
    };
    match result {
        Ok(output) => {
            broadcast_captured_web_output(
                &state.push_server,
                &output.stdout,
                crate::native::non_external_console_target(),
            );
            broadcast_captured_web_output(
                &state.push_server,
                &output.stderr,
                crate::native::non_external_console_target(),
            );
            if !output.success {
                log_immediate_command_failure(action, &output);
            }
            Json(ApiResponse {
                success: output.success,
                message: if output.success {
                    format!("バージョン{}の{action}を実行しました", body.version)
                } else {
                    format!("バージョン{}の{action}に失敗しました", body.version)
                },
            })
        }
        Err(e) => {
            log_web_failure(action, &e);
            Json(ApiResponse {
                success: false,
                message: format!("バージョン{}の{action}に失敗しました", body.version),
            })
        }
    }
}

// POST /api/diff_restore — copy-forward restore of one version.
pub async fn api_diff_restore(
    State(state): State<AppState>,
    Json(body): Json<DiffVersionBody>,
) -> Json<ApiResponse> {
    run_diff_version_action(&state, body, "restore").await
}

// POST /api/diff_merge — merge sections of one version into the working set.
pub async fn api_diff_merge(
    State(state): State<AppState>,
    Json(body): Json<DiffVersionBody>,
) -> Json<ApiResponse> {
    run_diff_version_action(&state, body, "merge").await
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
    match state.services.web_actions.csv_import(&body.csv).await {
        Ok(output) => {
            broadcast_captured_web_output(
                &state.push_server,
                &output.stdout,
                crate::native::non_external_console_target(),
            );
            broadcast_captured_web_output(
                &state.push_server,
                &output.stderr,
                crate::native::non_external_console_target(),
            );
            if output.success {
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
pub async fn api_csv_download(State(state): State<AppState>) -> axum::response::Response {
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;

    let output = match state.services.web_actions.csv_download().await {
        Ok(output) => output,
        Err(e) => {
            log_web_failure("csv download", &e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "CSVエクスポートに失敗しました",
            )
                .into_response();
        }
    };

    if !output.success {
        log_immediate_command_failure("csv download", &output);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "CSVエクスポートに失敗しました",
        )
            .into_response();
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
        output.stdout,
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
    if kill_running_child_for_job(&state, &body.task_id) {
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
            queue_view::queue_job_payload(
                &j.id,
                &queue_display_type(state.queue.as_ref(), j),
                &j.target,
                &queue_display_target(state.queue.as_ref(), j),
                Some(j.created_at),
            )
        })
        .collect();

    let running = state.queue.get_running_tasks();
    let running_count = running.len();
    let running_json: Vec<serde_json::Value> = running
        .iter()
        .map(|job| {
            queue_view::queue_job_payload(
                &job.id,
                &queue_display_type(state.queue.as_ref(), job),
                &job.target,
                &queue_display_target(state.queue.as_ref(), job),
                Some(job.created_at),
            )
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

    // Snapshot current matching novel IDs before queuing, then run through the
    // same update worker path used by normal selected-ID updates.
    let server_sort_state = current_web_update_sort_state();
    let records = state.services.library.records().await.unwrap_or_default();
    let snapshot_ids: Vec<i64> = {
        let mut matching = records
            .into_iter()
            .filter(|record| {
                let include = tags
                    .iter()
                    .any(|tag| record.tags.iter().any(|value| value == tag));
                let exclude = exclusion_tags
                    .iter()
                    .any(|tag| record.tags.iter().any(|value| value == tag));
                (tags.is_empty() || include) && !exclude
            })
            .collect::<Vec<_>>();
        sort_records_for_web_update(&mut matching, &server_sort_state);
        matching.into_iter().map(|record| record.id).collect()
    };

    let count = snapshot_ids.len();
    if snapshot_ids.is_empty() {
        return serde_json::json!({
            "success": true,
            "count": count,
            "status": "no_targets",
        })
        .into();
    }
    let sort_display = current_sort_display_string();
    let (target, legacy_args, meta) =
        build_update_by_tag_queue_payload(&tag_params, &snapshot_ids, &sort_display);
    let (_job_ids, queued) = match push_update_job_with_legacy_if_needed(
        state.queue.as_ref(),
        &state.running_jobs,
        target.clone(),
        "update",
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
    State(state): State<AppState>,
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
    let records = state.services.library.records().await.unwrap_or_default();
    let selected_ids = sort_ids_from_records(
        &selected_ids,
        &records,
        body.sort_state.as_ref(),
        body.timestamp,
    );
    let new_tag_color = super::configured_tag_color(&state).await;
    let tag_colors = state
        .services
        .tag_colors
        .for_tags(tag_views::tag_names(&records), new_tag_color.as_deref())
        .await
        .unwrap_or_default();

    Json(serde_json::json!(tag_views::taginfo_entries(
        &records,
        &selected_ids,
        with_exclusion,
        &tag_colors,
    )))
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
    match schedule_server_reboot(state.clone()).await {
        Ok(()) => {
            state.push_server.broadcast_event("reboot", "");
            Json(ApiResponse {
                success: true,
                message: "Rebooting".to_string(),
            })
        }
        Err(message) => Json(ApiResponse {
            success: false,
            message,
        }),
    }
}

/// Spawns a replacement server process with the same CLI args, then exits this
/// process after the new one is running. Used by `POST /api/reboot` and by
/// `POST /api/storage/mode` (backend switch requires a restart).
pub async fn schedule_server_reboot(state: AppState) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let current_dir = std::env::current_dir().map_err(|e| e.to_string())?;
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
        Ok(result) => result,
        Err(_) => Err("failed to schedule reboot".to_string()),
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

/// Ruby parity: build sort display string like "タイトル昇順" or "ID順".
/// 文言組み立て自体は Worker でも使う共有実装 (`application::webui::
/// sort_display_label`) に寄せる — 両環境の更新開始案内が同じ表示名になる。
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

    sort_display_label(sort_state.as_ref())
}

fn requested_sort_display_string(
    sort_state: Option<&serde_json::Value>,
    timestamp: Option<u64>,
) -> String {
    // `request_sort_state` は常に None (サーバー保存のソートが真実源) なので、
    // 実質 `current_sort_display_string` に委譲する。Some が返る経路ができても
    // native と同じく保存済み表示名へ倒す。
    match request_sort_state(sort_state, timestamp) {
        Some(sort_state) => sort_display_label(Some(&sort_state)),
        None => current_sort_display_string(),
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::time::Duration;

    use chrono::{TimeZone, Utc};
    use serde_yaml::{Mapping, Value};
    use crate::db::NovelRecord;
    use crate::queue::{JobType, PersistentQueue, QueueExecutionSpec, QueueJob};
    use crate::web::sort_state::CurrentSortState;

    use super::{
        build_update_by_tag_queue_payload,
        build_update_general_lastup_meta, build_update_start_message_meta,
        build_webui_update_start_message, cancelled_novel_ids, encode_convert_job_target,
        existing_update_job_id, format_queue_job_type,
        format_update_queue_target, normalize_update_targets, push_update_job_if_needed,
        push_update_job_with_legacy_if_needed, queue_lane_sizes, reboot_args_with_no_browser,
        release_novel_locks, restorable_tasks_available,
        sort_records_for_web_update, validate_diff_number,
        validate_download_targets, validate_general_lastup_option, web_update_sort_key_for_cli,
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
            requires_login: false,
            login_session: None,
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
            available_at: None,
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
        let mut records = vec![first.clone(), second.clone(), third.clone()];

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
            available_at: None,
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
            validate_download_targets(
                &["1".to_string(), "https://example.com".to_string()],
                crate::web::MAX_WEB_TARGETS_PER_REQUEST,
            )
                .is_ok()
        );
        assert!(validate_download_targets(
            &["--remove".to_string()],
            crate::web::MAX_WEB_TARGETS_PER_REQUEST
        )
        .is_err());
        assert!(validate_download_targets(
            &["1\t2".to_string()],
            crate::web::MAX_WEB_TARGETS_PER_REQUEST
        )
        .is_err());
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
    fn update_queue_target_prefers_webui_start_message() {
        let meta = build_update_start_message_meta(
            "全ての小説の更新を開始します（3件をID順で処理）".to_string(),
        );
        assert_eq!(
            format_update_queue_target(
                &["1".to_string(), "2".to_string(), "3".to_string()],
                Some(&meta)
            ),
            "全ての小説の更新を開始します（3件をID順で処理）"
        );
    }

    #[test]
    fn update_by_tag_queue_payload_uses_common_update_args() {
        let (target, legacy_args, meta) = build_update_by_tag_queue_payload(
            &["tag:modified".to_string(), "^tag:end".to_string()],
            &[42, 9],
            "最新話掲載日降順",
        );

        assert_eq!(target, "42\t9");
        assert_eq!(
            legacy_args,
            vec![
                Value::String("42".to_string()),
                Value::String("9".to_string())
            ]
        );
        assert_eq!(
            meta.get(Value::String("tag_params".to_string())),
            Some(&Value::Sequence(vec![
                Value::String("tag:modified".to_string()),
                Value::String("^tag:end".to_string())
            ]))
        );
        assert_eq!(
            meta.get(Value::String("snapshot_count".to_string())),
            Some(&Value::Number(serde_yaml::Number::from(2)))
        );
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
    fn normalize_update_targets_converts_tag_flag() {
        let normalized =
            normalize_update_targets(
                &["--tag".to_string(), "modified".to_string()],
                crate::web::MAX_WEB_TARGETS_PER_REQUEST,
            )
            .unwrap();
        assert_eq!(normalized, vec!["tag:modified"]);
    }

    #[test]
    fn normalize_update_targets_rejects_missing_tag_name() {
        assert!(normalize_update_targets(
            &["--tag".to_string()],
            crate::web::MAX_WEB_TARGETS_PER_REQUEST
        )
        .is_err());
        assert!(normalize_update_targets(
            &["--tag".to_string(), "--gl".to_string()],
            crate::web::MAX_WEB_TARGETS_PER_REQUEST
        )
        .is_err());
    }

    #[test]
    fn normalize_update_targets_rejects_unexpected_flags() {
        assert!(normalize_update_targets(
            &["--remove".to_string()],
            crate::web::MAX_WEB_TARGETS_PER_REQUEST
        )
        .is_err());
        assert!(normalize_update_targets(
            &["bad\ttarget".to_string()],
            crate::web::MAX_WEB_TARGETS_PER_REQUEST
        )
        .is_err());
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
            services: crate::web::default_app_services(Arc::new(
                crate::platform::mocks::MemoryNovelRepository::new(),
            )),
            server_security: crate::web::test_server_security(
                None,
                vec!["localhost".to_string()],
                false,
            ),
            bind_host: "localhost".into(),
            control_token: "control-token".to_string(),
            ui_token: "ui-token".to_string(),
            ui_bootstrap_ticket: "bootstrap-ticket".to_string(),
            ui_bootstrap_consumed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            queue,
            restore_prompt_pending: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            restorable_tasks_available: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            library_backup: Arc::new(crate::web::library_backup::LibraryBackupState::new()),
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

    fn seed_lock_file(dir: &std::path::Path, ids: &[i64]) {
        std::fs::create_dir_all(dir.join(".narou")).unwrap();
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.9f %:z");
        let content = ids
            .iter()
            .map(|id| format!("{id}: {now}\n"))
            .collect::<String>();
        std::fs::write(dir.join(".narou").join("lock.yaml"), content).unwrap();
    }

    fn sample_queue_job(id: &str, job_type: JobType, target: &str) -> QueueJob {
        QueueJob {
            id: id.to_string(),
            job_type,
            target: target.to_string(),
            created_at: 0,
            retry_count: 0,
            max_retries: 3,
            available_at: None,
        }
    }

    /// 中断した子プロセスの `NovelLockGuard::drop` は走らないため、その小説は
    /// `lock.yaml` のエントリが消えるまで 30 分間延期され続ける。解放は対象 ID
    /// だけを消し、他の小説のロックは残す (二重解放でも冪等)。
    #[test]
    fn release_novel_locks_removes_only_cancelled_ids() {
        let temp = tempfile::tempdir().unwrap();
        let _cwd = crate::test_support::set_current_dir_for_test(temp.path());
        seed_lock_file(temp.path(), &[12, 99]);
        let lock_path = temp.path().join(".narou").join("lock.yaml");

        release_novel_locks(&HashSet::from([12]));
        // 子の後始末と競合しても、同じ ID の二重解放で他のロックを失わない。
        release_novel_locks(&HashSet::from([12]));

        let locked: std::collections::HashMap<i64, Value> =
            serde_yaml::from_str(&std::fs::read_to_string(&lock_path).unwrap()).unwrap();
        assert_eq!(
            locked.keys().copied().collect::<HashSet<_>>(),
            HashSet::from([99])
        );
    }

    /// 解放した小説は `worker::pop_next_job` の延期条件
    /// (`load_active_locked_ids`) に現れなくなる。
    #[test]
    fn released_novel_is_no_longer_treated_as_locked() {
        let temp = tempfile::tempdir().unwrap();
        let _cwd = crate::test_support::set_current_dir_for_test(temp.path());
        seed_lock_file(temp.path(), &[12, 99]);

        release_novel_locks(&HashSet::from([12]));

        let active = crate::compat::load_active_locked_ids(Duration::from_secs(30 * 60)).unwrap();
        assert!(!active.contains(&12), "解放した小説は延期対象から外れる");
        assert!(active.contains(&99), "他の小説のロックは残す");
    }

    #[test]
    fn cancelled_novel_ids_reads_targets_of_cancelled_running_jobs() {
        let running_jobs = Mutex::new(vec![
            sample_queue_job("job-1", JobType::Download, "--force\t12"),
            sample_queue_job("job-2", JobType::Update, "--force\t12\t99"),
            sample_queue_job("job-3", JobType::Download, "42"),
        ]);

        assert_eq!(
            cancelled_novel_ids(&running_jobs, &["job-1".to_string()]),
            HashSet::from([12])
        );
        assert_eq!(
            cancelled_novel_ids(
                &running_jobs,
                &["job-1".to_string(), "job-2".to_string()]
            ),
            HashSet::from([12, 99])
        );
        assert!(cancelled_novel_ids(&running_jobs, &["missing".to_string()]).is_empty());
    }
}

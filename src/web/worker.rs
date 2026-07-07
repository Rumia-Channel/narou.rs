use std::collections::{HashMap, HashSet, VecDeque};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_yaml::Value;
use tokio::task::JoinHandle;

use super::push::PushServer;
use super::sort_state::sort_ids_for_request;
use crate::compat::{
    configure_web_subprocess_command, load_local_setting_bool, load_local_setting_string,
};
use crate::db::{with_database, with_database_mut};
use crate::progress::{WEB_PROGRESS_SCOPE_ENV, WS_LINE_PREFIX};
use crate::queue::{
    JobType, PersistentQueue, QueueExecutionSpec, QueueJob, QueueLane,
    WEBUI_MESSAGE_TEXT_META_KEY, WEBUI_MESSAGE_TYPE_META_KEY, WEBUI_UPDATE_START_MESSAGE_TYPE,
};

const MAX_FAILURE_DETAIL_LINES: usize = 8;
const MAX_FAILURE_DETAIL_CHARS: usize = 600;
const IDLE_EXTERNAL_QUEUE_POLL_SECS: u64 = 30;
/// Default exponential backoff schedule (seconds) for failed jobs.
const DEFAULT_RETRY_BACKOFF_SECS: &[i64] = &[60, 300, 900];
/// Detail substrings that mark a job outcome as a *permanent* failure which
/// must not be retried even when retries remain. Matching is case-insensitive.
const PERMANENT_FAILURE_KEYWORDS: &[&str] = &[
    "not found",
    "invalid argument",
    "invalidargument",
    "no such file",
    "permanent failure",
    "永久失敗",
    "恒久失敗",
];

/// Outcome of a failed-job transition: when `scheduled` is `true`, the worker
/// has already pushed the job back to the active pending queue with an
/// `available_at`; the broadcast step will emit `queue_retry`. Otherwise the
/// job has been moved to the failed history and `queue_failed` is broadcast.
#[derive(Debug, Clone, Default)]
struct RetrySchedule {
    scheduled: bool,
    retry_count: u32,
    max_retries: u32,
    backoff_secs: i64,
    available_at: i64,
}

#[derive(Debug, Clone, Default)]
struct JobRunResult {
    outcome: JobOutcome,
    detail: Option<String>,
    exit_code: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum JobOutcome {
    #[default]
    Failed,
    Completed,
    Partial,
    Cancelled,
}

#[derive(Clone, Copy)]
enum WorkerLane {
    All,
    Default,
    Secondary,
}

pub fn start_queue_workers(
    root_dir: PathBuf,
    queue: Arc<PersistentQueue>,
    push_server: Arc<PushServer>,
    running_jobs: Arc<parking_lot::Mutex<Vec<QueueJob>>>,
    running_child_pids: Arc<parking_lot::Mutex<HashMap<String, u32>>>,
    cancelled_job_ids: Arc<parking_lot::Mutex<HashSet<String>>>,
    concurrency_enabled: bool,
) -> Vec<JoinHandle<()>> {
    let job_transition_lock = Arc::new(parking_lot::Mutex::new(()));
    let lanes = if concurrency_enabled {
        vec![WorkerLane::Default, WorkerLane::Secondary]
    } else {
        vec![WorkerLane::All]
    };
    lanes
        .into_iter()
        .map(|lane| {
            start_queue_worker_for_lane(
                root_dir.clone(),
                Arc::clone(&queue),
                Arc::clone(&push_server),
                Arc::clone(&running_jobs),
                Arc::clone(&running_child_pids),
                Arc::clone(&cancelled_job_ids),
                Arc::clone(&job_transition_lock),
                lane,
            )
        })
        .collect()
}

fn start_queue_worker_for_lane(
    root_dir: PathBuf,
    queue: Arc<PersistentQueue>,
    push_server: Arc<PushServer>,
    running_jobs: Arc<parking_lot::Mutex<Vec<QueueJob>>>,
    running_child_pids: Arc<parking_lot::Mutex<HashMap<String, u32>>>,
    cancelled_job_ids: Arc<parking_lot::Mutex<HashSet<String>>>,
    job_transition_lock: Arc<parking_lot::Mutex<()>>,
    lane: WorkerLane,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let job = {
                let _guard = job_transition_lock.lock();
                let job = pop_next_job(queue.as_ref(), lane, &running_jobs);
                if let Some(job_ref) = job.as_ref() {
                    register_running_job(&running_jobs, job_ref);
                }
                job
            };

            let Some(job) = job else {
                tokio::select! {
                    _ = queue.wait_for_change() => {}
                    _ = tokio::time::sleep(Duration::from_secs(IDLE_EXTERNAL_QUEUE_POLL_SECS)) => {}
                }
                continue;
            };

            push_server.broadcast_event("queue_start", &job.id);

            let root_dir = root_dir.clone();
            let job_for_run = job.clone();
            let ps = Arc::clone(&push_server);
            let pid_ref = Arc::clone(&running_child_pids);
            let cancelled_ref = Arc::clone(&cancelled_job_ids);
            let queue_for_run = Arc::clone(&queue);
            let result = tokio::task::spawn_blocking(move || {
                execute_job(
                    &root_dir,
                    queue_for_run.as_ref(),
                    &job_for_run,
                    &ps,
                    &pid_ref,
                    &cancelled_ref,
                )
            })
            .await
            .unwrap_or_default();

            // Refresh in-memory database from disk (subprocess may have modified it)
            if let Err(e) = with_database_mut(|db| db.refresh()) {
                push_server.broadcast_error(&format!("DB更新エラー: {}", e));
            }

            let (queue_result, retry_scheduled) = {
                let _guard = job_transition_lock.lock();
                let mut retry_scheduled = RetrySchedule::default();
                let result = match result.outcome {
                    JobOutcome::Completed => queue.complete(&job.id),
                    JobOutcome::Partial => queue.partial(&job.id),
                    JobOutcome::Cancelled => queue.cancel(&job.id),
                    JobOutcome::Failed => {
                        if should_retry_job(&result, &job) {
                            let schedule = load_retry_backoff_schedule();
                            let backoff_secs =
                                compute_retry_backoff_secs(job.retry_count, &schedule);
                            let available_at = Some(chrono::Utc::now().timestamp() + backoff_secs);
                            match queue.requeue(&job.id, available_at) {
                                Ok(true) => {
                                    retry_scheduled = RetrySchedule {
                                        scheduled: true,
                                        retry_count: job.retry_count + 1,
                                        max_retries: job.max_retries,
                                        backoff_secs,
                                        available_at: available_at.unwrap_or(0),
                                    };
                                    Ok(())
                                }
                                Ok(false) => {
                                    // Lost the running slot to a concurrent worker; treat as
                                    // a permanent failure for this attempt.
                                    queue.fail(&job.id)
                                }
                                Err(error) => Err(error),
                            }
                        } else {
                            queue.fail(&job.id)
                        }
                    }
                };
                unregister_running_job(&running_jobs, &job.id);
                (result, retry_scheduled)
            };
            match result.outcome {
                JobOutcome::Completed => {
                    let _ = queue_result;
                    push_server.broadcast_event("queue_complete", &job.id);
                }
                JobOutcome::Partial => {
                    let _ = queue_result;
                    let mut data = serde_json::json!({ "job_id": job.id });
                    if let Some(exit_code) = result.exit_code {
                        data["exit_code"] = serde_json::json!(exit_code);
                    }
                    push_server.broadcast_raw(&serde_json::json!({
                        "type": "queue_partial",
                        "data": data,
                    }));
                }
                JobOutcome::Cancelled => {
                    let _ = queue_result;
                    push_server.broadcast_raw(&serde_json::json!({
                        "type": "queue_cancelled",
                        "data": { "job_id": job.id },
                    }));
                }
                JobOutcome::Failed => {
                    let _ = queue_result;
                    if retry_scheduled.scheduled {
                        push_server.broadcast_raw(&serde_json::json!({
                            "type": "queue_retry",
                            "data": {
                                "job_id": job.id,
                                "retry_count": retry_scheduled.retry_count,
                                "max_retries": retry_scheduled.max_retries,
                                "backoff_secs": retry_scheduled.backoff_secs,
                                "available_at": retry_scheduled.available_at,
                                "reason": failure_reason(&result),
                            },
                        }));
                    } else {
                        let mut data = serde_json::json!({
                            "job_id": job.id,
                            "reason": failure_reason(&result),
                        });
                        if load_local_setting_bool("webui.debug-mode")
                            && let Some(detail) = result.detail.as_deref()
                        {
                            data["detail"] = serde_json::Value::String(detail.to_string());
                        }
                        push_server.broadcast_raw(&serde_json::json!({
                            "type": "queue_failed",
                            "data": data,
                        }));
                    }
                }
            }
            match result.outcome {
                JobOutcome::Completed => {
                    let _ = queue_result;
                    push_server.broadcast_event("queue_complete", &job.id);
                }
                JobOutcome::Partial => {
                    let _ = queue_result;
                    let mut data = serde_json::json!({ "job_id": job.id });
                    if let Some(exit_code) = result.exit_code {
                        data["exit_code"] = serde_json::json!(exit_code);
                    }
                    push_server.broadcast_raw(&serde_json::json!({
                        "type": "queue_partial",
                        "data": data,
                    }));
                }
                JobOutcome::Cancelled => {
                    let _ = queue_result;
                    push_server.broadcast_raw(&serde_json::json!({
                        "type": "queue_cancelled",
                        "data": { "job_id": job.id },
                    }));
                }
                JobOutcome::Failed => {
                    let _ = queue_result;
                    let mut data = serde_json::json!({
                        "job_id": job.id,
                        "reason": failure_reason(&result),
                    });
                    if load_local_setting_bool("webui.debug-mode")
                        && let Some(detail) = result.detail.as_deref()
                    {
                        data["detail"] = serde_json::Value::String(detail.to_string());
                    }
                    push_server.broadcast_raw(&serde_json::json!({
                        "type": "queue_failed",
                        "data": data,
                    }));
                }
            }
            clear_progress_for_job(&push_server, &job.id);
            if should_reload_table_after_job(queue.as_ref(), &running_jobs) {
                push_server.broadcast_event("table.reload", "");
                push_server.broadcast_event("tag.updateCanvas", "");
            }
            push_server.broadcast_event("notification.queue", "");
        }
    })
}

fn pop_next_job(
    queue: &PersistentQueue,
    lane: WorkerLane,
    running_jobs: &parking_lot::Mutex<Vec<QueueJob>>,
) -> Option<QueueJob> {
    match lane {
        WorkerLane::All => queue.pop(),
        WorkerLane::Default => {
            let running = running_jobs.lock().clone();
            queue.pop_for_lane_excluding(QueueLane::Default, |candidate| {
                job_conflicts_with_running(candidate, &running)
            })
        }
        WorkerLane::Secondary => {
            let running = running_jobs.lock().clone();
            queue.pop_for_lane_excluding(QueueLane::Secondary, |candidate| {
                job_conflicts_with_running(candidate, &running)
            })
        }
    }
}

fn job_conflicts_with_running(candidate: &QueueJob, running_jobs: &[QueueJob]) -> bool {
    let candidate_ids = numeric_job_targets(candidate);
    if candidate_ids.is_empty() {
        return false;
    }
    running_jobs.iter().any(|running| {
        running.job_type.lane() != candidate.job_type.lane()
            && numeric_job_targets(running)
                .iter()
                .any(|id| candidate_ids.contains(id))
    })
}

fn numeric_job_targets(job: &QueueJob) -> HashSet<i64> {
    job.target
        .split('\t')
        .filter_map(|part| part.parse::<i64>().ok())
        .collect()
}

fn register_running_job(running_jobs: &parking_lot::Mutex<Vec<QueueJob>>, job: &QueueJob) {
    let mut guard = running_jobs.lock();
    guard.retain(|existing| existing.id != job.id);
    guard.push(job.clone());
}

fn unregister_running_job(running_jobs: &parking_lot::Mutex<Vec<QueueJob>>, job_id: &str) {
    running_jobs.lock().retain(|job| job.id != job_id);
}

fn should_reload_table_after_job(
    queue: &PersistentQueue,
    running_jobs: &parking_lot::Mutex<Vec<QueueJob>>,
) -> bool {
    match load_local_setting_string("webui.table.reload-timing")
        .as_deref()
        .unwrap_or("every")
    {
        "queue" => queue.active_pending_count() == 0 && running_jobs.lock().is_empty(),
        _ => true,
    }
}

fn clear_progress_for_job(push_server: &Arc<PushServer>, job_id: &str) {
    for target_console in ["stdout", "stdout2"] {
        push_server.broadcast_raw(&serde_json::json!({
            "type": "progressbar.clear",
            "data": { "scope": job_id },
            "target_console": target_console,
        }));
    }
}

fn execute_job(
    root_dir: &Path,
    queue: &PersistentQueue,
    job: &QueueJob,
    push_server: &Arc<PushServer>,
    running_pids: &Arc<parking_lot::Mutex<HashMap<String, u32>>>,
    cancelled_job_ids: &Arc<parking_lot::Mutex<HashSet<String>>>,
) -> JobRunResult {
    if matches!(job.job_type, JobType::AutoUpdate) {
        let success = crate::web::scheduler::execute_auto_update(
            root_dir,
            Arc::clone(push_server),
            &job.id,
            Arc::clone(running_pids),
        );
        return JobRunResult {
            outcome: if success {
                JobOutcome::Completed
            } else {
                JobOutcome::Failed
            },
            detail: None,
            exit_code: None,
        };
    }

    let target_console = console_target_for_job(job.job_type);
    let Ok(exe) = std::env::current_exe() else {
        push_server.broadcast_echo("エラー: 実行ファイルパスを取得できません", target_console);
        return JobRunResult {
            outcome: JobOutcome::Failed,
            detail: Some("エラー: 実行ファイルパスを取得できません".to_string()),
            exit_code: None,
        };
    };

    let spec = queue.execution_spec(&job.id);
    if let Some(spec) = spec.as_ref()
        && spec.cmd == "update_general_lastup"
    {
        return execute_update_general_lastup_job(
            root_dir,
            &exe,
            spec,
            push_server,
            running_pids,
            cancelled_job_ids,
            &job.id,
            target_console,
        );
    }

    let mut command = new_web_subprocess_command(&exe, root_dir, &job.id);

    if let Some(spec) = spec {
        match spec.cmd.as_str() {
            "download" => {
                command.arg("download");
                for arg in spec.args {
                    command.arg(arg);
                }
            }
            "download_force" => {
                command.arg("download").arg("--force");
                for arg in spec.args {
                    command.arg(arg);
                }
            }
            "update" => {
                command.arg("update");
                append_update_args(&mut command, push_server, target_console, &spec);
            }
            "update_by_tag" => {
                command.arg("update");
                append_update_by_tag_args(&mut command, &spec);
            }
            "convert" => {
                let (targets, device) = match convert_targets_and_device(&spec) {
                    Ok(value) => value,
                    Err(message) => {
                        push_server.broadcast_echo(&message, target_console);
                        return JobRunResult {
                            outcome: JobOutcome::Failed,
                            detail: Some(message),
                            exit_code: None,
                        };
                    }
                };
                command.arg("convert").arg("--no-open");
                for target in targets {
                    command.arg(target);
                }
                if let Some(device) = device {
                    command.env("NAROU_RS_WEB_DEVICE", device);
                }
            }
            "send" => {
                command.arg("send");
                for arg in spec.args {
                    command.arg(arg);
                }
            }
            "backup_bookmark" => {
                command.arg("send").arg("--backup-bookmark");
            }
            "backup" => {
                command.arg("backup");
                for arg in spec.args {
                    command.arg(arg);
                }
            }
            "mail" => {
                command.arg("mail");
                for arg in spec.args {
                    command.arg(arg);
                }
            }
            "freeze" => {
                command.arg("freeze");
                for arg in spec.args {
                    command.arg(arg);
                }
            }
            "remove" => {
                command.arg("remove");
                for arg in spec.args {
                    command.arg(arg);
                }
            }
            "inspect" => {
                command.arg("inspect");
                for arg in spec.args {
                    command.arg(arg);
                }
            }
            "diff" => {
                if spec.args.is_empty() {
                    push_server.broadcast_echo("diff task has no arguments", target_console);
                    return JobRunResult {
                        outcome: JobOutcome::Failed,
                        detail: Some("diff task has no arguments".to_string()),
                        exit_code: None,
                    };
                }
                return execute_diff_job(
                    root_dir,
                    &exe,
                    &spec.args,
                    push_server,
                    running_pids,
                    cancelled_job_ids,
                    &job.id,
                    target_console,
                );
            }
            "diff_clean" => {
                command.arg("diff").arg("--clean");
                for arg in spec.args {
                    command.arg(arg);
                }
            }
            "setting_burn" => {
                command.arg("setting").arg("--burn");
                for arg in spec.args {
                    command.arg(arg);
                }
            }
            "auto_update" => unreachable!(),
            unsupported => {
                push_server.broadcast_echo(
                    &format!("未対応の復元キューコマンドです: {}", unsupported),
                    target_console,
                );
                return JobRunResult {
                    outcome: JobOutcome::Failed,
                    detail: Some(format!("未対応の復元キューコマンドです: {}", unsupported)),
                    exit_code: None,
                };
            }
        }
    } else {
        match job.job_type {
            JobType::Download => {
                command.arg("download");
                for part in job.target.split('\t') {
                    if !part.is_empty() {
                        command.arg(part);
                    }
                }
            }
            JobType::Update => {
                command.arg("update");
                if !job.target.is_empty() {
                    let spec = QueueExecutionSpec {
                        cmd: "update".to_string(),
                        args: job
                        .target
                        .split('\t')
                        .map(|part| part.to_string())
                        .collect(),
                        meta: serde_yaml::Mapping::new(),
                    };
                    append_update_args(&mut command, push_server, target_console, &spec);
                }
            }
            JobType::Convert => {
                let (target, device) = match parse_convert_job_target(&job.target) {
                    Ok(value) => value,
                    Err(message) => {
                        push_server.broadcast_echo(&message, target_console);
                        return JobRunResult {
                            outcome: JobOutcome::Failed,
                            detail: Some(message),
                            exit_code: None,
                        };
                    }
                };
                command.arg("convert").arg("--no-open").arg(target);
                if let Some(device) = device {
                    command.env("NAROU_RS_WEB_DEVICE", device);
                }
            }
            JobType::Send => {
                command.arg("send").arg(&job.target);
            }
            JobType::Backup => {
                command.arg("backup").arg(&job.target);
            }
            JobType::Mail => {
                command.arg("mail");
                for part in job.target.split('\t') {
                    if !part.is_empty() {
                        command.arg(part);
                    }
                }
            }
            JobType::AutoUpdate => unreachable!(),
        }
    }
    spawn_and_stream_command(
        command,
        push_server,
        running_pids,
        cancelled_job_ids,
        &job.id,
        target_console,
    )
}

fn new_web_subprocess_command(exe: &Path, root_dir: &Path, job_id: &str) -> std::process::Command {
    let mut command = std::process::Command::new(exe);
    command
        .current_dir(root_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_web_subprocess_command(&mut command);
    command.env(WEB_PROGRESS_SCOPE_ENV, job_id);
    command
}

fn execution_spec_meta_bool(spec: &QueueExecutionSpec, key: &str) -> bool {
    match spec.meta.get(Value::String(key.to_string())) {
        Some(Value::Bool(value)) => *value,
        Some(Value::String(value)) => matches!(value.as_str(), "1" | "true" | "yes" | "on"),
        _ => false,
    }
}

fn execution_spec_meta_string(spec: &QueueExecutionSpec, key: &str) -> Option<String> {
    match spec.meta.get(Value::String(key.to_string())) {
        Some(Value::String(value)) if !value.is_empty() => Some(value.clone()),
        _ => None,
    }
}

fn execution_spec_update_start_message(spec: &QueueExecutionSpec) -> Option<String> {
    match (
        spec.meta
            .get(Value::String(WEBUI_MESSAGE_TYPE_META_KEY.to_string())),
        spec.meta
            .get(Value::String(WEBUI_MESSAGE_TEXT_META_KEY.to_string())),
    ) {
        (Some(Value::String(message_type)), Some(Value::String(message)))
            if message_type == WEBUI_UPDATE_START_MESSAGE_TYPE && !message.is_empty() =>
        {
            Some(message.clone())
        }
        _ => None,
    }
}

#[allow(dead_code)]
fn execution_spec_meta_strings(spec: &QueueExecutionSpec, key: &str) -> Vec<String> {
    match spec.meta.get(Value::String(key.to_string())) {
        Some(Value::Sequence(values)) => values
            .iter()
            .filter_map(|value| match value {
                Value::String(value) if !value.is_empty() => Some(value.clone()),
                Value::Number(value) => Some(value.to_string()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn convert_targets_and_device(spec: &QueueExecutionSpec) -> Result<(Vec<String>, Option<String>), String> {
    let device = super::normalize_web_device_override(execution_spec_meta_string(spec, "device").as_deref())?;
    if device.is_some() {
        return Ok((spec.args.clone(), device));
    }
    if spec.args.len() > 1
        && let Some(last) = spec.args.last()
        && let Ok(Some(device)) = super::normalize_web_device_override(Some(last.as_str()))
    {
        return Ok((spec.args[..spec.args.len() - 1].to_vec(), Some(device)));
    }
    Ok((spec.args.clone(), None))
}

fn refresh_web_state(push_server: &Arc<PushServer>) {
    if let Err(e) = with_database_mut(|db| db.refresh()) {
        push_server.broadcast_error(&format!("DB更新エラー: {}", e));
    }
    push_server.broadcast_event("table.reload", "");
    push_server.broadcast_event("tag.updateCanvas", "");
}

fn execute_update_general_lastup_job(
    root_dir: &Path,
    exe: &Path,
    spec: &QueueExecutionSpec,
    push_server: &Arc<PushServer>,
    running_pids: &Arc<parking_lot::Mutex<HashMap<String, u32>>>,
    cancelled_job_ids: &Arc<parking_lot::Mutex<HashSet<String>>>,
    job_id: &str,
    target_console: &str,
) -> JobRunResult {
    let mut command = new_web_subprocess_command(exe, root_dir, job_id);
    command.arg("update").arg("--gl");
    append_update_args(&mut command, push_server, target_console, spec);
    let result = spawn_and_stream_command(
        command,
        push_server,
        running_pids,
        cancelled_job_ids,
        job_id,
        target_console,
    );
    if !matches!(result.outcome, JobOutcome::Completed | JobOutcome::Partial)
        || !execution_spec_meta_bool(spec, "update_modified")
    {
        return result;
    }

    refresh_web_state(push_server);
    push_server.broadcast_echo(
        "<span style=\"color:#d7ba7d\">modified タグの付いた小説を更新します</span>",
        target_console,
    );

    let modified_ids = current_modified_update_target_ids();
    if modified_ids.is_empty() {
        push_server.broadcast_echo("modified タグの付いた小説はありません", target_console);
        return result;
    }

    let mut followup = new_web_subprocess_command(exe, root_dir, job_id);
    followup.arg("update");
    for id in modified_ids {
        followup.arg(id);
    }
    spawn_and_stream_command(
        followup,
        push_server,
        running_pids,
        cancelled_job_ids,
        job_id,
        target_console,
    )
}

fn update_by_tag_update_args(spec: &QueueExecutionSpec) -> Vec<String> {
    let snapshot_ids = execution_spec_meta_strings(spec, "snapshot_ids");
    if !snapshot_ids.is_empty() {
        return snapshot_ids;
    }

    let mut args = Vec::new();
    if let Some(sort_key) = execution_spec_meta_string(spec, "sort_by") {
        args.push("--sort-by".to_string());
        args.push(sort_key);
    }
    args.extend(spec.args.clone());
    args
}

fn append_update_by_tag_args(command: &mut std::process::Command, spec: &QueueExecutionSpec) {
    for part in update_by_tag_update_args(spec) {
        if !part.is_empty() {
            command.arg(part);
        }
    }
}

fn current_modified_update_target_ids() -> Vec<String> {
    let ids = with_database(|db| {
        Ok(db
            .tag_index()
            .get("modified")
            .map(|ids| ids.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default())
    })
    .unwrap_or_default();
    sort_ids_for_request(&ids, None, None)
        .into_iter()
        .map(|id| id.to_string())
        .collect()
}

fn append_update_args(
    command: &mut std::process::Command,
    push_server: &Arc<PushServer>,
    target_console: &str,
    spec: &QueueExecutionSpec,
) {
    if let Some(message) = execution_spec_update_start_message(spec) {
        push_server.broadcast_echo(
            &format!("<span style=\"color:#bbb\">{}</span>", message),
            target_console,
        );
    }
    for part in &spec.args {
        if !part.is_empty() {
            command.arg(part);
        }
    }
}

fn execute_diff_job(
    root_dir: &Path,
    exe: &Path,
    args: &[String],
    push_server: &Arc<PushServer>,
    running_pids: &Arc<parking_lot::Mutex<HashMap<String, u32>>>,
    cancelled_job_ids: &Arc<parking_lot::Mutex<HashSet<String>>>,
    job_id: &str,
    target_console: &str,
) -> JobRunResult {
    if args.len() < 2 {
        push_server.broadcast_echo("diff task is missing the diff number", target_console);
        return JobRunResult {
            outcome: JobOutcome::Failed,
            detail: Some("diff task is missing the diff number".to_string()),
            exit_code: None,
        };
    }
    let (ids, number) = args.split_at(args.len() - 1);
    let Some(number) = number.first() else {
        push_server.broadcast_echo("diff task is missing the diff number", target_console);
        return JobRunResult {
            outcome: JobOutcome::Failed,
            detail: Some("diff task is missing the diff number".to_string()),
            exit_code: None,
        };
    };
    let mut saw_failure = false;
    let mut saw_partial = false;
    let mut saw_cancelled = false;
    let mut details = Vec::new();
    for id in ids {
        let mut command = std::process::Command::new(exe);
        command
            .current_dir(root_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .arg("diff")
            .arg("--no-tool")
            .arg(id)
            .arg("--number")
            .arg(number);
        configure_web_subprocess_command(&mut command);
        command.env(WEB_PROGRESS_SCOPE_ENV, job_id);
        let result = spawn_and_stream_command(
            command,
            push_server,
            running_pids,
            cancelled_job_ids,
            job_id,
            target_console,
        );
        match result.outcome {
            JobOutcome::Completed => {}
            JobOutcome::Partial => saw_partial = true,
            JobOutcome::Cancelled => saw_cancelled = true,
            JobOutcome::Failed => saw_failure = true,
        }
        if let Some(detail) = result.detail {
            details.push(detail);
        }
    }
    JobRunResult {
        outcome: if saw_cancelled {
            JobOutcome::Cancelled
        } else if saw_failure {
            JobOutcome::Failed
        } else if saw_partial {
            JobOutcome::Partial
        } else {
            JobOutcome::Completed
        },
        detail: summarize_failure_details(&details),
        exit_code: None,
    }
}

fn spawn_and_stream_command(
    mut command: std::process::Command,
    push_server: &Arc<PushServer>,
    running_pids: &Arc<parking_lot::Mutex<HashMap<String, u32>>>,
    cancelled_job_ids: &Arc<parking_lot::Mutex<HashSet<String>>>,
    job_id: &str,
    target_console: &str,
) -> JobRunResult {
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            push_server.broadcast_echo(&format!("プロセス起動失敗: {}", e), target_console);
            return JobRunResult {
                outcome: JobOutcome::Failed,
                detail: Some(format!("プロセス起動失敗: {}", e)),
                exit_code: None,
            };
        }
    };

    running_pids.lock().insert(job_id.to_string(), child.id());
    let recent_output = Arc::new(parking_lot::Mutex::new(VecDeque::new()));

    let stdout = child.stdout.take();
    let ps_out = Arc::clone(push_server);
    let stdout_target_console = target_console.to_string();
    let stdout_recent = Arc::clone(&recent_output);
    let stdout_thread = std::thread::spawn(move || {
        if let Some(out) = stdout {
            let reader = std::io::BufReader::new(out);
            for line in reader.lines() {
                match line {
                    Ok(text) => {
                        if let Some(json_str) = text.strip_prefix(WS_LINE_PREFIX) {
                            if let Ok(msg) = serde_json::from_str::<serde_json::Value>(json_str) {
                                if is_novel_refresh_event(&msg) {
                                    refresh_db_and_broadcast_table_reload(ps_out.as_ref());
                                    continue;
                                }
                                let routed =
                                    route_structured_web_message(msg, &stdout_target_console);
                                ps_out.broadcast_raw(&routed);
                            }
                        } else {
                            remember_failure_line(stdout_recent.as_ref(), &text);
                            ps_out.broadcast_echo(&text, &stdout_target_console);
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    });

    let stderr = child.stderr.take();
    let ps_err = Arc::clone(push_server);
    let stderr_target_console = target_console.to_string();
    let stderr_recent = Arc::clone(&recent_output);
    let stderr_thread = std::thread::spawn(move || {
        if let Some(err) = stderr {
            let reader = std::io::BufReader::new(err);
            for line in reader.lines() {
                match line {
                    Ok(text) => {
                        remember_failure_line(stderr_recent.as_ref(), &text);
                        ps_err.broadcast_echo(&text, &stderr_target_console);
                    }
                    Err(_) => break,
                }
            }
        }
    });

    let status = child.wait();
    running_pids.lock().remove(job_id);
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
    let cancelled = cancelled_job_ids.lock().remove(job_id);
    if cancelled {
        return JobRunResult {
            outcome: JobOutcome::Cancelled,
            detail: None,
            exit_code: status.ok().and_then(|value| value.code()),
        };
    }
    match status {
        Ok(status) => {
            let outcome = classify_job_outcome(&status);
            let exit_code = status.code();
            let detail = matches!(outcome, JobOutcome::Failed)
                .then(|| summarize_failure_output(recent_output.as_ref()))
                .flatten();
            JobRunResult {
                outcome,
                detail,
                exit_code,
            }
        }
        Err(error) => JobRunResult {
            outcome: JobOutcome::Failed,
            detail: Some(format!("終了待機失敗: {}", error)),
            exit_code: None,
        },
    }
}

fn classify_job_outcome(status: &std::process::ExitStatus) -> JobOutcome {
    match status.code() {
        Some(0) => JobOutcome::Completed,
        Some(1..=127) => JobOutcome::Partial,
        _ => JobOutcome::Failed,
    }
}

fn remember_failure_line(lines: &parking_lot::Mutex<VecDeque<String>>, text: &str) {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "<hr>" || trimmed.starts_with('\u{2015}') {
        return;
    }

    let mut guard = lines.lock();
    if guard.back().is_some_and(|line| line == trimmed) {
        return;
    }
    guard.push_back(trimmed.to_string());
    if guard.len() > MAX_FAILURE_DETAIL_LINES {
        guard.pop_front();
    }
}

fn summarize_failure_output(lines: &parking_lot::Mutex<VecDeque<String>>) -> Option<String> {
    let entries: Vec<String> = lines.lock().iter().cloned().collect();
    summarize_failure_details(&entries)
}

fn summarize_failure_details(lines: &[String]) -> Option<String> {
    let mut text = lines
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        return None;
    }
    if text.chars().count() > MAX_FAILURE_DETAIL_CHARS {
        text = text.chars().take(MAX_FAILURE_DETAIL_CHARS).collect::<String>();
        text.push('…');
    }
    Some(text)
}

fn failure_reason(result: &JobRunResult) -> String {
    if let Some(detail) = result.detail.as_deref() {
        let reason = detail
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or(detail)
            .trim();
        if !reason.is_empty() {
            return reason.to_string();
        }
    }
    match result.exit_code {
        Some(code) => format!("終了コード {}", code),
        None => "終了コード不明".to_string(),
    }
}

fn should_retry_job(result: &JobRunResult, job: &QueueJob) -> bool {
    is_transient_failure(result) && job.retry_count < job.max_retries
}

fn is_transient_failure(result: &JobRunResult) -> bool {
    if let Some(detail) = result.detail.as_deref() {
        let lower = detail.to_lowercase();
        for keyword in PERMANENT_FAILURE_KEYWORDS {
            if lower.contains(keyword) {
                return false;
            }
        }
    }
    true
}

fn compute_retry_backoff_secs(retry_count: u32, schedule: &[i64]) -> i64 {
    if schedule.is_empty() {
        return DEFAULT_RETRY_BACKOFF_SECS[0];
    }
    let idx = (retry_count as usize).min(schedule.len() - 1);
    schedule[idx]
}

fn load_retry_backoff_schedule() -> Vec<i64> {
    match crate::compat::load_local_setting_string("queue.retry-backoff") {
        Some(spec) => {
            let parsed = parse_backoff_spec(&spec);
            if parsed.is_empty() {
                DEFAULT_RETRY_BACKOFF_SECS.to_vec()
            } else {
                parsed
            }
        }
        None => DEFAULT_RETRY_BACKOFF_SECS.to_vec(),
    }
}

fn parse_backoff_spec(spec: &str) -> Vec<i64> {
    spec.split(',')
        .map(str::trim)
        .filter_map(parse_backoff_value)
        .collect()
}

fn parse_backoff_value(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (num_str, multiplier): (&str, i64) =
        if let Some(rest) = raw.strip_suffix(['s', 'S']) {
            (rest, 1)
        } else if let Some(rest) = raw.strip_suffix(['m', 'M']) {
            (rest, 60)
        } else if let Some(rest) = raw.strip_suffix(['h', 'H']) {
            (rest, 3600)
        } else {
            (raw, 1)
        };
    let num_str = num_str.trim();
    let parsed = num_str.parse::<i64>().ok()?;
    if parsed < 0 {
        return None;
    }
    Some(parsed * multiplier)
}

fn console_target_for_job(job_type: JobType) -> &'static str {
    match job_type {
        JobType::Download | JobType::Update | JobType::AutoUpdate => "stdout",
        JobType::Convert | JobType::Send | JobType::Backup | JobType::Mail => {
            super::non_external_console_target()
        }
    }
}

fn route_structured_web_message(
    mut message: serde_json::Value,
    target_console: &str,
) -> serde_json::Value {
    if target_console != "stdout"
        && message.get("target_console").is_none()
        && let Some(object) = message.as_object_mut()
    {
        object.insert(
            "target_console".to_string(),
            serde_json::Value::String(target_console.to_string()),
        );
    }
    message
}

/// Returns `true` for the per-novel refresh event emitted by child CLI
/// subprocesses (see [`crate::progress::emit_novel_refresh`]). The web server
/// reloads its DB cache and broadcasts a `table.reload` so the UI reflects
/// per-novel state changes (e.g. modified-tag removal) the moment they happen.
pub(crate) fn is_novel_refresh_event(message: &serde_json::Value) -> bool {
    message
        .get("type")
        .and_then(|v| v.as_str())
        .is_some_and(|t| t == "novel.refresh")
}

pub(crate) fn refresh_db_and_broadcast_table_reload(push_server: &PushServer) {
    let _ = with_database_mut(|db| db.refresh());
    push_server.broadcast_event("table.reload", "");
    push_server.broadcast_event("tag.updateCanvas", "");
}

fn parse_convert_job_target(value: &str) -> Result<(&str, Option<String>), String> {
    let mut parts = value.splitn(2, '\t');
    let target = parts.next().unwrap_or(value);
    let device = super::normalize_web_device_override(parts.next())?;
    Ok((target, device))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;

    use super::{
        MAX_FAILURE_DETAIL_CHARS, MAX_FAILURE_DETAIL_LINES, JobOutcome, JobRunResult,
        classify_job_outcome, clear_progress_for_job, compute_retry_backoff_secs,
        console_target_for_job, convert_targets_and_device, execution_spec_meta_bool,
        execution_spec_meta_string, execution_spec_meta_strings, failure_reason,
        is_transient_failure, job_conflicts_with_running, parse_backoff_spec,
        parse_backoff_value, parse_convert_job_target, remember_failure_line,
        route_structured_web_message, should_retry_job, summarize_failure_details,
        update_by_tag_update_args,
    };
    use crate::queue::{JobType, QueueJob};

    #[test]
    fn parse_convert_job_target_splits_device_override() {
        assert_eq!(
            parse_convert_job_target("1\tkindle").unwrap(),
            ("1", Some("kindle".to_string()))
        );
        assert_eq!(parse_convert_job_target("1").unwrap(), ("1", None));
    }

    #[test]
    fn parse_convert_job_target_rejects_invalid_device_override() {
        assert!(parse_convert_job_target("1\tunknown").is_err());
    }

    #[test]
    fn convert_targets_and_device_reads_batched_targets_and_meta_override() {
        let mut meta = serde_yaml::Mapping::new();
        meta.insert(
            serde_yaml::Value::String("device".to_string()),
            serde_yaml::Value::String("kindle".to_string()),
        );
        let spec = crate::queue::QueueExecutionSpec {
            cmd: "convert".to_string(),
            args: vec!["1".to_string(), "2".to_string()],
            meta,
        };
        assert_eq!(
            convert_targets_and_device(&spec).unwrap(),
            (vec!["1".to_string(), "2".to_string()], Some("kindle".to_string()))
        );
    }

    #[test]
    fn convert_targets_and_device_keeps_numeric_last_arg_as_target() {
        let spec = crate::queue::QueueExecutionSpec {
            cmd: "convert".to_string(),
            args: vec!["1".to_string(), "2".to_string()],
            meta: serde_yaml::Mapping::new(),
        };
        assert_eq!(
            convert_targets_and_device(&spec).unwrap(),
            (vec!["1".to_string(), "2".to_string()], None)
        );
    }

    #[test]
    fn convert_targets_and_device_reads_legacy_trailing_device() {
        let spec = crate::queue::QueueExecutionSpec {
            cmd: "convert".to_string(),
            args: vec!["1".to_string(), "kindle".to_string()],
            meta: serde_yaml::Mapping::new(),
        };
        assert_eq!(
            convert_targets_and_device(&spec).unwrap(),
            (vec!["1".to_string()], Some("kindle".to_string()))
        );
    }

    #[test]
    fn console_target_splits_external_site_jobs_from_local_jobs() {
        assert_eq!(console_target_for_job(JobType::Download), "stdout");
        assert_eq!(console_target_for_job(JobType::Update), "stdout");
        assert_eq!(console_target_for_job(JobType::AutoUpdate), "stdout");
        assert_eq!(
            console_target_for_job(JobType::Convert),
            crate::web::non_external_console_target()
        );
        assert_eq!(
            console_target_for_job(JobType::Send),
            crate::web::non_external_console_target()
        );
        assert_eq!(
            console_target_for_job(JobType::Backup),
            crate::web::non_external_console_target()
        );
        assert_eq!(
            console_target_for_job(JobType::Mail),
            crate::web::non_external_console_target()
        );
    }

    #[test]
    fn job_conflict_blocks_same_novel_across_lanes_only() {
        let running_update = QueueJob {
            id: "running-update".to_string(),
            job_type: JobType::Update,
            target: "12".to_string(),
            created_at: 0,
            retry_count: 0,
            max_retries: 3,
            available_at: None,
        };
        let same_novel_convert = QueueJob {
            id: "convert".to_string(),
            job_type: JobType::Convert,
            target: "12\tkindle".to_string(),
            created_at: 0,
            retry_count: 0,
            max_retries: 3,
            available_at: None,
        };
        let other_novel_convert = QueueJob {
            target: "13".to_string(),
            ..same_novel_convert.clone()
        };
        let same_lane_download = QueueJob {
            id: "download".to_string(),
            job_type: JobType::Download,
            target: "12".to_string(),
            created_at: 0,
            retry_count: 0,
            max_retries: 3,
            available_at: None,
        };

        assert!(job_conflicts_with_running(
            &same_novel_convert,
            &[running_update.clone()]
        ));
        assert!(!job_conflicts_with_running(
            &other_novel_convert,
            &[running_update.clone()]
        ));
        assert!(!job_conflicts_with_running(
            &same_lane_download,
            &[running_update]
        ));
    }

    #[test]
    fn route_structured_web_message_adds_target_console() {
        let message = serde_json::json!({
            "type": "progressbar.init",
            "data": { "topic": "convert" }
        });
        let routed = route_structured_web_message(message, "stdout2");
        assert_eq!(routed["target_console"], "stdout2");
    }

    #[test]
    fn classify_job_outcome_distinguishes_complete_partial_and_failed() {
        assert_eq!(
            classify_job_outcome(&std::process::ExitStatus::from_raw(0)),
            JobOutcome::Completed
        );
        assert_eq!(
            classify_job_outcome(&std::process::ExitStatus::from_raw(exit_status_raw(1))),
            JobOutcome::Partial
        );
        assert_eq!(
            classify_job_outcome(&std::process::ExitStatus::from_raw(exit_status_raw(128))),
            JobOutcome::Failed
        );
    }

    #[cfg(unix)]
    fn exit_status_raw(code: i32) -> i32 {
        code << 8
    }

    #[cfg(windows)]
    fn exit_status_raw(code: u32) -> u32 {
        code
    }

    #[test]
    fn remember_failure_line_deduplicates_and_limits_recent_lines() {
        let lines = parking_lot::Mutex::new(std::collections::VecDeque::new());
        remember_failure_line(&lines, "first");
        remember_failure_line(&lines, "first");
        for index in 0..MAX_FAILURE_DETAIL_LINES {
            remember_failure_line(&lines, &format!("line-{index}"));
        }
        let collected: Vec<String> = lines.lock().iter().cloned().collect();
        assert_eq!(collected.len(), MAX_FAILURE_DETAIL_LINES);
        assert!(!collected.iter().any(|line| line == "first"));
        assert_eq!(collected.last().map(String::as_str), Some("line-7"));
    }

    #[test]
    fn summarize_failure_details_truncates_long_output() {
        let detail = summarize_failure_details(&[String::from("x".repeat(MAX_FAILURE_DETAIL_CHARS + 10))])
            .expect("detail");
        assert!(detail.ends_with('…'));
        assert!(detail.chars().count() <= MAX_FAILURE_DETAIL_CHARS + 1);
    }

    #[test]
    fn failure_reason_prefers_detail_then_exit_code() {
        assert_eq!(
            failure_reason(&super::JobRunResult {
                outcome: JobOutcome::Failed,
                detail: Some(" first line \nsecond line".to_string()),
                exit_code: Some(12),
            }),
            "first line"
        );
        assert_eq!(
            failure_reason(&super::JobRunResult {
                outcome: JobOutcome::Failed,
                detail: None,
                exit_code: Some(9),
            }),
            "終了コード 9"
        );
    }

    fn make_failed_result(detail: Option<&str>, exit_code: Option<i32>) -> JobRunResult {
        JobRunResult {
            outcome: JobOutcome::Failed,
            detail: detail.map(|value| value.to_string()),
            exit_code,
        }
    }

    #[test]
    fn is_transient_failure_treats_missing_detail_as_transient() {
        assert!(is_transient_failure(&make_failed_result(None, Some(128))));
        assert!(is_transient_failure(&make_failed_result(Some(""), Some(2))));
    }

    #[test]
    fn is_transient_failure_flags_permanent_keywords() {
        assert!(!is_transient_failure(&make_failed_result(
            Some("HTTP 404 Not Found"),
            None
        )));
        assert!(!is_transient_failure(&make_failed_result(
            Some("error: Invalid argument"),
            None
        )));
        assert!(!is_transient_failure(&make_failed_result(
            Some("恒久失敗: 認証情報が不正です"),
            None
        )));
        // Permanent keywords only match if they appear in the detail; a generic
        // network error must stay transient.
        assert!(is_transient_failure(&make_failed_result(
            Some("connection reset by peer"),
            None
        )));
    }

    #[test]
    fn compute_retry_backoff_secs_indexes_schedule_and_clamps_tail() {
        let schedule = [60, 300, 900];
        assert_eq!(compute_retry_backoff_secs(0, &schedule), 60);
        assert_eq!(compute_retry_backoff_secs(1, &schedule), 300);
        assert_eq!(compute_retry_backoff_secs(2, &schedule), 900);
        // retry_count beyond schedule length uses the last entry.
        assert_eq!(compute_retry_backoff_secs(5, &schedule), 900);
    }

    #[test]
    fn compute_retry_backoff_secs_falls_back_when_schedule_empty() {
        let empty: [i64; 0] = [];
        assert_eq!(compute_retry_backoff_secs(0, &empty), 60);
    }

    #[test]
    fn parse_backoff_value_handles_units() {
        assert_eq!(parse_backoff_value("30"), Some(30));
        assert_eq!(parse_backoff_value("1m"), Some(60));
        assert_eq!(parse_backoff_value("5m"), Some(300));
        assert_eq!(parse_backoff_value("2H"), Some(7_200));
        assert_eq!(parse_backoff_value(" 45s "), Some(45));
        assert_eq!(parse_backoff_value(""), None);
        assert_eq!(parse_backoff_value("-5"), None);
        assert_eq!(parse_backoff_value("abc"), None);
    }

    #[test]
    fn parse_backoff_spec_splits_csv() {
        assert_eq!(parse_backoff_spec("30,1m,2h"), vec![30, 60, 7_200]);
        assert_eq!(parse_backoff_spec("  10s , 5m , 15m  "), vec![10, 300, 900]);
        assert_eq!(parse_backoff_spec(""), Vec::<i64>::new());
    }

    #[test]
    fn should_retry_job_combines_transient_and_retry_budget() {
        let transient = make_failed_result(Some("connection reset"), Some(128));
        let permanent = make_failed_result(Some("HTTP 404 Not Found"), None);

        let mut job = QueueJob {
            id: "j1".to_string(),
            job_type: JobType::Download,
            target: "1".to_string(),
            created_at: 0,
            retry_count: 0,
            max_retries: 3,
            available_at: None,
        };
        assert!(should_retry_job(&transient, &job));
        assert!(!should_retry_job(&permanent, &job));

        job.retry_count = 3;
        assert!(!should_retry_job(&transient, &job));
    }

    #[test]
    fn clear_progress_for_job_targets_both_consoles_with_scope() {
        let push_server = std::sync::Arc::new(crate::web::push::PushServer::new());
        let mut receiver = push_server.channel().subscribe();

        clear_progress_for_job(&push_server, "job-123");

        let first: serde_json::Value = serde_json::from_str(&receiver.try_recv().unwrap()).unwrap();
        let second: serde_json::Value = serde_json::from_str(&receiver.try_recv().unwrap()).unwrap();
        let targets = [first["target_console"].as_str().unwrap(), second["target_console"].as_str().unwrap()];

        assert_eq!(first["type"], "progressbar.clear");
        assert_eq!(second["type"], "progressbar.clear");
        assert!(targets.contains(&"stdout"));
        assert!(targets.contains(&"stdout2"));
        assert_eq!(first["data"]["scope"], "job-123");
        assert_eq!(second["data"]["scope"], "job-123");
    }

    #[test]
    fn execution_spec_meta_bool_accepts_boolean_and_string_values() {
        let mut bool_meta = serde_yaml::Mapping::new();
        bool_meta.insert(
            serde_yaml::Value::String("update_modified".to_string()),
            serde_yaml::Value::Bool(true),
        );
        let bool_spec = crate::queue::QueueExecutionSpec {
            cmd: "update_general_lastup".to_string(),
            args: Vec::new(),
            meta: bool_meta,
        };
        assert!(execution_spec_meta_bool(&bool_spec, "update_modified"));

        let mut string_meta = serde_yaml::Mapping::new();
        string_meta.insert(
            serde_yaml::Value::String("update_modified".to_string()),
            serde_yaml::Value::String("true".to_string()),
        );
        let string_spec = crate::queue::QueueExecutionSpec {
            cmd: "update_general_lastup".to_string(),
            args: Vec::new(),
            meta: string_meta,
        };
        assert!(execution_spec_meta_bool(&string_spec, "update_modified"));
        assert!(!execution_spec_meta_bool(&string_spec, "missing"));
        assert_eq!(
            execution_spec_meta_string(&string_spec, "update_modified").as_deref(),
            Some("true")
        );
    }

    #[test]
    fn execution_spec_meta_strings_accepts_string_sequences() {
        let mut meta = serde_yaml::Mapping::new();
        meta.insert(
            serde_yaml::Value::String("snapshot_ids".to_string()),
            serde_yaml::Value::Sequence(vec![
                serde_yaml::Value::String("12".to_string()),
                serde_yaml::Value::Number(serde_yaml::Number::from(34)),
            ]),
        );
        let spec = crate::queue::QueueExecutionSpec {
            cmd: "update_by_tag".to_string(),
            args: Vec::new(),
            meta,
        };

        assert_eq!(execution_spec_meta_strings(&spec, "snapshot_ids"), vec!["12", "34"]);
        assert!(execution_spec_meta_strings(&spec, "missing").is_empty());
    }

    #[test]
    fn update_by_tag_args_use_snapshot_ids_like_selected_update() {
        let mut meta = serde_yaml::Mapping::new();
        meta.insert(
            serde_yaml::Value::String("snapshot_ids".to_string()),
            serde_yaml::Value::Sequence(vec![
                serde_yaml::Value::String("42".to_string()),
                serde_yaml::Value::String("9".to_string()),
            ]),
        );
        meta.insert(
            serde_yaml::Value::String("sort_by".to_string()),
            serde_yaml::Value::String("general_lastup".to_string()),
        );
        let spec = crate::queue::QueueExecutionSpec {
            cmd: "update_by_tag".to_string(),
            args: vec!["tag:modified".to_string()],
            meta,
        };

        assert_eq!(update_by_tag_update_args(&spec), vec!["42", "9"]);
    }

    #[test]
    fn update_by_tag_args_fall_back_to_tag_selector_for_legacy_queue() {
        let mut meta = serde_yaml::Mapping::new();
        meta.insert(
            serde_yaml::Value::String("sort_by".to_string()),
            serde_yaml::Value::String("general_lastup".to_string()),
        );
        let spec = crate::queue::QueueExecutionSpec {
            cmd: "update_by_tag".to_string(),
            args: vec!["tag:modified".to_string()],
            meta,
        };

        assert_eq!(
            update_by_tag_update_args(&spec),
            vec!["--sort-by", "general_lastup", "tag:modified"]
        );
    }
}

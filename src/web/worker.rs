use std::collections::{HashMap, VecDeque};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_yaml::Value;
use tokio::task::JoinHandle;

use super::push::PushServer;
use crate::compat::{
    configure_web_subprocess_command, load_local_setting_bool, load_local_setting_string,
};
use crate::db::with_database_mut;
use crate::progress::{WEB_PROGRESS_SCOPE_ENV, WS_LINE_PREFIX};
use crate::queue::{JobType, PersistentQueue, QueueExecutionSpec, QueueJob, QueueLane};

const WEBUI_UPDATE_START_PREFIX: &str = "__webui_update_start__=";
const MAX_FAILURE_DETAIL_LINES: usize = 8;
const MAX_FAILURE_DETAIL_CHARS: usize = 600;

#[derive(Debug, Clone, Default)]
struct JobRunResult {
    success: bool,
    detail: Option<String>,
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
    concurrency_enabled: bool,
) -> Vec<JoinHandle<()>> {
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
    lane: WorkerLane,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let Some(job) = pop_next_job(queue.as_ref(), lane) else {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            };

            register_running_job(&running_jobs, &job);
            push_server.broadcast_event("queue_start", &job.id);

            let root_dir = root_dir.clone();
            let job_for_run = job.clone();
            let ps = Arc::clone(&push_server);
            let pid_ref = Arc::clone(&running_child_pids);
            let queue_for_run = Arc::clone(&queue);
            let result = tokio::task::spawn_blocking(move || {
                execute_job(
                    &root_dir,
                    queue_for_run.as_ref(),
                    &job_for_run,
                    &ps,
                    &pid_ref,
                )
            })
            .await
            .unwrap_or_default();

            // Refresh in-memory database from disk (subprocess may have modified it)
            if let Err(e) = with_database_mut(|db| db.refresh()) {
                push_server.broadcast_error(&format!("DB更新エラー: {}", e));
            }

            unregister_running_job(&running_jobs, &job.id);
            if result.success {
                let _ = queue.complete(&job.id);
                push_server.broadcast_event("queue_complete", &job.id);
            } else {
                let _ = queue.fail(&job.id);
                let mut data = serde_json::json!({ "job_id": job.id });
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
            clear_progress_for_job(&push_server, &job.id);
            if should_reload_table_after_job(queue.as_ref(), &running_jobs) {
                push_server.broadcast_event("table.reload", "");
                push_server.broadcast_event("tag.updateCanvas", "");
            }
            push_server.broadcast_event("notification.queue", "");
        }
    })
}

fn pop_next_job(queue: &PersistentQueue, lane: WorkerLane) -> Option<QueueJob> {
    match lane {
        WorkerLane::All => queue.pop(),
        WorkerLane::Default => queue.pop_for_lane(QueueLane::Default),
        WorkerLane::Secondary => queue.pop_for_lane(QueueLane::Secondary),
    }
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
) -> JobRunResult {
    if matches!(job.job_type, JobType::AutoUpdate) {
        return JobRunResult {
            success: crate::web::scheduler::execute_auto_update(
            root_dir,
            Arc::clone(push_server),
            &job.id,
            Arc::clone(running_pids),
            ),
            detail: None,
        };
    }

    let target_console = console_target_for_job(job.job_type);
    let Ok(exe) = std::env::current_exe() else {
        push_server.broadcast_echo("エラー: 実行ファイルパスを取得できません", target_console);
        return JobRunResult {
            success: false,
            detail: Some("エラー: 実行ファイルパスを取得できません".to_string()),
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
            "update" | "update_by_tag" => {
                command.arg("update");
                append_update_args(&mut command, push_server, target_console, &spec.args);
            }
            "convert" => {
                let convert_target = spec.args.join("\t");
                let (target, device) = parse_convert_job_target(&convert_target);
                command.arg("convert").arg("--no-open").arg(target);
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
                        success: false,
                        detail: Some("diff task has no arguments".to_string()),
                    };
                }
                return execute_diff_job(
                    root_dir,
                    &exe,
                    &spec.args,
                    push_server,
                    running_pids,
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
                    success: false,
                    detail: Some(format!("未対応の復元キューコマンドです: {}", unsupported)),
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
                    let parts: Vec<String> = job
                        .target
                        .split('\t')
                        .map(|part| part.to_string())
                        .collect();
                    append_update_args(&mut command, push_server, target_console, &parts);
                }
            }
            JobType::Convert => {
                let (target, device) = parse_convert_job_target(&job.target);
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
    spawn_and_stream_command(command, push_server, running_pids, &job.id, target_console)
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
    job_id: &str,
    target_console: &str,
) -> JobRunResult {
    let mut command = new_web_subprocess_command(exe, root_dir, job_id);
    command.arg("update").arg("--gl");
    append_update_args(&mut command, push_server, target_console, &spec.args);
    let result = spawn_and_stream_command(command, push_server, running_pids, job_id, target_console);
    if !result.success || !execution_spec_meta_bool(spec, "update_modified") {
        return result;
    }

    refresh_web_state(push_server);
    push_server.broadcast_echo(
        "<span style=\"color:#d7ba7d\">modified タグの付いた小説を更新します</span>",
        target_console,
    );

    let mut followup = new_web_subprocess_command(exe, root_dir, job_id);
    followup.arg("update");
    if let Some(sort_key) = execution_spec_meta_string(spec, "sort_by") {
        followup.arg("--sort-by").arg(sort_key);
    }
    followup.arg("tag:modified");
    spawn_and_stream_command(followup, push_server, running_pids, job_id, target_console)
}

fn append_update_args(
    command: &mut std::process::Command,
    push_server: &Arc<PushServer>,
    target_console: &str,
    args: &[String],
) {
    if let Some((first, rest)) = args.split_first() {
        if let Some(message) = first.strip_prefix(WEBUI_UPDATE_START_PREFIX) {
            push_server.broadcast_echo(
                &format!("<span style=\"color:#bbb\">{}</span>", message),
                target_console,
            );
        } else if !first.is_empty() {
            command.arg(first);
        }
        for part in rest {
            if !part.is_empty() {
                command.arg(part);
            }
        }
    }
}

fn execute_diff_job(
    root_dir: &Path,
    exe: &Path,
    args: &[String],
    push_server: &Arc<PushServer>,
    running_pids: &Arc<parking_lot::Mutex<HashMap<String, u32>>>,
    job_id: &str,
    target_console: &str,
) -> JobRunResult {
    if args.len() < 2 {
        push_server.broadcast_echo("diff task is missing the diff number", target_console);
        return JobRunResult {
            success: false,
            detail: Some("diff task is missing the diff number".to_string()),
        };
    }
    let (ids, number) = args.split_at(args.len() - 1);
    let Some(number) = number.first() else {
        push_server.broadcast_echo("diff task is missing the diff number", target_console);
        return JobRunResult {
            success: false,
            detail: Some("diff task is missing the diff number".to_string()),
        };
    };
    let mut success = true;
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
        let result =
            spawn_and_stream_command(command, push_server, running_pids, job_id, target_console);
        if !result.success {
            success = false;
            if let Some(detail) = result.detail {
                details.push(detail);
            }
        }
    }
    JobRunResult {
        success,
        detail: summarize_failure_details(&details),
    }
}

fn spawn_and_stream_command(
    mut command: std::process::Command,
    push_server: &Arc<PushServer>,
    running_pids: &Arc<parking_lot::Mutex<HashMap<String, u32>>>,
    job_id: &str,
    target_console: &str,
) -> JobRunResult {
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            push_server.broadcast_echo(&format!("プロセス起動失敗: {}", e), target_console);
            return JobRunResult {
                success: false,
                detail: Some(format!("プロセス起動失敗: {}", e)),
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

    let status = child.wait().map(|s| s.success()).unwrap_or(false);
    running_pids.lock().remove(job_id);
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
    JobRunResult {
        success: status,
        detail: (!status).then(|| summarize_failure_output(recent_output.as_ref())).flatten(),
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

fn console_target_for_job(job_type: JobType) -> &'static str {
    match job_type {
        JobType::Download | JobType::Update | JobType::AutoUpdate => "stdout",
        JobType::Convert | JobType::Send | JobType::Backup | JobType::Mail => "stdout2",
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

fn parse_convert_job_target(value: &str) -> (&str, Option<&str>) {
    let mut parts = value.splitn(2, '\t');
    let target = parts.next().unwrap_or(value);
    let device = parts.next().filter(|device| !device.is_empty());
    (target, device)
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_FAILURE_DETAIL_CHARS, MAX_FAILURE_DETAIL_LINES, clear_progress_for_job,
        console_target_for_job, execution_spec_meta_bool, execution_spec_meta_string,
        parse_convert_job_target, remember_failure_line, route_structured_web_message,
        summarize_failure_details,
    };
    use crate::queue::JobType;

    #[test]
    fn parse_convert_job_target_splits_device_override() {
        assert_eq!(parse_convert_job_target("1\tkindle"), ("1", Some("kindle")));
        assert_eq!(parse_convert_job_target("1"), ("1", None));
    }

    #[test]
    fn console_target_splits_external_site_jobs_from_local_jobs() {
        assert_eq!(console_target_for_job(JobType::Download), "stdout");
        assert_eq!(console_target_for_job(JobType::Update), "stdout");
        assert_eq!(console_target_for_job(JobType::AutoUpdate), "stdout");
        assert_eq!(console_target_for_job(JobType::Convert), "stdout2");
        assert_eq!(console_target_for_job(JobType::Send), "stdout2");
        assert_eq!(console_target_for_job(JobType::Backup), "stdout2");
        assert_eq!(console_target_for_job(JobType::Mail), "stdout2");
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
}

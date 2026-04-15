use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::db::with_database_mut;
use crate::progress::WS_LINE_PREFIX;
use crate::queue::{JobType, QueueJob};

use super::jobs::open_queue;
use super::push::PushServer;

pub fn start_queue_worker(
    root_dir: PathBuf,
    push_server: Arc<PushServer>,
    running_job: Arc<parking_lot::Mutex<Option<QueueJob>>>,
    running_child_pid: Arc<parking_lot::Mutex<Option<u32>>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let queue = match open_queue() {
                Ok(queue) => queue,
                Err(message) => {
                    push_server.broadcast_error(&message);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };

            let Some(job) = queue.pop() else {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            };

            *running_job.lock() = Some(job.clone());
            push_server.broadcast_event("queue_start", &job.id);

            let root_dir = root_dir.clone();
            let job_for_run = job.clone();
            let ps = Arc::clone(&push_server);
            let pid_ref = Arc::clone(&running_child_pid);
            let success = tokio::task::spawn_blocking(move || execute_job(&root_dir, &job_for_run, &ps, &pid_ref))
                .await
                .unwrap_or(false);

            // Refresh in-memory database from disk (subprocess may have modified it)
            if let Err(e) = with_database_mut(|db| db.refresh()) {
                push_server.broadcast_error(&format!("DB更新エラー: {}", e));
            }

            *running_job.lock() = None;
            if success {
                let _ = queue.complete(&job.id);
                push_server.broadcast_event("queue_complete", &job.id);
            } else {
                let _ = queue.fail(&job.id);
                push_server.broadcast_event("queue_failed", &job.id);
            }
            // Trigger frontend table reload after DB refresh
            push_server.broadcast_event("table.reload", "");
            push_server.broadcast_event("tag.updateCanvas", "");
            push_server.broadcast_event("notification.queue", "");
        }
    })
}

fn execute_job(root_dir: &Path, job: &QueueJob, push_server: &Arc<PushServer>, running_pid: &Arc<parking_lot::Mutex<Option<u32>>>) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        push_server.broadcast_echo("エラー: 実行ファイルパスを取得できません", "stdout");
        return false;
    };

    let mut command = std::process::Command::new(exe);
    command
        .current_dir(root_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NAROU_RS_WEB_MODE", "1");

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
                for part in job.target.split('\t') {
                    if !part.is_empty() {
                        command.arg(part);
                    }
                }
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
            command.arg("send").arg("--mail");
            for part in job.target.split('\t') {
                if !part.is_empty() {
                    command.arg(part);
                }
            }
        }
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            push_server.broadcast_echo(&format!("プロセス起動失敗: {}", e), "stdout");
            return false;
        }
    };

    // Store child PID for external cancellation
    *running_pid.lock() = Some(child.id());

    // Stream stdout in a separate thread
    let stdout = child.stdout.take();
    let ps_out = Arc::clone(push_server);
    let stdout_thread = std::thread::spawn(move || {
        if let Some(out) = stdout {
            let reader = std::io::BufReader::new(out);
            for line in reader.lines() {
                match line {
                    Ok(text) => {
                        if let Some(json_str) = text.strip_prefix(WS_LINE_PREFIX) {
                            // Structured WS event from child process — send directly
                            if let Ok(msg) = serde_json::from_str::<serde_json::Value>(json_str) {
                                ps_out.broadcast_raw(&msg);
                            }
                        } else {
                            ps_out.broadcast_echo(&text, "stdout");
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    });

    // Stream stderr in a separate thread
    let stderr = child.stderr.take();
    let ps_err = Arc::clone(push_server);
    let stderr_thread = std::thread::spawn(move || {
        if let Some(err) = stderr {
            let reader = std::io::BufReader::new(err);
            for line in reader.lines() {
                match line {
                    Ok(text) => ps_err.broadcast_echo(&text, "stdout"),
                    Err(_) => break,
                }
            }
        }
    });

    let status = child.wait().map(|s| s.success()).unwrap_or(false);
    *running_pid.lock() = None;
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
    status
}

fn parse_convert_job_target(value: &str) -> (&str, Option<&str>) {
    let mut parts = value.splitn(2, '\t');
    let target = parts.next().unwrap_or(value);
    let device = parts.next().filter(|device| !device.is_empty());
    (target, device)
}

#[cfg(test)]
mod tests {
    use super::parse_convert_job_target;

    #[test]
    fn parse_convert_job_target_splits_device_override() {
        assert_eq!(parse_convert_job_target("1\tkindle"), ("1", Some("kindle")));
        assert_eq!(parse_convert_job_target("1"), ("1", None));
    }
}

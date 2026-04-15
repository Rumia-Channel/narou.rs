use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::queue::{JobType, QueueJob};

use super::jobs::open_queue;
use super::push::PushServer;

pub fn start_queue_worker(
    root_dir: PathBuf,
    push_server: Arc<PushServer>,
    running_job: Arc<parking_lot::Mutex<Option<QueueJob>>>,
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
            push_server.broadcast("queue_start", &job.id);
            let root_dir = root_dir.clone();
            let job_for_run = job.clone();
            let success = tokio::task::spawn_blocking(move || execute_job(&root_dir, &job_for_run))
                .await
                .unwrap_or(false);

            *running_job.lock() = None;
            if success {
                let _ = queue.complete(&job.id);
                push_server.broadcast("queue_complete", &job.id);
            } else {
                let _ = queue.fail(&job.id);
                push_server.broadcast("queue_failed", &job.id);
            }
        }
    })
}

fn execute_job(root_dir: &Path, job: &QueueJob) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };

    let mut command = std::process::Command::new(exe);
    command
        .current_dir(root_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

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

    command
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
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

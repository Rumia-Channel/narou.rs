use axum::{extract::State, response::Json};

use crate::db::with_database;
use crate::queue::{JobType, PersistentQueue};

use super::AppState;
use super::state::{
    ApiResponse, ConvertBody, CsvImportBody, DiffBody, DiffCleanBody, DownloadBody, ReorderBody,
    TargetsBody, TaskIdBody, UpdateBody,
};

pub async fn api_download(
    State(state): State<AppState>,
    Json(body): Json<DownloadBody>,
) -> Json<serde_json::Value> {
    let targets = body.targets;
    let queue = match open_queue() {
        Ok(queue) => queue,
        Err(message) => {
            return serde_json::json!({
                "success": false,
                "message": message,
                "results": []
            })
            .into();
        }
    };

    let job_type = if body.mail {
        JobType::Mail
    } else {
        JobType::Download
    };

    let jobs: Vec<(JobType, String)> = targets
        .iter()
        .map(|target| {
            if body.force {
                (job_type, format!("--force\t{}", target))
            } else {
                (job_type, target.clone())
            }
        })
        .collect();
    let ids = match queue.push_batch(&jobs) {
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

    let results: Vec<serde_json::Value> = targets
        .iter()
        .zip(ids.iter())
        .map(|(target, job_id)| {
            state.push_server.broadcast("download_queued", target);
            serde_json::json!({ "target": target, "job_id": job_id, "status": "queued" })
        })
        .collect();

    serde_json::json!({ "success": true, "results": results }).into()
}

pub async fn api_update(
    State(state): State<AppState>,
    Json(body): Json<UpdateBody>,
) -> Json<serde_json::Value> {
    let targets = targets_to_strings(&body.targets);

    let queue = match open_queue() {
        Ok(queue) => queue,
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

    let jobs: Vec<(JobType, String)> = if has_flags {
        // CLI-style args: join as single job target
        let combined = targets.join("\t");
        vec![(JobType::Update, combined)]
    } else if targets.is_empty() {
        // Update all
        let ids = with_database(|db| Ok(db.ids())).unwrap_or_default();
        if body.force {
            ids.iter()
                .map(|id| (JobType::Update, format!("--force\t{}", id)))
                .collect()
        } else {
            ids.iter()
                .map(|id| (JobType::Update, id.to_string()))
                .collect()
        }
    } else if body.force {
        targets
            .iter()
            .map(|t| (JobType::Update, format!("--force\t{}", t)))
            .collect()
    } else {
        targets
            .iter()
            .map(|t| (JobType::Update, t.clone()))
            .collect()
    };

    let count = jobs.len();
    let job_ids = match queue.push_batch(&jobs) {
        Ok(ids) => ids,
        Err(e) => {
            return serde_json::json!({
                "success": false,
                "message": e.to_string(),
                "count": 0
            })
            .into();
        }
    };

    state
        .push_server
        .broadcast("update_queued", &format!("{} novels", count));
    serde_json::json!({
        "success": true,
        "status": "queued",
        "count": count,
        "job_ids": job_ids
    })
    .into()
}

pub async fn api_convert(
    State(state): State<AppState>,
    Json(body): Json<ConvertBody>,
) -> Json<serde_json::Value> {
    let device = body.device.unwrap_or_else(|| "text".to_string());
    let queue = match open_queue() {
        Ok(queue) => queue,
        Err(message) => {
            return serde_json::json!({
                "success": false,
                "message": message,
                "results": []
            })
            .into();
        }
    };
    let jobs: Vec<(JobType, String)> = body
        .targets
        .iter()
        .map(|target| (JobType::Convert, format!("{}\t{}", target, device)))
        .collect();
    let ids = match queue.push_batch(&jobs) {
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
        .map(|(target, job_id)| {
            state.push_server.broadcast("convert_queued", target);
            serde_json::json!({
                "target": target,
                "device": device,
                "job_id": job_id,
                "status": "queued"
            })
        })
        .collect();

    serde_json::json!({ "success": true, "results": results }).into()
}

pub async fn queue_status(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let queue = match open_queue() {
        Ok(queue) => queue,
        Err(message) => {
            return Json(serde_json::json!({
                "pending": 0,
                "completed": 0,
                "failed": 0,
                "error": message,
            }));
        }
    };

    Json(serde_json::json!({
        "pending": queue.pending_count(),
        "completed": queue.completed_count(),
        "failed": queue.failed_count(),
    }))
}

pub async fn queue_clear(State(_state): State<AppState>) -> Json<ApiResponse> {
    let result = open_queue().and_then(|q| q.clear().map_err(|e| e.to_string()));

    match result {
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

pub(crate) fn open_queue() -> Result<PersistentQueue, String> {
    PersistentQueue::with_default()
        .or_else(|_| {
            let path = std::path::PathBuf::from(".narou").join("queue.yaml");
            PersistentQueue::new(&path)
        })
        .map_err(|e| e.to_string())
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
    let targets = targets_to_strings(&body.targets);
    let queue = match open_queue() {
        Ok(queue) => queue,
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
    let ids = match queue.push_batch(&jobs) {
        Ok(ids) => ids,
        Err(e) => {
            return serde_json::json!({
                "success": false,
                "message": e.to_string(),
            })
            .into();
        }
    };

    for target in &targets {
        state.push_server.broadcast("send_queued", target);
    }
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
    let targets = targets_to_strings(&body.targets);
    let mut args: Vec<&str> = vec!["inspect"];
    let target_refs: Vec<&str> = targets.iter().map(|s| s.as_str()).collect();
    args.extend(&target_refs);

    state
        .push_server
        .broadcast("inspect_start", &targets.join(", "));

    match run_immediate(&args) {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.is_empty() {
                state.push_server.broadcast("inspect", &stdout);
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
    let targets = targets_to_strings(&body.targets);
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
    let targets = targets_to_strings(&body.targets);
    let queue = match open_queue() {
        Ok(queue) => queue,
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
    let ids = match queue.push_batch(&jobs) {
        Ok(ids) => ids,
        Err(e) => {
            return serde_json::json!({
                "success": false,
                "message": e.to_string(),
            })
            .into();
        }
    };

    for target in &targets {
        state.push_server.broadcast("backup_queued", target);
    }
    serde_json::json!({
        "success": true,
        "count": ids.len(),
        "job_ids": ids,
    })
    .into()
}

// POST /api/mail
pub async fn api_mail(
    State(state): State<AppState>,
    Json(body): Json<TargetsBody>,
) -> Json<serde_json::Value> {
    let targets = targets_to_strings(&body.targets);
    let queue = match open_queue() {
        Ok(queue) => queue,
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
    let ids = match queue.push_batch(&jobs) {
        Ok(ids) => ids,
        Err(e) => {
            return serde_json::json!({
                "success": false,
                "message": e.to_string(),
            })
            .into();
        }
    };

    for target in &targets {
        state.push_server.broadcast("mail_queued", target);
    }
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
    let targets = targets_to_strings(&body.targets);
    let mut args: Vec<&str> = vec!["setting", "--burn"];
    let target_refs: Vec<&str> = targets.iter().map(|s| s.as_str()).collect();
    args.extend(&target_refs);

    state
        .push_server
        .broadcast("setting_burn_start", &targets.join(", "));

    match run_immediate(&args) {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.is_empty() {
                state.push_server.broadcast("setting_burn", &stdout);
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

        let novel_dir =
            crate::db::existing_novel_dir_for_record(&archive_root, &record);
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

// POST /api/diff
pub async fn api_diff(
    State(state): State<AppState>,
    Json(body): Json<DiffBody>,
) -> Json<ApiResponse> {
    let ids: Vec<String> = body
        .ids
        .iter()
        .map(|v| match v {
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect();

    for id in &ids {
        let args = vec!["diff", "--no-tool", id, "--number", &body.number];
        match run_immediate(&args) {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if !stdout.is_empty() {
                    state.push_server.broadcast("console", &stdout);
                }
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stderr.is_empty() {
                    state.push_server.broadcast("console", &stderr);
                }
            }
            Err(e) => {
                state
                    .push_server
                    .broadcast("console", &format!("diff error: {}", e));
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

    let args = vec!["diff", "--clean", &target];
    match run_immediate(&args) {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.is_empty() {
                state.push_server.broadcast("console", &stdout);
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
                state.push_server.broadcast("csv_import", &stdout);
            }
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
    if let Ok(queue) = open_queue() {
        let _ = queue.clear_pending();
    }

    state.push_server.broadcast_event("notification.queue", "");
    Json(ApiResponse {
        success: true,
        message: "キャンセルしました".to_string(),
    })
}

// POST /api/cancel_running_task — cancel specific running task
pub async fn cancel_running_task(
    State(state): State<AppState>,
    Json(body): Json<TaskIdBody>,
) -> Json<serde_json::Value> {
    let running = state.running_job.lock().clone();
    if let Some(job) = running {
        if job.id == body.task_id {
            kill_running_child(&state);
            return serde_json::json!({ "status": "ok" }).into();
        }
    }
    serde_json::json!({ "error": "実行中の処理を中断できませんでした" }).into()
}

fn kill_running_child(state: &AppState) {
    let pid = state.running_child_pid.lock().take();
    if let Some(pid) = pid {
        // Kill process tree on Windows using taskkill /T
        // On Unix, fall back to plain kill via std::process::Command
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
            state.push_server.broadcast_echo(
                &format!("プロセス終了に失敗: {}", e),
                "stdout",
            );
        }
        state.push_server.broadcast_echo("--- ジョブをキャンセルしました ---", "stdout");
    }
}

// GET /api/get_pending_tasks
pub async fn get_pending_tasks(
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    let queue = match open_queue() {
        Ok(q) => q,
        Err(e) => {
            return Json(serde_json::json!({
                "pending": [],
                "running": [],
                "pending_count": 0,
                "running_count": 0,
                "error": e,
            }));
        }
    };

    let pending = queue.get_pending_tasks();
    let pending_count = pending.len();
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

    let running_guard = state.running_job.lock();
    let (running_json, running_count) = if let Some(job) = running_guard.as_ref() {
        (vec![serde_json::json!({
            "id": job.id,
            "type": job.job_type,
            "target": job.target,
            "created_at": job.created_at,
        })], 1)
    } else {
        (vec![], 0)
    };
    drop(running_guard);

    Json(serde_json::json!({
        "pending": pending_json,
        "running": running_json,
        "pending_count": pending_count,
        "running_count": running_count,
    }))
}

// POST /api/remove_pending_task
pub async fn remove_pending_task(
    State(_state): State<AppState>,
    Json(body): Json<TaskIdBody>,
) -> Json<ApiResponse> {
    let queue = match open_queue() {
        Ok(q) => q,
        Err(e) => {
            return Json(ApiResponse {
                success: false,
                message: e,
            });
        }
    };

    match queue.remove_pending(&body.task_id) {
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
    State(_state): State<AppState>,
    Json(body): Json<ReorderBody>,
) -> Json<ApiResponse> {
    let queue = match open_queue() {
        Ok(q) => q,
        Err(e) => {
            return Json(ApiResponse {
                success: false,
                message: e,
            });
        }
    };

    match queue.reorder_pending(&body.task_ids) {
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
    State(_state): State<AppState>,
) -> Json<serde_json::Value> {
    let (default_count, convert_count) = match open_queue() {
        Ok(q) => {
            let tasks = q.get_pending_tasks();
            let convert = tasks.iter().filter(|t| matches!(t.job_type, JobType::Convert)).count();
            let default = tasks.len() - convert;
            (default, convert)
        }
        Err(_) => (0, 0),
    };

    Json(serde_json::json!({
        "default": default_count,
        "convert": convert_count,
        "total": default_count + convert_count,
    }))
}

// POST /api/shutdown
pub async fn api_shutdown(
    State(state): State<AppState>,
    Json(_body): Json<serde_json::Value>,
) -> Json<ApiResponse> {
    state.push_server.broadcast("shutdown", "Server shutting down");
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
    state.push_server.broadcast("reboot", "Server rebooting");
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            return Json(ApiResponse {
                success: false,
                message: e.to_string(),
            });
        }
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
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

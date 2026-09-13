//! ライブラリ全体バックアップの Web API。
//!
//! `GET /api/library_backup` は 0.4.0 未満からのアップデート後初回起動で
//! バックアップ提案が必要かを返す。`POST /api/library_backup` は
//! `narou_rs_backup` サブプロセスで zip を作成し、進捗をコンソールへ流す。

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;

use super::AppState;
use crate::startup_backup;

/// ライブラリバックアップの実行状態。同時実行は1つまで。
pub struct LibraryBackupState {
    /// バックアップ実行中フラグ。
    running: AtomicBool,
}

impl LibraryBackupState {
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
        }
    }
}

#[derive(Deserialize)]
pub struct LibraryBackupAction {
    /// "create" でバックアップ作成、"dismiss" で提案を閉じる。
    action: String,
    /// 任意の保存先フォルダ。省略時は exe 横の backup/。
    output: Option<String>,
}

/// GET /api/library_backup — 提案が必要かと容量情報を返す。
pub async fn api_library_backup_status(
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    let pending = evaluate_pending(&state);
    match pending {
        Some(offer) => serde_json::json!({
            "pending": true,
            "total_bytes": offer.total_bytes,
            "free_bytes": offer.free_bytes,
            "enough_space": offer.enough_space,
            "default_output": startup_backup::default_backup_dir()
                .map(|p| p.display().to_string()),
            "running": state.library_backup.running.load(Ordering::Acquire),
        })
        .into(),
        None => serde_json::json!({
            "pending": false,
            "running": state.library_backup.running.load(Ordering::Acquire),
        })
        .into(),
    }
}

/// POST /api/library_backup — バックアップ作成または提案の却下。
pub async fn api_library_backup(
    State(state): State<AppState>,
    Json(body): Json<LibraryBackupAction>,
) -> impl IntoResponse {
    let Some(root) = crate::logger::find_narou_root() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"success": false, "message": "library root not found"})),
        );
    };

    match body.action.as_str() {
        "dismiss" => {
            startup_backup::record_run(&root);
            (
                StatusCode::OK,
                Json(serde_json::json!({"success": true, "dismissed": true})),
            )
        }
        "create" => {
            // 既に他経路 (CLI 等) で提案済みなら冪等に成功を返す。
            // marker を直接見て判定する (キャッシュは持たない)。
            if startup_backup::pending_offer(&root).is_none() {
                return (
                    StatusCode::OK,
                    Json(serde_json::json!({"success": true, "already_done": true})),
                );
            }
            if state.library_backup.running.swap(true, Ordering::AcqRel) {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({"success": false, "message": "backup already running"})),
                );
            }

            let output = body
                .output
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .or_else(startup_backup::default_backup_dir);
            let Some(output) = output else {
                state.library_backup.running.store(false, Ordering::Release);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        serde_json::json!({"success": false, "message": "backup destination unavailable"}),
                    ),
                );
            };

            // 回答があった時点で marker を記録する (成否に関わらず再提案しない)。
            startup_backup::record_run(&root);

            spawn_backup_process(&state, root, output);
            (
                StatusCode::OK,
                Json(serde_json::json!({"success": true, "started": true})),
            )
        }
        _ => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"success": false, "message": "unknown action"})),
        ),
    }
}

/// 提案状態を評価する。毎回 marker と容量を見直す。
fn evaluate_pending(state: &AppState) -> Option<startup_backup::PendingOffer> {
    let _ = state;
    crate::logger::find_narou_root().and_then(|root| startup_backup::pending_offer(&root))
}

/// `narou_rs_backup` をサブプロセスで起動し、出力をコンソールへ流す。
/// exe が見つからない場合はプロセス内で実行するフォールバック。
fn spawn_backup_process(state: &AppState, root: PathBuf, output: PathBuf) {
    let push_server = state.push_server.clone();
    let backup_state = state.library_backup.clone();
    let target_console = crate::native::non_external_console_target();

    std::thread::spawn(move || {
        let result = run_backup(&push_server, &root, &output, target_console);
        backup_state.running.store(false, Ordering::Release);
        match result {
            Ok(path) => {
                push_server.broadcast_event(
                    "library_backup.done",
                    &serde_json::json!({ "path": path.display().to_string() }).to_string(),
                );
            }
            Err(message) => {
                push_server.broadcast_event(
                    "library_backup.failed",
                    &serde_json::json!({ "message": message }).to_string(),
                );
            }
        }
    });
}

fn run_backup(
    push_server: &Arc<super::push::PushServer>,
    root: &Path,
    output: &Path,
    target_console: &str,
) -> Result<PathBuf, String> {
    if let Some(exe) = startup_backup::backup_exe_path() {
        run_backup_subprocess(push_server, &exe, root, output, target_console)
    } else {
        push_server.broadcast_echo(
            "narou_rs_backup が見つからないためプロセス内でバックアップします",
            target_console,
        );
        startup_backup::create_library_backup(root, output).map_err(|e| e.to_string())
    }
}

fn run_backup_subprocess(
    push_server: &Arc<super::push::PushServer>,
    exe: &Path,
    root: &Path,
    output: &Path,
    target_console: &str,
) -> Result<PathBuf, String> {
    let mut command = Command::new(exe);
    command
        .arg("--root")
        .arg(root)
        .arg("--output")
        .arg(output)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|e| format!("バックアッププロセスの起動に失敗: {}", e))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let ps_out = push_server.clone();
    let console = target_console.to_string();
    let stdout_thread = std::thread::spawn(move || {
        if let Some(out) = stdout {
            for line in BufReader::new(out).lines() {
                if let Ok(text) = line {
                    ps_out.broadcast_echo(&text, &console);
                }
            }
        }
    });
    let ps_err = push_server.clone();
    let console = target_console.to_string();
    let stderr_thread = std::thread::spawn(move || {
        if let Some(err) = stderr {
            for line in BufReader::new(err).lines() {
                if let Ok(text) = line {
                    ps_err.broadcast_echo(&text, &console);
                }
            }
        }
    });

    let status = child.wait().map_err(|e| e.to_string())?;
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();

    if status.success() {
        // 出力ファイル名は narou-backup-<timestamp>.zip。最新を拾う。
        latest_backup_zip(output)
            .ok_or_else(|| "バックアップファイルが見つかりません".to_string())
    } else {
        Err(format!(
            "バックアッププロセスが異常終了しました: {}",
            status
        ))
    }
}

fn latest_backup_zip(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("narou-backup-") && n.ends_with(".zip"))
        })
        .max_by_key(|path| {
            path.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        })
}

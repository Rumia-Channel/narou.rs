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

impl Default for LibraryBackupState {
    fn default() -> Self {
        Self::new()
    }
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
        Ok(Some(offer)) => serde_json::json!({
            "pending": true,
            "total_bytes": offer.total_bytes,
            "free_bytes": offer.free_bytes,
            "enough_space": offer.enough_space,
            "default_output": startup_backup::default_backup_dir()
                .map(|p| p.display().to_string()),
            "running": state.library_backup.running.load(Ordering::Acquire),
        })
        .into(),
        Ok(None) => serde_json::json!({
            "pending": false,
            "running": state.library_backup.running.load(Ordering::Acquire),
        })
        .into(),
        Err(error) => serde_json::json!({
            "pending": false,
            "error": error.to_string(),
            "running": state.library_backup.running.load(Ordering::Acquire),
        }).into(),
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
            match startup_backup::backup_offer_needed(&root) {
                Ok(false) => return (
                    StatusCode::OK,
                    Json(serde_json::json!({"success": true, "already_done": true})),
                ),
                Err(error) => return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"success": false, "message": error.to_string()})),
                ),
                Ok(true) => {},
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
fn evaluate_pending(_state: &AppState) -> crate::error::Result<Option<startup_backup::PendingOffer>> {
    match crate::logger::find_narou_root() {
        Some(root) => startup_backup::pending_offer(&root),
        None => Ok(None),
    }
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
                push_server.broadcast_raw(crate::application::push_events::library_backup_done(
                    &path.display().to_string(),
                ));
            }
            Err(message) => {
                push_server.broadcast_raw(crate::application::push_events::library_backup_failed(&message));
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
    let output = if output.is_absolute() { output.to_path_buf() } else { root.join(output) };
    if let Some(exe) = startup_backup::backup_exe_path() {
        run_backup_subprocess(push_server, &exe, root, &output, target_console)
    } else {
        push_server.broadcast_echo(
            "narou_rs_backup が見つからないためプロセス内でバックアップします",
            target_console,
        );
        startup_backup::create_library_backup(root, &output).map_err(|e| e.to_string())
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
        .current_dir(root)
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
        let out = stdout.ok_or_else(|| "バックアッププロセスの標準出力がありません".to_string())?;
        read_backup_completion(BufReader::new(out), |text| ps_out.broadcast_echo(text, &console))
    });
    let ps_err = push_server.clone();
    let console = target_console.to_string();
    let stderr_thread = std::thread::spawn(move || -> Result<(), String> {
        let err = stderr.ok_or_else(|| "バックアッププロセスの標準エラー出力がありません".to_string())?;
        for line in BufReader::new(err).lines() {
            let text = line.map_err(|error| format!("バックアップ出力の読み取りに失敗: {error}"))?;
            ps_err.broadcast_echo(&text, &console);
        }
        Ok(())
    });

    let status = child.wait().map_err(|e| e.to_string())?;
    let reported = stdout_thread.join().map_err(|_| "バックアップ標準出力スレッドが異常終了しました".to_string())?;
    let errors = stderr_thread.join().map_err(|_| "バックアップ標準エラースレッドが異常終了しました".to_string())?;

    if status.success() {
        errors?;
        let path = reported?;
        let actual = path.canonicalize().map_err(|error| format!("バックアップ完了ファイルを開けません: {error}"))?;
        let expected = output.canonicalize().map_err(|error| error.to_string())?;
        if actual.parent() != Some(expected.as_path()) || !actual.is_file() {
            return Err("バックアップ完了パスが指定した保存先と一致しません".into());
        }
        Ok(actual)
    } else {
        Err(format!(
            "バックアッププロセスが異常終了しました: {}",
            status
        ))
    }
}

fn read_backup_completion(reader: impl BufRead, mut echo: impl FnMut(&str)) -> Result<PathBuf, String> {
    let mut completed = None;
    for line in reader.lines() {
        let line = line.map_err(|error| format!("バックアップ出力の読み取りに失敗: {error}"))?;
        if let Some(json) = line.strip_prefix(startup_backup::BACKUP_RESULT_PREFIX) {
            if completed.is_some() { return Err("バックアップ完了パスが複数報告されました".into()); }
            let path: PathBuf = serde_json::from_str(json).map_err(|error| format!("バックアップ完了パスが不正です: {error}"))?;
            if !path.is_absolute() { return Err("バックアップ完了パスが絶対パスではありません".into()); }
            completed = Some(path);
        } else {
            echo(&line);
        }
    }
    completed.ok_or_else(|| "バックアッププロセスから完了パスが報告されませんでした".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_uses_exact_reported_path_not_a_preexisting_latest_zip() {
        let output = tempfile::tempdir().unwrap();
        let old = output.path().join("narou-backup-newer-but-unrelated.zip");
        std::fs::write(&old, b"old backup").unwrap();
        let exact = output.path().join("narou-backup-this-run.zip");
        let line = format!("作成しました\n{}{}\n", startup_backup::BACKUP_RESULT_PREFIX, serde_json::to_string(&exact).unwrap());
        let mut echoed = Vec::new();
        assert_eq!(read_backup_completion(line.as_bytes(), |line| echoed.push(line.to_owned())).unwrap(), exact);
        assert_eq!(echoed, ["作成しました"]);
        assert!(read_backup_completion("作成しました\n".as_bytes(), |_| {}).is_err());
    }

    #[test]
    fn malformed_duplicate_and_truncated_completion_fail_closed() {
        assert!(read_backup_completion(format!("{}not-json\n", startup_backup::BACKUP_RESULT_PREFIX).as_bytes(), |_| {}).is_err());
        let path = tempfile::tempdir().unwrap();
        let marker = format!("{}{}\n", startup_backup::BACKUP_RESULT_PREFIX, serde_json::to_string(path.path()).unwrap());
        assert!(read_backup_completion(format!("{marker}{marker}").as_bytes(), |_| {}).is_err());
        let reader = std::io::BufReader::new(FailingRead);
        assert!(read_backup_completion(reader, |_| {}).is_err());
    }

    struct FailingRead;
    impl std::io::Read for FailingRead {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "lost process output"))
        }
    }
}

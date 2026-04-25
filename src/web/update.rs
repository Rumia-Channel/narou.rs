//! `/api/update/start` — WEB UI からのアプリ自動アップデート。
//!
//! 同梱の `narou_rs_updater(.exe)` に処理を引き継ぎ、本体プロセスを終了する。
//! 失敗ケースはレスポンス JSON の `success=false` と `progressbar.clear` で
//! クライアントへ通知する。

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use reqwest::header::USER_AGENT;
use serde::Deserialize;

use super::AppState;
use super::jobs::prepare_process_shutdown;
use super::state::ApiResponse;
use crate::version;

const PROGRESS_TOPIC: &str = "update";

#[derive(Debug, Deserialize, Default)]
pub struct UpdateStartBody {
    /// 任意: 指定すれば独自のアセット URL を使用 (デバッグ用)。
    #[serde(default)]
    pub asset_url: Option<String>,
}

pub async fn api_update_start(
    State(state): State<AppState>,
    body: Option<Json<UpdateStartBody>>,
) -> Result<Json<ApiResponse>, (StatusCode, Json<ApiResponse>)> {
    let body = body.map(|Json(b)| b).unwrap_or_default();

    if !version::commit_version_exists() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiResponse {
                success: false,
                message: "develop ビルドではアップデートできません".to_string(),
            }),
        ));
    }

    let install_dir = match resolve_install_dir() {
        Ok(d) => d,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse {
                    success: false,
                    message: format!("install dir 取得失敗: {e}"),
                }),
            ));
        }
    };
    let updater_path = updater_binary_path(&install_dir);
    if !updater_path.exists() {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiResponse {
                success: false,
                message: format!(
                    "updater が見つかりません: {} — 手動でリリース zip を取得してください",
                    updater_path.display()
                ),
            }),
        ));
    }

    let asset_name = match current_asset_name() {
        Some(name) => name,
        None => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiResponse {
                    success: false,
                    message: format!(
                        "未対応のプラットフォーム ({}/{}) — 手動でアップデートしてください",
                        std::env::consts::OS,
                        std::env::consts::ARCH
                    ),
                }),
            ));
        }
    };

    state
        .push_server
        .broadcast_progressbar_init(PROGRESS_TOPIC);
    state
        .push_server
        .broadcast_echo("アップデート: 最新リリース情報を取得しています...", "stdout");

    let asset_url = match body.asset_url {
        Some(url) if !url.is_empty() => url,
        _ => match fetch_asset_url(&asset_name).await {
            Ok(url) => url,
            Err(e) => {
                state
                    .push_server
                    .broadcast_progressbar_clear(PROGRESS_TOPIC);
                return Err((
                    StatusCode::BAD_GATEWAY,
                    Json(ApiResponse {
                        success: false,
                        message: format!("リリース取得失敗: {e}"),
                    }),
                ));
            }
        },
    };

    state
        .push_server
        .broadcast_echo(&format!("アップデート: ダウンロード中 ({asset_name})"), "stdout");

    let zip_path = install_dir.join(format!("update_download_{asset_name}.tmp"));
    let download_state = state.clone();
    if let Err(e) = download_to_file(&asset_url, &zip_path, &download_state).await {
        state
            .push_server
            .broadcast_progressbar_clear(PROGRESS_TOPIC);
        let _ = std::fs::remove_file(&zip_path);
        return Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiResponse {
                success: false,
                message: format!("ダウンロード失敗: {e}"),
            }),
        ));
    }
    state
        .push_server
        .broadcast_progressbar_clear(PROGRESS_TOPIC);

    if let Err(e) = validate_zip(&zip_path) {
        let _ = std::fs::remove_file(&zip_path);
        return Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiResponse {
                success: false,
                message: format!("zip 検証失敗: {e}"),
            }),
        ));
    }

    let pid = std::process::id();
    let exe_path = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse {
                    success: false,
                    message: format!("current_exe 取得失敗: {e}"),
                }),
            ));
        }
    };
    let exe_name = exe_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "narou_rs.exe".to_string()
            } else {
                "narou_rs".to_string()
            }
        });
    let hide_console = crate::compat::inherited_hide_console_requested();
    let restart_args = build_restart_args(&exe_name, hide_console);

    let mut cmd = std::process::Command::new(&updater_path);
    cmd.arg("--pid")
        .arg(pid.to_string())
        .arg("--zip")
        .arg(&zip_path)
        .arg("--install-dir")
        .arg(&install_dir)
        .arg("--log")
        .arg(install_dir.join("update.log"))
        .arg("--restart");
    for a in &restart_args {
        cmd.arg(a);
    }
    cmd.current_dir(&install_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::compat::configure_hidden_console_command(&mut cmd);
    crate::compat::configure_process_group_command(&mut cmd);

    state
        .push_server
        .broadcast_echo("アップデート: 適用処理を開始します。本体を再起動します...", "stdout");

    if let Err(e) = cmd.spawn() {
        let _ = std::fs::remove_file(&zip_path);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiResponse {
                success: false,
                message: format!("updater spawn 失敗: {e}"),
            }),
        ));
    }

    state.push_server.broadcast_event("reboot", "");
    let shutdown_state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        prepare_process_shutdown(&shutdown_state).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::process::exit(0);
    });

    Ok(Json(ApiResponse {
        success: true,
        message: "アップデートを開始しました".to_string(),
    }))
}

fn resolve_install_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "no parent".to_string())
}

fn updater_binary_path(install_dir: &Path) -> PathBuf {
    let name = if cfg!(windows) {
        "narou_rs_updater.exe"
    } else {
        "narou_rs_updater"
    };
    install_dir.join(name)
}

/// 起動時の引数から再起動時のコマンドを組み立てる (`reboot_args_with_no_browser`
/// の自前版。jobs.rs と同じ方針: `--no-browser` と必要なら `--hide-console` を追加)。
fn build_restart_args(exe_name: &str, hide_console: bool) -> Vec<String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if !args.iter().any(|a| a == "-n" || a == "--no-browser") {
        args.push("--no-browser".to_string());
    }
    if hide_console && !args.iter().any(|a| a == "--hide-console") {
        args.push("--hide-console".to_string());
    }
    let mut full = vec![exe_name.to_string()];
    full.extend(args);
    full
}

async fn fetch_asset_url(asset_name: &str) -> Result<String, String> {
    let url = "https://api.github.com/repos/Rumia-Channel/narou.rs/releases/latest";
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(url)
        .header(USER_AGENT, "narou.rs")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("GitHub API status {}", resp.status()));
    }
    let text = resp.text().await.map_err(|e| e.to_string())?;
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    pick_asset_url(&json, asset_name).ok_or_else(|| {
        format!(
            "アセット {asset_name} が見つかりません (タグ {})",
            json["tag_name"].as_str().unwrap_or("?")
        )
    })
}

fn pick_asset_url(release_json: &serde_json::Value, asset_name: &str) -> Option<String> {
    let assets = release_json["assets"].as_array()?;
    for asset in assets {
        let name = asset["name"].as_str().unwrap_or("");
        if name == asset_name {
            return asset["browser_download_url"]
                .as_str()
                .map(|s| s.to_string());
        }
    }
    None
}

async fn download_to_file(
    url: &str,
    dest: &Path,
    state: &AppState,
) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60 * 30))
        .build()
        .map_err(|e| e.to_string())?;
    let mut resp = client
        .get(url)
        .header(USER_AGENT, "narou.rs")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP status {}", resp.status()));
    }
    let total = resp.content_length();
    let mut file = tokio::fs::File::create(dest).await.map_err(|e| e.to_string())?;
    let mut downloaded: u64 = 0;
    let mut last_percent: i64 = -1;
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        downloaded += chunk.len() as u64;
        if let Some(total) = total {
            if total > 0 {
                let percent = ((downloaded as f64 / total as f64) * 100.0) as i64;
                if percent != last_percent {
                    state
                        .push_server
                        .broadcast_progressbar_step(percent as f64, PROGRESS_TOPIC);
                    last_percent = percent;
                }
            }
        }
    }
    file.flush().await.map_err(|e| e.to_string())?;
    Ok(())
}

fn validate_zip(path: &Path) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open: {e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("zip: {e}"))?;
    let exe_name_with_ext = if cfg!(windows) {
        "narou/narou_rs.exe"
    } else {
        "narou/narou_rs"
    };
    let updater_with_ext = if cfg!(windows) {
        "narou/narou_rs_updater.exe.new"
    } else {
        "narou/narou_rs_updater.new"
    };
    let mut has_exe = false;
    let mut has_updater = false;
    for i in 0..archive.len() {
        let entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let n = entry.name();
        if n == exe_name_with_ext {
            has_exe = true;
        }
        if n == updater_with_ext {
            has_updater = true;
        }
    }
    if !has_exe {
        return Err(format!("{exe_name_with_ext} が含まれていません"));
    }
    if !has_updater {
        return Err(format!(
            "{updater_with_ext} が含まれていません — このリリースは自動更新非対応です"
        ));
    }
    Ok(())
}

/// 現在のプラットフォーム/アーキ向けのリリースアセット名を返す。
/// release.yml の matrix と一致させる。armv7/v6 は best-effort 判定。
pub fn current_asset_name() -> Option<String> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let key = match (os, arch) {
        ("windows", "x86_64") => "win_x64",
        ("windows", "aarch64") => "win_arm64",
        ("macos", "x86_64") => "mac_x64",
        ("macos", "aarch64") => "mac_arm64",
        ("linux", "x86_64") => "linux_x64",
        ("linux", "aarch64") => "linux_arm64",
        ("linux", "arm") => "linux_armv6",
        ("linux", "armv7") => "linux_armv7",
        _ => return None,
    };
    Some(format!("narou_rs_{key}.zip"))
}

// IntoResponse for the (StatusCode, Json<ApiResponse>) tuple in error path
// is provided automatically by axum.
#[allow(dead_code)]
fn _ensure_response_impl(_: (StatusCode, Json<ApiResponse>)) -> axum::response::Response {
    (StatusCode::OK, Json(ApiResponse { success: true, message: String::new() })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_restart_args_adds_no_browser() {
        // Note: tests can't fully control std::env::args() — verify the
        // suffix logic by manipulating the produced vector.
        let args = build_restart_args("narou_rs", false);
        assert!(args.first().is_some());
        // Suffix must include --no-browser
        assert!(args.iter().any(|a| a == "--no-browser"));
    }

    #[test]
    fn current_asset_name_known_combinations() {
        // We can only assert the function returns the expected value for the
        // current host (other combinations are constructed via match arms in
        // production code and need no separate test once exercised).
        let name = current_asset_name();
        if cfg!(target_os = "windows") && cfg!(target_arch = "x86_64") {
            assert_eq!(name.as_deref(), Some("narou_rs_win_x64.zip"));
        }
        if cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") {
            assert_eq!(name.as_deref(), Some("narou_rs_linux_x64.zip"));
        }
    }

    #[test]
    fn pick_asset_url_returns_browser_download_url() {
        let payload = serde_json::json!({
            "tag_name": "v0.1.31",
            "assets": [
                {
                    "name": "other.zip",
                    "browser_download_url": "https://example.com/other.zip"
                },
                {
                    "name": "narou_rs_linux_x64.zip",
                    "browser_download_url": "https://example.com/wanted.zip"
                }
            ]
        });
        assert_eq!(
            pick_asset_url(&payload, "narou_rs_linux_x64.zip"),
            Some("https://example.com/wanted.zip".to_string())
        );
        assert_eq!(pick_asset_url(&payload, "missing.zip"), None);
    }

    #[test]
    fn validate_zip_requires_main_and_updater() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("rel.zip");
        let file = std::fs::File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default();
        let main_name = if cfg!(windows) { "narou/narou_rs.exe" } else { "narou/narou_rs" };
        let upd_name = if cfg!(windows) { "narou/narou_rs_updater.exe.new" } else { "narou/narou_rs_updater.new" };
        writer.start_file(main_name, opts).unwrap();
        writer.write_all(b"main").unwrap();
        writer.start_file(upd_name, opts).unwrap();
        writer.write_all(b"updater").unwrap();
        writer.finish().unwrap();
        assert!(validate_zip(&zip_path).is_ok());
    }

    #[test]
    fn validate_zip_fails_when_updater_missing() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("rel.zip");
        let file = std::fs::File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default();
        let main_name = if cfg!(windows) { "narou/narou_rs.exe" } else { "narou/narou_rs" };
        writer.start_file(main_name, opts).unwrap();
        writer.write_all(b"main").unwrap();
        writer.finish().unwrap();
        assert!(validate_zip(&zip_path).is_err());
    }

    #[test]
    fn validate_zip_fails_when_old_style_updater_present() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("rel.zip");
        let file = std::fs::File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default();
        let main_name = if cfg!(windows) { "narou/narou_rs.exe" } else { "narou/narou_rs" };
        let old_upd = if cfg!(windows) { "narou/narou_rs_updater.exe" } else { "narou/narou_rs_updater" };
        writer.start_file(main_name, opts).unwrap();
        writer.write_all(b"main").unwrap();
        writer.start_file(old_upd, opts).unwrap();
        writer.write_all(b"updater").unwrap();
        writer.finish().unwrap();
        assert!(validate_zip(&zip_path).is_err());
    }
}

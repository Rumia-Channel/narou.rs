use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::USER_AGENT;
use serde_json::Value;

use crate::application::{
    ApplicationEvent, EventSink, SelfUpdateRequest, SelfUpdateResult, SelfUpdateService,
};
use crate::error::{NarouError, Result};
use crate::platform::PlatformFuture;

const PROGRESS_TOPIC: &str = "update";

#[derive(Debug, Clone, Copy, Default)]
pub struct NativeSelfUpdateService;

impl SelfUpdateService for NativeSelfUpdateService {
    fn start<'a>(
        &'a self,
        request: SelfUpdateRequest,
        events: Arc<dyn EventSink>,
    ) -> PlatformFuture<'a, Result<SelfUpdateResult>> {
        Box::pin(async move { start_native_update(request, events).await })
    }
}

async fn start_native_update(
    request: SelfUpdateRequest,
    events: Arc<dyn EventSink>,
) -> Result<SelfUpdateResult> {
    if let Some(reason) = crate::version::self_update_unavailable_reason() {
        return Err(platform_error(reason));
    }

    let install_dir = resolve_install_dir().map_err(platform_error)?;
    let updater_path = updater_binary_path(&install_dir);
    if !updater_path.exists() {
        return Err(platform_error(format!(
            "updater が見つかりません: {} — 手動でリリース zip を取得してください",
            updater_path.display()
        )));
    }

    let asset_name = current_asset_name().ok_or_else(|| {
        platform_error(format!(
            "未対応のプラットフォーム ({}/{}) — 手動でアップデートしてください",
            std::env::consts::OS,
            std::env::consts::ARCH
        ))
    })?;

    emit(
        &events,
        "progressbar.init",
        serde_json::json!({ "topic": PROGRESS_TOPIC }),
    )
    .await;
    emit(
        &events,
        "echo",
        serde_json::json!({
            "body": "アップデート: 最新リリース情報を取得しています...",
            "target_console": "stdout"
        }),
    )
    .await;

    let asset_url = match request.asset_url.filter(|url| !url.is_empty()) {
        Some(url) => url,
        None => match fetch_asset_url(&asset_name).await {
            Ok(url) => url,
            Err(error) => {
                emit(
                    &events,
                    "progressbar.clear",
                    serde_json::json!({ "topic": PROGRESS_TOPIC }),
                )
                .await;
                return Err(platform_error(format!("リリース取得失敗: {error}")));
            }
        },
    };

    emit(
        &events,
        "echo",
        serde_json::json!({
            "body": format!("アップデート: ダウンロード中 ({asset_name})"),
            "target_console": "stdout"
        }),
    )
    .await;

    let zip_path = install_dir.join(format!("update_download_{asset_name}.tmp"));
    if let Err(error) = download_to_file(&asset_url, &zip_path, &events).await {
        emit(
            &events,
            "progressbar.clear",
            serde_json::json!({ "topic": PROGRESS_TOPIC }),
        )
        .await;
        let _ = tokio::fs::remove_file(&zip_path).await;
        return Err(platform_error(format!("ダウンロード失敗: {error}")));
    }
    emit(
        &events,
        "progressbar.clear",
        serde_json::json!({ "topic": PROGRESS_TOPIC }),
    )
    .await;

    let validation_path = zip_path.clone();
    let validation_result = tokio::task::spawn_blocking(move || validate_zip(&validation_path))
        .await
        .map_err(|error| platform_error(format!("zip validation task failed: {error}")))?;
    if let Err(error) = validation_result {
        let _ = tokio::fs::remove_file(&zip_path).await;
        return Err(platform_error(format!("zip 検証失敗: {error}")));
    }

    let pid = std::process::id();
    let exe_path = std::env::current_exe().map_err(|error| platform_error(format!("current_exe 取得失敗: {error}")))?;
    let exe_name = exe_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "narou_rs.exe".to_string()
            } else {
                "narou_rs".to_string()
            }
        });
    let restart_args = build_restart_args(&exe_name, crate::compat::inherited_hide_console_requested());

    let mut command = std::process::Command::new(&updater_path);
    command
        .arg("--pid")
        .arg(pid.to_string())
        .arg("--zip")
        .arg(&zip_path)
        .arg("--install-dir")
        .arg(&install_dir)
        .arg("--log")
        .arg(install_dir.join("update.log"))
        .arg("--restart")
        .args(&restart_args)
        .current_dir(&install_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::compat::configure_hidden_console_command(&mut command);
    crate::compat::configure_process_group_command(&mut command);

    append_update_session_marker(
        &install_dir.join("update.log"),
        &format!(
            "session: parent pid={} spawning updater (Unix detach via setsid; SIGHUP relayed only if signal is explicitly sent to updater pid)",
            pid
        ),
    );

    emit(
        &events,
        "echo",
        serde_json::json!({
            "body": "アップデート: 適用処理を開始します。本体を再起動します...",
            "target_console": "stdout"
        }),
    )
    .await;

    command
        .spawn()
        .map_err(|error| platform_error(format!("updater spawn 失敗: {error}")))?;
    emit(&events, "reboot", Value::String(String::new())).await;

    Ok(SelfUpdateResult { asset_name })
}

async fn emit(events: &Arc<dyn EventSink>, name: &str, data: Value) {
    let _ = events.publish(ApplicationEvent::new(name, data)).await;
}

fn platform_error(message: impl Into<String>) -> NarouError {
    NarouError::Platform(message.into())
}

fn resolve_install_dir() -> std::result::Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
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

fn append_update_session_marker(path: &Path, message: &str) {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::time::SystemTime;

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let line = format!("[{}.{:03}] {message}\n", now.as_secs(), now.subsec_millis());
    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(mut file) => {
            let _ = file.write_all(line.as_bytes());
            let _ = file.flush();
        }
        Err(error) => eprintln!("update.log append failed: {error}"),
    }
}

fn build_restart_args(exe_name: &str, hide_console: bool) -> Vec<String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if !args.iter().any(|arg| arg == "-n" || arg == "--no-browser") {
        args.push("--no-browser".to_string());
    }
    if hide_console && !args.iter().any(|arg| arg == "--hide-console") {
        args.push("--hide-console".to_string());
    }
    let mut full = vec![exe_name.to_string()];
    full.extend(args);
    full
}

async fn fetch_asset_url(asset_name: &str) -> std::result::Result<String, String> {
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| error.to_string())?
        .get("https://api.github.com/repos/Rumia-Channel/narou.rs/releases/latest")
        .header(USER_AGENT, "narou.rs")
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("GitHub API status {}", response.status()));
    }
    let json: Value = serde_json::from_str(
        &response.text().await.map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    pick_asset_url(&json, asset_name).ok_or_else(|| {
        format!(
            "アセット {asset_name} が見つかりません (タグ {})",
            json["tag_name"].as_str().unwrap_or("?")
        )
    })
}

fn pick_asset_url(release_json: &Value, asset_name: &str) -> Option<String> {
    release_json["assets"].as_array()?.iter().find_map(|asset| {
        (asset["name"].as_str() == Some(asset_name))
            .then(|| asset["browser_download_url"].as_str().map(str::to_string))
            .flatten()
    })
}

async fn download_to_file(
    url: &str,
    dest: &Path,
    events: &Arc<dyn EventSink>,
) -> std::result::Result<(), String> {
    use tokio::io::AsyncWriteExt;

    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(60 * 30))
        .build()
        .map_err(|error| error.to_string())?
        .get(url)
        .header(USER_AGENT, "narou.rs")
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP status {}", response.status()));
    }
    let total = response.content_length();
    let mut response = response;
    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|error| error.to_string())?;
    let mut downloaded = 0u64;
    let mut last_percent = -1i64;
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        file.write_all(&chunk).await.map_err(|error| error.to_string())?;
        downloaded += chunk.len() as u64;
        if let Some(total) = total.filter(|total| *total > 0) {
            let percent = (downloaded.saturating_mul(100) / total) as i64;
            if percent != last_percent {
                emit(
                    events,
                    "progressbar.step",
                    serde_json::json!({
                        "percent": percent as f64,
                        "topic": PROGRESS_TOPIC,
                        "target_console": "stdout"
                    }),
                )
                .await;
                last_percent = percent;
            }
        }
    }
    file.flush().await.map_err(|error| error.to_string())?;
    Ok(())
}

fn validate_zip(path: &Path) -> std::result::Result<(), String> {
    let file = std::fs::File::open(path).map_err(|error| format!("open: {error}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|error| format!("zip: {error}"))?;
    let exe_name = if cfg!(windows) {
        "narou/narou_rs.exe"
    } else {
        "narou/narou_rs"
    };
    let updater_name = if cfg!(windows) {
        "narou/narou_rs_updater.exe.new"
    } else {
        "narou/narou_rs_updater.new"
    };
    let mut has_exe = false;
    let mut has_updater = false;
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(|error| error.to_string())?;
        has_exe |= entry.name() == exe_name;
        has_updater |= entry.name() == updater_name;
    }
    if !has_exe {
        return Err(format!("{exe_name} が含まれていません"));
    }
    if !has_updater {
        return Err(format!(
            "{updater_name} が含まれていません — このリリースは自動更新非対応です"
        ));
    }
    Ok(())
}

pub fn current_asset_name() -> Option<String> {
    let key = match (std::env::consts::OS, std::env::consts::ARCH) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_asset_name_matches_host() {
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
            "assets": [
                { "name": "other.zip", "browser_download_url": "https://example.com/other.zip" },
                { "name": "wanted.zip", "browser_download_url": "https://example.com/wanted.zip" }
            ]
        });
        assert_eq!(pick_asset_url(&payload, "wanted.zip"), Some("https://example.com/wanted.zip".to_string()));
        assert_eq!(pick_asset_url(&payload, "missing.zip"), None);
    }
}

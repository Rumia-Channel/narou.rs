use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::USER_AGENT;
use serde_json::Value;

use crate::application::{
    ApplicationEvent, EventSink, SelfUpdateRequest, SelfUpdateResult, SelfUpdateService,
    SelfUpdateVariant,
};
use crate::db::inventory::{Inventory, InventoryScope};
use crate::error::{NarouError, Result};
use crate::platform::PlatformFuture;

const PROGRESS_TOPIC: &str = "update";

/// `global_setting.yaml` key that persists the user's self-update variant
/// choice (`gpl` / `standard`). Settable via `narou setting`.
pub const VARIANT_SETTING_KEY: &str = "self-update.variant";

/// Version boundary for the one-time variant prompt: builds at or below this
/// version predate the GPL/standard split, so the update flow asks the user
/// which variant to install. Later builds reuse the saved choice.
const VARIANT_PROMPT_MAX_VERSION: &str = "0.4.0";

/// The variant of the running binary.
pub fn build_variant() -> SelfUpdateVariant {
    if crate::version::EMBEDS_AOZORA_LITE {
        SelfUpdateVariant::Gpl
    } else {
        SelfUpdateVariant::Standard
    }
}

/// Variant preference saved in `global_setting.yaml`, if any.
pub fn saved_variant_preference() -> Option<SelfUpdateVariant> {
    let inventory = Inventory::with_default_root().ok()?;
    let settings: std::collections::HashMap<String, serde_yaml::Value> = inventory
        .load("global_setting", InventoryScope::Global)
        .unwrap_or_default();
    settings
        .get(VARIANT_SETTING_KEY)
        .and_then(|v| v.as_str())
        .and_then(SelfUpdateVariant::from_str_lossy)
}

/// Persist an explicit variant choice so later updates reuse it.
fn persist_variant_preference(variant: SelfUpdateVariant) -> Result<()> {
    let inventory = Inventory::with_default_root()
        .map_err(|e| platform_error(format!("global_setting へのアクセス失敗: {e}")))?;
    inventory
        .update_yaml::<(), std::collections::HashMap<String, serde_yaml::Value>, _>(
            "global_setting",
            InventoryScope::Global,
            |mut settings| {
                settings.insert(
                    VARIANT_SETTING_KEY.to_string(),
                    serde_yaml::Value::String(variant.as_str().to_string()),
                );
                Ok((settings, ()))
            },
        )
        .map_err(|e| platform_error(format!("{VARIANT_SETTING_KEY} の保存失敗: {e}")))
}

/// Whether the update flow must ask the user which variant to install.
/// True only for builds at or below `VARIANT_PROMPT_MAX_VERSION` — the first
/// release line that predates the GPL/standard asset split.
pub fn variant_choice_required() -> bool {
    crate::version::version_at_most(
        &crate::version::create_version_string(),
        VARIANT_PROMPT_MAX_VERSION,
    )
}

/// Effective variant for this update: explicit request > saved preference >
/// the running build's own variant.
fn resolve_variant(request: &SelfUpdateRequest) -> SelfUpdateVariant {
    request
        .variant
        .or_else(saved_variant_preference)
        .unwrap_or_else(build_variant)
}

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

    let variant = resolve_variant(&request);
    if let Some(chosen) = request.variant {
        // 明示的な選択は以後の更新でも再利用するよう保存する。
        persist_variant_preference(chosen)?;
    }

    let asset_name = asset_name_for(variant).ok_or_else(|| {
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
    #[cfg(target_os = "linux")]
    let systemd_service = current_systemd_service();
    #[cfg(not(target_os = "linux"))]
    let systemd_service: Option<()> = None;

    append_update_session_marker(
        &install_dir.join("update.log"),
        &format!(
            "session: parent pid={} handoff={} (setsid alone cannot escape a systemd service cgroup)",
            pid,
            if systemd_service.is_some() { "systemd-run" } else { "detached updater" }
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

    #[cfg(target_os = "linux")]
    if let Some(service) = systemd_service {
        // systemd's default KillMode=control-group kills ordinary descendants
        // when the Web process exits, even if the updater called setsid().
        // A transient *separate service* survives that shutdown. Run the
        // already installed updater without --restart (supported by old
        // releases), then ask systemd to restart the original service. This
        // also replaces any old process started by Restart=always.
        let mut command = systemd_update_command(
            &service,
            &updater_path,
            &zip_path,
            &install_dir,
            pid,
        );
        let output = tokio::process::Command::from(command)
            .output()
            .await
            .map_err(|error| platform_error(format!(
                "systemd-run 起動失敗: {error}。サービスの更新権限を確認してください"
            )))?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr);
            let _ = tokio::fs::remove_file(&zip_path).await;
            return Err(platform_error(format!(
                "systemd-run によるアップデートの引き渡し失敗: {}。systemd の権限と journalctl を確認してください",
                detail.trim()
            )));
        }
    } else {
        spawn_detached_updater(&updater_path, &zip_path, &install_dir, pid, &restart_args)?;
    }
    #[cfg(not(target_os = "linux"))]
    spawn_detached_updater(&updater_path, &zip_path, &install_dir, pid, &restart_args)?;

    emit(&events, "reboot", Value::String(String::new())).await;

    Ok(SelfUpdateResult { asset_name })
}

fn spawn_detached_updater(
    updater_path: &Path,
    zip_path: &Path,
    install_dir: &Path,
    pid: u32,
    restart_args: &[String],
) -> Result<()> {
    let mut command = std::process::Command::new(updater_path);
    command
        .arg("--pid")
        .arg(pid.to_string())
        .arg("--zip")
        .arg(zip_path)
        .arg("--install-dir")
        .arg(install_dir)
        .arg("--log")
        .arg(install_dir.join("update.log"))
        .arg("--restart")
        .args(restart_args)
        .current_dir(install_dir)
        // Old updaters also pass this environment variable to the new binary.
        .env(
            "NAROU_RS_RESTART_CWD",
            std::env::current_dir().unwrap_or_else(|_| install_dir.to_path_buf()),
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::compat::configure_hidden_console_command(&mut command);
    crate::compat::configure_process_group_command(&mut command);
    command
        .spawn()
        .map_err(|error| platform_error(format!("updater spawn 失敗: {error}")))?;
    Ok(())
}

#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq, Eq)]
struct SystemdService {
    unit: String,
    user: bool,
}

#[cfg(target_os = "linux")]
fn systemd_service_from_cgroup(contents: &str) -> Option<SystemdService> {
    // cgroup v2: 0::/system.slice/narou.service
    // cgroup v1: 1:name=systemd:/system.slice/narou.service
    // User units also contain user@UID.service: choose the innermost unit.
    for line in contents.lines() {
        let path = line.splitn(3, ':').nth(2)?;
        let components: Vec<&str> = path.split('/').collect();
        let Some(unit) = components
            .iter()
            .rev()
            .find(|part| part.ends_with(".service") && !part.starts_with("user@"))
        else {
            continue;
        };
        return Some(SystemdService {
            unit: (*unit).to_string(),
            user: components
                .iter()
                .any(|part| part.starts_with("user@") && part.ends_with(".service")),
        });
    }
    None
}

#[cfg(target_os = "linux")]
fn current_systemd_service() -> Option<SystemdService> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    systemd_service_from_cgroup(&cgroup)
}

#[cfg(target_os = "linux")]
fn systemd_update_command(
    service: &SystemdService,
    updater_path: &Path,
    zip_path: &Path,
    install_dir: &Path,
    pid: u32,
) -> std::process::Command {
    let mut command = std::process::Command::new("systemd-run");
    if service.user {
        command.arg("--user");
    }
    // Use /bin/sh solely to sequence two argv-safe commands, avoiding a new
    // flag unsupported by the updater shipped with existing release ZIPs.
    // $0 is the unit, $1 is the updater binary, and "$@" forwards its args.
    command
        .arg("--unit")
        .arg(format!("narou-rs-update-{pid}"))
        .arg("--collect")
        .arg("--property=Type=exec")
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg(if service.user {
            r#""$@" && systemctl --user restart "$0""#
        } else {
            r#""$@" && systemctl restart "$0""#
        })
        .arg(&service.unit)
        .arg(updater_path)
        .arg("--pid")
        .arg(pid.to_string())
        .arg("--zip")
        .arg(zip_path)
        .arg("--install-dir")
        .arg(install_dir)
        .arg("--log")
        .arg(install_dir.join("update.log"))
        .current_dir(install_dir);
    command
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

/// Release asset name for the given variant on this platform.
/// GPL builds download `narou_rs_<key>-GPL.zip`; standard builds use the
/// unsuffixed name.
pub fn asset_name_for(variant: SelfUpdateVariant) -> Option<String> {
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
    let suffix = match variant {
        SelfUpdateVariant::Gpl => "-GPL",
        SelfUpdateVariant::Standard => "",
    };
    Some(format!("narou_rs_{key}{suffix}.zip"))
}

/// Asset name matching this build's own variant (used when no explicit or
/// saved choice exists).
pub fn current_asset_name() -> Option<String> {
    asset_name_for(build_variant())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn systemd_cgroup_identifies_innermost_service_and_manager() {
        assert_eq!(
            systemd_service_from_cgroup("0::/system.slice/narou.service\n"),
            Some(SystemdService {
                unit: "narou.service".to_string(),
                user: false,
            })
        );
        assert_eq!(
            systemd_service_from_cgroup(
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/narou.service\n"
            ),
            Some(SystemdService {
                unit: "narou.service".to_string(),
                user: true,
            })
        );
        assert_eq!(
            systemd_service_from_cgroup("0::/user.slice/user-1000.slice/session-1.scope\n"),
            None
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn systemd_handoff_uses_transient_unit_and_legacy_updater_arguments() {
        let service = SystemdService {
            unit: "narou.service".to_string(),
            user: false,
        };
        let command = systemd_update_command(
            &service,
            Path::new("/opt/narou/narou_rs_updater"),
            Path::new("/opt/narou/update.zip"),
            Path::new("/opt/narou"),
            1234,
        );
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"--unit".to_string()));
        assert!(args.contains(&"narou-rs-update-1234".to_string()));
        assert!(args.contains(&"--property=Type=exec".to_string()));
        assert!(args.contains(&r#""$@" && systemctl restart "$0""#.to_string()));
        assert!(!args.contains(&"--restart".to_string()));
        assert_eq!(args.last().unwrap(), "/opt/narou/update.log");
    }

    #[test]
    fn asset_name_for_appends_gpl_suffix() {
        if cfg!(target_os = "windows") && cfg!(target_arch = "x86_64") {
            assert_eq!(
                asset_name_for(SelfUpdateVariant::Standard).as_deref(),
                Some("narou_rs_win_x64.zip")
            );
            assert_eq!(
                asset_name_for(SelfUpdateVariant::Gpl).as_deref(),
                Some("narou_rs_win_x64-GPL.zip")
            );
        }
        if cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") {
            assert_eq!(
                asset_name_for(SelfUpdateVariant::Standard).as_deref(),
                Some("narou_rs_linux_x64.zip")
            );
            assert_eq!(
                asset_name_for(SelfUpdateVariant::Gpl).as_deref(),
                Some("narou_rs_linux_x64-GPL.zip")
            );
        }
    }

    #[test]
    fn current_asset_name_matches_build_variant() {
        let expected = asset_name_for(build_variant());
        assert_eq!(current_asset_name(), expected);
    }

    #[test]
    fn variant_round_trips_through_str() {
        assert_eq!(
            SelfUpdateVariant::from_str_lossy("GPL"),
            Some(SelfUpdateVariant::Gpl)
        );
        assert_eq!(
            SelfUpdateVariant::from_str_lossy(" standard "),
            Some(SelfUpdateVariant::Standard)
        );
        assert_eq!(SelfUpdateVariant::from_str_lossy("bogus"), None);
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

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::compat::configure_hidden_console_command;
use crate::db::inventory::Inventory;
use crate::setting_core::SettingScope;

pub const NAME: &str = "narou.rs";
pub const VERSION: &str = match option_env!("NAROU_RS_VERSION_OVERRIDE") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

pub fn create_version_string() -> String {
    if is_local_build() {
        format!("{} (local-build)", VERSION)
    } else if commit_version_exists() {
        VERSION.to_string()
    } else {
        format!("{} (develop)", VERSION)
    }
}

pub fn version_json() -> serde_json::Value {
    serde_json::json!({
        "version": create_version_string(),
        "name": NAME,
        "develop": !commit_version_exists(),
        "local_build": is_local_build(),
        "container": is_container_runtime(),
        "self_update_supported": self_update_unavailable_reason().is_none(),
        "build_variant": BUILD_VARIANT,
    })
}

pub fn runtime_description() -> String {
    let mut command = Command::new("rustc");
    command.arg("--version");
    configure_hidden_console_command(&mut command);
    if let Ok(output) = command.output() {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !text.is_empty() {
                return text;
            }
        }
    }

    format!(
        "Rust {} ({}/{})",
        VERSION,
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

pub fn aozoraepub3_jar_path() -> Option<PathBuf> {
    if let Some(path) = aozoraepub3_jar_from_global_setting() {
        return Some(path);
    }

    if let Some(path) = aozoraepub3_jar_from_narou_root() {
        return Some(path);
    }

    aozoraepub3_jar_next_to_exe()
}

pub const IS_RELEASE_BUILD: bool = option_env!("NAROU_RS_RELEASE_BUILD").is_some();
pub const IS_LOCAL_BUILD: bool = option_env!("NAROU_RS_LOCAL_BUILD").is_some();

/// `true` when this binary embeds AozoraEpub3_Lite (the `lite` feature).
/// Such builds are distributed as the GPL variant (`narou_rs_*-GPL.zip`).
pub const EMBEDS_AOZORA_LITE: bool = cfg!(feature = "lite");



/// Self-update asset variant this binary belongs to: `"gpl"` or `"standard"`.
pub const BUILD_VARIANT: &str = if EMBEDS_AOZORA_LITE { "gpl" } else { "standard" };



const DISABLE_SELF_UPDATE_ENV: &str = "NAROU_RS_DISABLE_SELF_UPDATE";

/// バージョン比較は Worker と共有する（`src/application/version_compare.rs`）。
pub use crate::application::version_compare::{version_at_most, version_compare, version_core};

pub fn commit_version_exists() -> bool {
    if IS_RELEASE_BUILD {
        return true;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let Some(dir) = exe.parent() else {
        return false;
    };
    dir.join("commitversion").exists()
}

pub fn is_local_build() -> bool {
    IS_LOCAL_BUILD || is_source_checkout_build()
}

fn is_source_checkout_build() -> bool {
    std::env::current_exe()
        .ok()
        .is_some_and(|exe| is_cargo_target_executable(&exe))
}

fn is_cargo_target_executable(exe: &Path) -> bool {
    exe.ancestors().any(|ancestor| {
        ancestor.file_name().is_some_and(|name| name == "target")
            && ancestor
                .parent()
                .is_some_and(|root| root.join("Cargo.toml").is_file())
    })
}

pub fn is_container_runtime() -> bool {
    if std::env::var_os(DISABLE_SELF_UPDATE_ENV).is_some() {
        return true;
    }
    if cfg!(windows) {
        return false;
    }
    Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists()
}

pub fn self_update_unavailable_reason() -> Option<&'static str> {
    if is_source_checkout_build() {
        return Some(
            "ソースツリーの target 配下でビルドした local-build 版は自動更新できません。git pull 後に cargo build --release を実行するか、GitHub Release 版を別のディレクトリへ展開してください",
        );
    }
    if IS_LOCAL_BUILD {
        return Some(
            "local-build 版のためアップデートボタンは表示されません。GitHub Release 版へ上書き更新せず、必要に応じて手元で再ビルドしてください",
        );
    }
    if !commit_version_exists() {
        return Some(
            "develop ビルド扱いのためアップデートボタンは表示されません。実行ファイルと同じフォルダに commitversion ファイルが無い場合に発生します",
        );
    }
    if is_container_runtime() {
        return Some(
            "Docker / Podman コンテナ内では自動アップデートできません。イメージを更新してコンテナを再作成してください",
        );
    }
    None
}

fn aozoraepub3_jar_from_global_setting() -> Option<PathBuf> {
    let settings = crate::db::settings::load(SettingScope::Global).ok()?;
    let dir = settings.get("aozoraepub3dir")?.as_str()?;
    let jar = PathBuf::from(dir).join("AozoraEpub3.jar");
    jar.exists().then_some(jar)
}

fn aozoraepub3_jar_from_narou_root() -> Option<PathBuf> {
    let inv = Inventory::with_default_root().ok()?;
    let jar = inv.root_dir().join("AozoraEpub3").join("AozoraEpub3.jar");
    jar.exists().then_some(jar)
}

fn aozoraepub3_jar_next_to_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let jar = dir.join("AozoraEpub3").join("AozoraEpub3.jar");
    jar.exists().then_some(jar)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_string_keeps_package_version_prefix() {
        let version = create_version_string();
        assert!(version.starts_with(VERSION));
    }

    #[test]
    fn version_json_contains_name_and_version() {
        let value = version_json();
        assert_eq!(value["name"], NAME);
        assert!(value["version"].as_str().unwrap().starts_with(VERSION));
    }

    #[test]
    fn runtime_description_is_not_empty() {
        assert!(!runtime_description().is_empty());
    }

    #[test]
    fn cargo_target_executable_is_treated_as_local_build() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("Cargo.toml"), "[package]\n").unwrap();

        assert!(is_cargo_target_executable(
            &temp.path().join("target/release/narou_rs")
        ));
        assert!(is_cargo_target_executable(
            &temp
                .path()
                .join("target/x86_64-unknown-linux-gnu/release/narou_rs")
        ));
        assert!(!is_cargo_target_executable(
            &temp.path().join("narou/narou_rs")
        ));
    }

    #[test]
    fn version_at_most_includes_boundary() {
        assert!(version_at_most("0.4.0", "0.4.0"));
        assert!(version_at_most("0.3.6", "0.4.0"));
        assert!(version_at_most("0.4.0 (develop)", "0.4.0"));
        assert!(!version_at_most("0.4.1", "0.4.0"));
        assert!(!version_at_most("1.0.0", "0.4.0"));
        assert!(!version_at_most("garbage", "0.4.0"));
    }

    #[test]
    fn build_variant_matches_lite_feature() {
        #[cfg(feature = "lite")]
        {
            assert!(EMBEDS_AOZORA_LITE);
            assert_eq!(BUILD_VARIANT, "gpl");
        }
        #[cfg(not(feature = "lite"))]
        {
            assert!(!EMBEDS_AOZORA_LITE);
            assert_eq!(BUILD_VARIANT, "standard");
        }
    }
}



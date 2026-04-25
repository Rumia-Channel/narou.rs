use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::compat::configure_hidden_console_command;
use crate::db::inventory::Inventory;

pub const NAME: &str = "narou.rs";
pub const VERSION: &str = match option_env!("NAROU_RS_VERSION_OVERRIDE") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

pub fn create_version_string() -> String {
    if commit_version_exists() {
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

fn aozoraepub3_jar_from_global_setting() -> Option<PathBuf> {
    let path = global_setting_path()?;
    let raw = fs::read_to_string(path).ok()?;
    let settings: HashMap<String, serde_yaml::Value> = serde_yaml::from_str(&raw).ok()?;
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

fn global_setting_path() -> Option<PathBuf> {
    if let Ok(inv) = Inventory::with_default_root() {
        let dir = inv.root_dir().join(".narousetting");
        if dir.is_dir() {
            return Some(dir.join("global_setting.yaml"));
        }
    }

    let home = home_dir()?;
    let path = home.join(".narousetting").join("global_setting.yaml");
    Some(path)
}

fn home_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var("USERPROFILE").ok().map(PathBuf::from)
    } else {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
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
}

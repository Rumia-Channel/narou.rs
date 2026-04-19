use axum::{extract::State, response::Json};
use std::collections::HashMap;
use std::path::Path;

use crate::converter::device::Device;
use crate::setting_info::{
    SettingVariables, VarInfo, VarType, default_arg_command_names, is_known_default_arg_name,
    default_local_setting_value, original_setting_var_infos, setting_variables, tab_for_setting,
    webui_help_override,
};
use crate::db::inventory::{Inventory, InventoryScope};
use crate::db::with_database;

use super::AppState;
use super::state::ApiResponse;

/// Tab metadata matching narou.rb SETTING_TAB_NAMES / SETTING_TAB_INFO
const TABS: &[(&str, &str, &str)] = &[
    ("general", "一般", ""),
    ("detail", "詳細", ""),
    (
        "webui",
        "WEB UI",
        "WEB UI 専用の設定です",
    ),
    (
        "global",
        "Global",
        "Global な設定はユーザープロファイルに保存され、OSに関わらず適用されます",
    ),
    (
        "default",
        "default.*",
        "default.* 系の設定は個別の変換設定で未設定の項目の挙動を決めます",
    ),
    (
        "force",
        "force.*",
        "force.* 系の設定は個別設定、default.* 等の設定を無視して強制適用されます",
    ),
    (
        "command",
        "コマンド",
        "default_args.* 系の設定はコマンド実行時のオプションを省略した場合のデフォルト値を指定します",
    ),
    ("replace", "置換設定", ""),
];

/// GET /api/setting — returns all settings with metadata
pub async fn get_global_settings(
    State(_state): State<AppState>,
) -> Json<serde_json::Value> {
    let vars = setting_variables();
    let novel_vars = original_setting_var_infos();

    // Load current values
    let local_values: HashMap<String, serde_yaml::Value> = with_database(|db| {
        db.inventory()
            .load("local_setting", InventoryScope::Local)
    })
    .unwrap_or_default();

    let global_values: HashMap<String, serde_yaml::Value> = {
        let inv = Inventory::with_default_root().unwrap_or_else(|_| {
            Inventory::new(std::env::current_dir().unwrap_or_default())
        });
        inv.load("global_setting", InventoryScope::Global)
            .unwrap_or_default()
    };

    let mut settings = Vec::new();

    // Local settings
    for (name, info) in &vars.local {
        if info.invisible {
            continue;
        }
        let tab = tab_for_setting(name);
        if tab.is_none() {
            continue;
        }
        let value = local_values
            .get(*name)
            .cloned()
            .or_else(|| default_local_setting_value(name));
        settings.push(build_setting_entry(name, info, "local", tab.unwrap(), value));
    }

    // Global settings
    for (name, info) in &vars.global {
        if info.invisible {
            continue;
        }
        let tab = tab_for_setting(name);
        if tab.is_none() {
            continue;
        }
        let value = global_values.get(*name).cloned();
        settings.push(build_setting_entry(name, info, "global", tab.unwrap(), value));
    }

    // default.* / force.* entries from novel vars
    for prefix in &["default", "force"] {
        for (base_name, info) in &novel_vars {
            let name = format!("{}.{}", prefix, base_name);
            let tab = *prefix;
            let value = local_values.get(&name).cloned();
            let mut entry = build_setting_entry(&name, info, "local", tab, value);
            // default/force booleans use 3-way (nil/off/on)
            if matches!(info.var_type, VarType::Boolean) {
                entry["three_way"] = serde_json::json!(true);
            }
            // Make visible for the settings page
            entry["invisible"] = serde_json::json!(false);
            settings.push(entry);
        }
    }

    // default_args.* entries from known commands
    for cmd in default_arg_command_names() {
        let name = format!("default_args.{}", cmd);
        let value = local_values.get(&name).cloned();
        settings.push(serde_json::json!({
            "name": name,
            "scope": "local",
            "tab": "command",
            "var_type": "string",
            "help": format!("{} コマンドのデフォルトオプション", cmd),
            "value": yaml_to_json(value),
            "invisible": false,
        }));
    }

    // Load replace.txt content
    let replace_content = std::fs::read_to_string("replace.txt").unwrap_or_default();

    // Tabs metadata
    let tabs: Vec<serde_json::Value> = TABS
        .iter()
        .map(|(id, label, info)| {
            serde_json::json!({
                "id": id,
                "label": label,
                "info": info,
            })
        })
        .collect();

    Json(serde_json::json!({
        "tabs": tabs,
        "settings": settings,
        "replace_content": replace_content,
    }))
}

/// POST /api/setting — save settings
pub async fn save_global_settings(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<ApiResponse> {
    let Some(entries) = body["settings"].as_object() else {
        return Json(ApiResponse {
            success: false,
            message: "settings object required".to_string(),
        });
    };

    let vars = setting_variables();
    let novel_vars: HashMap<String, VarInfo> = original_setting_var_infos()
        .into_iter()
        .map(|(name, info)| (name.to_string(), info))
        .collect();

    // Separate into local and global
    let mut local_changes: HashMap<String, serde_yaml::Value> = HashMap::new();
    let mut global_changes: HashMap<String, serde_yaml::Value> = HashMap::new();
    let mut deletes_local: Vec<String> = Vec::new();
    let mut deletes_global: Vec<String> = Vec::new();
    let mut auto_schedule_changed = false;

    for (name, json_val) in entries {
        // Determine scope
        let is_global = vars.global.iter().any(|(n, _)| *n == name.as_str());

        if json_val.is_null() {
            if is_global {
                deletes_global.push(name.clone());
            } else {
                deletes_local.push(name.clone());
            }
            continue;
        }

        let yaml_val = match coerce_setting_value(name, json_val, &vars, &novel_vars) {
            Ok(value) => value,
            Err(message) => {
                return Json(ApiResponse {
                    success: false,
                    message: format!("{}: {}", name, message),
                });
            }
        };
        if is_global {
            global_changes.insert(name.clone(), yaml_val);
        } else {
            local_changes.insert(name.clone(), yaml_val);
        }
    }

    // Save local settings
    if !local_changes.is_empty() || !deletes_local.is_empty() {
        let result = with_database(|db| {
            let inv = db.inventory();
            let mut settings: HashMap<String, serde_yaml::Value> = inv
                .load("local_setting", InventoryScope::Local)
                .unwrap_or_default();
            let auto_schedule_before = auto_schedule_snapshot(&settings);
            let previous_device = setting_string(settings.get("device"));
            for (k, v) in local_changes {
                settings.insert(k, v);
            }
            for k in &deletes_local {
                settings.remove(k);
            }
            if setting_string(settings.get("device")) != previous_device {
                apply_device_related_settings(&mut settings);
            }
            inv.save("local_setting", InventoryScope::Local, &settings)?;
            Ok(auto_schedule_before != auto_schedule_snapshot(&settings))
        });
        match result {
            Ok(changed) => {
                auto_schedule_changed = changed;
            }
            Err(e) => {
                return Json(ApiResponse {
                    success: false,
                    message: format!("Failed to save local settings: {}", e),
                });
            }
        }
    }

    // Save global settings
    if !global_changes.is_empty() || !deletes_global.is_empty() {
        let result: std::result::Result<(), Box<dyn std::error::Error>> = (|| {
            let inv = Inventory::with_default_root().unwrap_or_else(|_| {
                Inventory::new(std::env::current_dir().unwrap_or_default())
            });
            let mut settings: HashMap<String, serde_yaml::Value> = inv
                .load("global_setting", InventoryScope::Global)
                .unwrap_or_default();
            for (k, v) in global_changes {
                settings.insert(k, v);
            }
            for k in &deletes_global {
                settings.remove(k);
            }
            inv.save("global_setting", InventoryScope::Global, &settings)?;
            Ok(())
        })();
        if let Err(e) = result {
            return Json(ApiResponse {
                success: false,
                message: format!("Failed to save global settings: {}", e),
            });
        }
    }

    // Save replace.txt if provided
    if let Some(content) = body["replace_content"].as_str() {
        if let Err(e) = std::fs::write("replace.txt", content) {
            return Json(ApiResponse {
                success: false,
                message: format!("Failed to save replace.txt: {}", e),
            });
        }
    }

    if auto_schedule_changed {
        let started = crate::web::scheduler::restart_auto_update_scheduler(
            state.queue.clone(),
            state.running_jobs.clone(),
            state.push_server.clone(),
            &state.auto_update_scheduler,
        );
        let message = if started {
            "自動アップデートスケジューラーを更新しました"
        } else {
            "自動アップデートスケジューラーを停止しました"
        };
        state.push_server.broadcast_echo(message, "stdout");
    }

    Json(ApiResponse {
        success: true,
        message: "設定を保存しました".to_string(),
    })
}

fn auto_schedule_snapshot(
    settings: &HashMap<String, serde_yaml::Value>,
) -> (Option<serde_yaml::Value>, Option<serde_yaml::Value>) {
    (
        settings.get("update.auto-schedule.enable").cloned(),
        settings.get("update.auto-schedule").cloned(),
    )
}

fn build_setting_entry(
    name: &str,
    info: &crate::setting_info::VarInfo,
    scope: &str,
    tab: &str,
    value: Option<serde_yaml::Value>,
) -> serde_json::Value {
    let help = webui_help_override(name, info.help)
        .unwrap_or_else(|| info.help.to_string());
    serde_json::json!({
        "name": name,
        "scope": scope,
        "tab": tab,
        "var_type": info.var_type,
        "help": help,
        "value": yaml_to_json(value),
        "select_keys": info.select_keys,
        "select_summaries": select_summaries_for_setting(name, info),
        "invisible": info.invisible,
    })
}

fn select_summaries_for_setting(name: &str, info: &VarInfo) -> Option<Vec<String>> {
    let keys = info.select_keys.as_ref()?;
    let base_name = name
        .strip_prefix("default.")
        .or_else(|| name.strip_prefix("force."))
        .unwrap_or(name);
    Some(match base_name {
        "device" | "convert.multi-device" => keys
            .iter()
            .map(|key| Device::from_str(key).display_name().to_string())
            .collect(),
        "update.sort-by" => keys
            .iter()
            .map(|key| match key.as_str() {
                "id" => "ID".to_string(),
                "last_update" => "更新日".to_string(),
                "title" => "タイトル".to_string(),
                "author" => "作者".to_string(),
                "site" => "掲載サイト".to_string(),
                "keyword" => "キーワード".to_string(),
                "general_lastup" => "掲載日".to_string(),
                "new_arrivals_date" => "新着日".to_string(),
                _ => key.clone(),
            })
            .collect(),
        "convert.copy-to-grouping" => vec![
            "端末毎にまとめる".to_string(),
            "掲載サイト毎にまとめる".to_string(),
        ],
        "economy" => vec![
            "変換後に作業ファイルを削除".to_string(),
            "送信後に書籍ファイルを削除".to_string(),
            "差分ファイルを保存しない".to_string(),
            "rawデータを保存しない".to_string(),
        ],
        "webui.table.reload-timing" => {
            vec!["１作品ごとに更新".to_string(), "キューごとに更新".to_string()]
        }
        "webui.performance-mode" => vec![
            "自動判定".to_string(),
            "常に有効".to_string(),
            "常に無効".to_string(),
        ],
        _ => keys.clone(),
    })
}

fn yaml_to_json(value: Option<serde_yaml::Value>) -> serde_json::Value {
    match value {
        None => serde_json::Value::Null,
        Some(v) => match v {
            serde_yaml::Value::Null => serde_json::Value::Null,
            serde_yaml::Value::Bool(b) => serde_json::Value::Bool(b),
            serde_yaml::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    serde_json::json!(i)
                } else if let Some(f) = n.as_f64() {
                    serde_json::json!(f)
                } else {
                    serde_json::Value::Null
                }
            }
            serde_yaml::Value::String(s) => serde_json::Value::String(s),
            serde_yaml::Value::Sequence(seq) => {
                let arr: Vec<serde_json::Value> = seq
                    .into_iter()
                    .filter_map(|v| yaml_to_json(Some(v)).as_str().map(String::from))
                    .map(serde_json::Value::String)
                    .collect();
                serde_json::Value::Array(arr)
            }
            _ => serde_json::Value::Null,
        },
    }
}

fn coerce_setting_value(
    name: &str,
    value: &serde_json::Value,
    vars: &SettingVariables,
    novel_vars: &HashMap<String, VarInfo>,
) -> Result<serde_yaml::Value, String> {
    if let Some(info) = vars.get(name) {
        return coerce_value_for_type(info, value);
    }
    if let Some(base_name) = name
        .strip_prefix("default.")
        .or_else(|| name.strip_prefix("force."))
    {
        if let Some(info) = novel_vars.get(base_name) {
            return coerce_value_for_type(info, value);
        }
    }
    if is_known_default_arg_name(name) {
        return coerce_string_value(value);
    }
    Err("不明な設定名です".to_string())
}

fn coerce_value_for_type(info: &VarInfo, value: &serde_json::Value) -> Result<serde_yaml::Value, String> {
    match info.var_type {
        VarType::Boolean => coerce_bool_value(value),
        VarType::Integer => coerce_integer_value(value),
        VarType::Float => coerce_float_value(value),
        VarType::String => coerce_string_value(value),
        VarType::Select => coerce_select_value(value, info.select_keys.as_deref()),
        VarType::Multiple => coerce_multiple_value(value, info.select_keys.as_deref()),
        VarType::Directory => coerce_directory_value(value),
    }
}

fn coerce_bool_value(value: &serde_json::Value) -> Result<serde_yaml::Value, String> {
    match value {
        serde_json::Value::Bool(flag) => Ok(serde_yaml::Value::Bool(*flag)),
        serde_json::Value::String(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "true" => Ok(serde_yaml::Value::Bool(true)),
            "false" => Ok(serde_yaml::Value::Bool(false)),
            _ => Err("true か false を指定して下さい".to_string()),
        },
        _ => Err("true か false を指定して下さい".to_string()),
    }
}

fn coerce_integer_value(value: &serde_json::Value) -> Result<serde_yaml::Value, String> {
    let number = match value {
        serde_json::Value::Number(raw) => raw
            .as_i64()
            .ok_or_else(|| "整数を指定して下さい".to_string())?,
        serde_json::Value::String(raw) => raw
            .trim()
            .parse::<i64>()
            .map_err(|_| "整数を指定して下さい".to_string())?,
        _ => return Err("整数を指定して下さい".to_string()),
    };
    Ok(serde_yaml::Value::Number(serde_yaml::Number::from(number)))
}

fn coerce_float_value(value: &serde_json::Value) -> Result<serde_yaml::Value, String> {
    let number = match value {
        serde_json::Value::Number(raw) => raw
            .as_f64()
            .ok_or_else(|| "数値を指定して下さい".to_string())?,
        serde_json::Value::String(raw) => raw
            .trim()
            .parse::<f64>()
            .map_err(|_| "数値を指定して下さい".to_string())?,
        _ => return Err("数値を指定して下さい".to_string()),
    };
    Ok(serde_yaml::Value::Number(serde_yaml::Number::from(number)))
}

fn coerce_string_value(value: &serde_json::Value) -> Result<serde_yaml::Value, String> {
    match value {
        serde_json::Value::String(raw) => Ok(serde_yaml::Value::String(raw.clone())),
        serde_json::Value::Number(raw) => Ok(serde_yaml::Value::String(raw.to_string())),
        serde_json::Value::Bool(raw) => Ok(serde_yaml::Value::String(raw.to_string())),
        _ => Err("文字列を指定して下さい".to_string()),
    }
}

fn coerce_select_value(
    value: &serde_json::Value,
    select_keys: Option<&[String]>,
) -> Result<serde_yaml::Value, String> {
    let selected = match value {
        serde_json::Value::String(raw) => raw.clone(),
        _ => return Err("選択肢の中から指定して下さい".to_string()),
    };
    if let Some(keys) = select_keys {
        if !keys.iter().any(|key| key == &selected) {
            return Err(format!(
                "不明な値です。{} の中から指定して下さい",
                keys.join(", ")
            ));
        }
    }
    Ok(serde_yaml::Value::String(selected))
}

fn coerce_multiple_value(
    value: &serde_json::Value,
    select_keys: Option<&[String]>,
) -> Result<serde_yaml::Value, String> {
    let raw = match value {
        serde_json::Value::String(raw) => raw.clone(),
        serde_json::Value::Array(values) => values
            .iter()
            .map(|item| match item {
                serde_json::Value::String(text) => Ok(text.clone()),
                _ => Err("複数選択の値が不正です".to_string()),
            })
            .collect::<Result<Vec<_>, _>>()?
            .join(","),
        _ => return Err("複数選択の値が不正です".to_string()),
    };
    if let Some(keys) = select_keys {
        for part in raw.split(',').map(str::trim).filter(|part| !part.is_empty()) {
            if !keys.iter().any(|key| key == part) {
                return Err(format!(
                    "不明な値です。{} の中から指定して下さい",
                    keys.join(", ")
                ));
            }
        }
    }
    Ok(serde_yaml::Value::String(raw))
}

fn coerce_directory_value(value: &serde_json::Value) -> Result<serde_yaml::Value, String> {
    let raw = match value {
        serde_json::Value::String(raw) => raw.trim(),
        _ => return Err("存在するフォルダを指定して下さい".to_string()),
    };
    let path = Path::new(raw);
    if !path.is_dir() {
        return Err("存在するフォルダを指定して下さい".to_string());
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|_| "存在するフォルダを指定して下さい".to_string())?;
    Ok(serde_yaml::Value::String(
        strip_extended_path_prefix(canonical)
            .to_string_lossy()
            .to_string(),
    ))
}

fn strip_extended_path_prefix(path: std::path::PathBuf) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let raw = path.to_string_lossy();
        if let Some(rest) = raw.strip_prefix(r"\\?\") {
            return std::path::PathBuf::from(rest);
        }
    }
    path
}

fn setting_string(value: Option<&serde_yaml::Value>) -> Option<String> {
    match value {
        Some(serde_yaml::Value::String(raw)) => Some(raw.clone()),
        _ => None,
    }
}

fn apply_device_related_settings(settings: &mut HashMap<String, serde_yaml::Value>) {
    let Some(device) = setting_string(settings.get("device")) else {
        return;
    };
    let desired_half_indent = match device.to_ascii_lowercase().as_str() {
        "kindle" => true,
        "kobo" | "epub" | "ibunko" | "reader" | "ibooks" => false,
        _ => return,
    };
    settings.insert(
        "default.enable_half_indent_bracket".to_string(),
        serde_yaml::Value::Bool(desired_half_indent),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coerce_float_setting_from_string() {
        let vars = setting_variables();
        let novel_vars: HashMap<String, VarInfo> = original_setting_var_infos()
            .into_iter()
            .map(|(name, info)| (name.to_string(), info))
            .collect();
        let value = coerce_setting_value(
            "update.interval",
            &serde_json::json!("1.5"),
            &vars,
            &novel_vars,
        )
        .unwrap();
        assert_eq!(value, serde_yaml::Value::Number(serde_yaml::Number::from(1.5)));
    }

    #[test]
    fn coerce_select_setting_rejects_unknown_value() {
        let vars = setting_variables();
        let novel_vars: HashMap<String, VarInfo> = original_setting_var_infos()
            .into_iter()
            .map(|(name, info)| (name.to_string(), info))
            .collect();
        assert!(coerce_setting_value(
            "webui.table.reload-timing",
            &serde_json::json!("invalid"),
            &vars,
            &novel_vars,
        )
        .is_err());
    }

    #[test]
    fn apply_device_related_settings_updates_half_indent() {
        let mut settings = HashMap::from([(
            "device".to_string(),
            serde_yaml::Value::String("kobo".to_string()),
        )]);
        apply_device_related_settings(&mut settings);
        assert_eq!(
            settings.get("default.enable_half_indent_bracket"),
            Some(&serde_yaml::Value::Bool(false))
        );
    }

    #[test]
    fn select_summaries_use_display_labels() {
        let vars = setting_variables();
        let info = vars
            .get("webui.performance-mode")
            .expect("webui.performance-mode metadata");
        assert_eq!(
            select_summaries_for_setting("webui.performance-mode", info),
            Some(vec![
                "自動判定".to_string(),
                "常に有効".to_string(),
                "常に無効".to_string(),
            ])
        );
    }

    #[test]
    fn select_summaries_support_default_prefixed_settings() {
        let vars = setting_variables();
        let info = vars.get("device").expect("device metadata");
        assert_eq!(
            select_summaries_for_setting("default.device", info),
            Some(vec![
                "Kindle".to_string(),
                "Kobo".to_string(),
                "EPUB".to_string(),
                "i文庫".to_string(),
                "SonyReader".to_string(),
                "iBooks".to_string(),
            ])
        );
    }
}

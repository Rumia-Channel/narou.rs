use axum::{extract::State, response::Json};
use std::collections::HashMap;

use crate::setting_info::{
    VarType, original_setting_var_infos, setting_variables, tab_for_setting,
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
        "Global な設定はユーザープロファイルに保存され、ostに関わらず適用されます",
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
        let value = local_values.get(*name).cloned();
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
    let command_names = [
        "download", "update", "convert", "diff", "inspect", "send", "trace",
        "console", "list", "csv",
    ];
    for cmd in &command_names {
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
    State(_state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<ApiResponse> {
    let Some(entries) = body["settings"].as_object() else {
        return Json(ApiResponse {
            success: false,
            message: "settings object required".to_string(),
        });
    };

    let vars = setting_variables();

    // Separate into local and global
    let mut local_changes: HashMap<String, serde_yaml::Value> = HashMap::new();
    let mut global_changes: HashMap<String, serde_yaml::Value> = HashMap::new();
    let mut deletes_local: Vec<String> = Vec::new();
    let mut deletes_global: Vec<String> = Vec::new();

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

        let yaml_val = json_to_yaml(json_val);
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
            for (k, v) in local_changes {
                settings.insert(k, v);
            }
            for k in &deletes_local {
                settings.remove(k);
            }
            inv.save("local_setting", InventoryScope::Local, &settings)?;
            Ok(())
        });
        if let Err(e) = result {
            return Json(ApiResponse {
                success: false,
                message: format!("Failed to save local settings: {}", e),
            });
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

    Json(ApiResponse {
        success: true,
        message: "設定を保存しました".to_string(),
    })
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
        "invisible": info.invisible,
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

fn json_to_yaml(value: &serde_json::Value) -> serde_yaml::Value {
    match value {
        serde_json::Value::Null => serde_yaml::Value::Null,
        serde_json::Value::Bool(b) => serde_yaml::Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                serde_yaml::Value::Number(serde_yaml::Number::from(i))
            } else if let Some(f) = n.as_f64() {
                serde_yaml::Value::Number(serde_yaml::Number::from(f))
            } else {
                serde_yaml::Value::Null
            }
        }
        serde_json::Value::String(s) => {
            // Parse string booleans/numbers if needed
            if s == "true" {
                serde_yaml::Value::Bool(true)
            } else if s == "false" {
                serde_yaml::Value::Bool(false)
            } else if let Ok(i) = s.parse::<i64>() {
                serde_yaml::Value::Number(serde_yaml::Number::from(i))
            } else {
                serde_yaml::Value::String(s.clone())
            }
        }
        serde_json::Value::Array(arr) => {
            let seq: Vec<serde_yaml::Value> = arr.iter().map(json_to_yaml).collect();
            serde_yaml::Value::Sequence(seq)
        }
        _ => serde_yaml::Value::Null,
    }
}

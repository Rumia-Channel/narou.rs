use std::collections::HashMap;
use std::path::Path as FsPath;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use serde::Serialize;

use crate::converter::ini::{IniData, IniValue};
use crate::converter::settings::load_replace_patterns;
use crate::db::with_database;
use crate::error::NarouError;
use crate::setting_info::{VarInfo, VarType, original_setting_var_infos};

use super::AppState;
use super::state::{ApiResponse, IdPath};

#[derive(Debug, Serialize)]
struct NovelSettingEntry {
    name: String,
    help: String,
    var_type: VarType,
    #[serde(skip_serializing_if = "Option::is_none")]
    select_keys: Option<Vec<String>>,
    value: serde_json::Value,
}

pub async fn get_settings(
    State(_state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let record = with_database(|db| {
        db.get(id)
            .cloned()
            .ok_or_else(|| NarouError::NotFound(format!("ID: {}", id)))
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    let novel_dir = with_database(|db| {
        super::safe_existing_novel_dir(db.archive_root(), &record)
            .map_err(NarouError::Database)
    })
    .map_err(|e| {
        eprintln!("web load novel settings directory failed for {}: {}", id, e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "設定の読み込みに失敗しました".to_string(),
        )
    })?;

    let ini_path = novel_dir.join("setting.ini");
    let ini = IniData::load_file(&ini_path)
        .map_err(|e| {
            eprintln!("web load setting.ini failed for {}: {}", id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "設定の読み込みに失敗しました".to_string(),
            )
        })?;
    let replace_patterns: Vec<serde_json::Value> = load_replace_patterns(&novel_dir.join("replace.txt"))
        .into_iter()
        .map(|(left, right)| serde_json::json!({ "left": left, "right": right }))
        .collect();
    let settings = build_setting_entries(&ini);

    Ok(Json(serde_json::json!({
        "id": id,
        "title": record.title,
        "author": record.author,
        "settings": settings,
        "replace_patterns": replace_patterns,
    })))
}

pub async fn save_settings(
    State(_state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let record = with_database(|db| {
        db.get(id)
            .cloned()
            .ok_or_else(|| NarouError::NotFound(format!("ID: {}", id)))
    })
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    let novel_dir = with_database(|db| {
        super::safe_existing_novel_dir(db.archive_root(), &record)
            .map_err(NarouError::Database)
    })
    .map_err(|e| {
        eprintln!("web resolve novel settings directory failed for {}: {}", id, e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "設定の保存に失敗しました".to_string(),
        )
    })?;

    let mut ini = IniData::load_file(&novel_dir.join("setting.ini"))
        .map_err(|e| {
            eprintln!("web load setting.ini for save failed for {}: {}", id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "設定の保存に失敗しました".to_string(),
            )
        })?;
    let setting_map = body
        .get("settings")
        .and_then(|v| v.as_array())
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "settings array required".to_string()))?;
    let known_vars: HashMap<&'static str, VarInfo> = original_setting_var_infos()
        .into_iter()
        .collect();

    for item in setting_map {
        let Some(name) = item.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(info) = known_vars.get(name) else {
            continue;
        };
        let value = item.get("value").unwrap_or(&serde_json::Value::Null);
        apply_setting_value(&mut ini, name, info, value)
            .map_err(|message| (StatusCode::BAD_REQUEST, message))?;
    }

    ini.save(&novel_dir.join("setting.ini"))
        .map_err(|e| {
            eprintln!("web save setting.ini failed for {}: {}", id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "設定の保存に失敗しました".to_string(),
            )
        })?;

    if let Some(patterns) = body.get("replace_patterns").and_then(|v| v.as_array()) {
        save_replace_patterns(&novel_dir.join("replace.txt"), patterns)
            .map_err(|e| {
                eprintln!("web save replace.txt failed for {}: {}", id, e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "replace.txt の保存に失敗しました".to_string(),
                )
            })?;
    }

    Ok(Json(ApiResponse {
        success: true,
        message: "Settings saved".to_string(),
    }))
}

pub async fn list_devices(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let devices = crate::converter::device::OutputManager::available_devices();
    let list: Vec<serde_json::Value> = devices
        .iter()
        .map(|(name, available)| serde_json::json!({ "name": name, "available": available }))
        .collect();
    Json(serde_json::json!({ "devices": list }))
}

fn build_setting_entries(ini: &IniData) -> Vec<NovelSettingEntry> {
    let global = ini.global_section();
    original_setting_var_infos()
        .into_iter()
        .map(|(name, info)| NovelSettingEntry {
            name: name.to_string(),
            help: info.help.to_string(),
            var_type: info.var_type,
            select_keys: info.select_keys,
            value: ini_value_to_json(global.get(name)),
        })
        .collect()
}

fn ini_value_to_json(value: Option<&IniValue>) -> serde_json::Value {
    match value {
        None | Some(IniValue::Null) => serde_json::Value::Null,
        Some(IniValue::Boolean(b)) => serde_json::Value::Bool(*b),
        Some(IniValue::Integer(i)) => serde_json::json!(*i),
        Some(IniValue::Float(f)) => serde_json::json!(*f),
        Some(IniValue::String(s)) => serde_json::Value::String(s.clone()),
    }
}

fn apply_setting_value(
    ini: &mut IniData,
    name: &str,
    info: &VarInfo,
    value: &serde_json::Value,
) -> Result<(), String> {
    let parsed = match info.var_type {
        VarType::Boolean => parse_bool_value(value)?,
        VarType::Integer => parse_integer_value(value)?,
        VarType::Float => parse_float_value(value)?,
        VarType::String => parse_string_value(value)?,
        VarType::Select => parse_select_value(value, info.select_keys.as_deref())?,
        VarType::Multiple => parse_multiple_value(value, info.select_keys.as_deref())?,
        VarType::Directory => parse_string_value(value)?,
    };

    if let Some(parsed) = parsed {
        ini.set_global(name, parsed);
    } else if let Some(global) = ini.sections.get_mut("global") {
        global.remove(name);
    }

    Ok(())
}

fn parse_bool_value(value: &serde_json::Value) -> Result<Option<IniValue>, String> {
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Bool(flag) => Ok(Some(IniValue::Boolean(*flag))),
        serde_json::Value::String(raw) => {
            match raw.trim().to_ascii_lowercase().as_str() {
                "" => Ok(None),
                "true" => Ok(Some(IniValue::Boolean(true))),
                "false" => Ok(Some(IniValue::Boolean(false))),
                _ => Err("true か false を指定して下さい".to_string()),
            }
        }
        _ => Err("true か false を指定して下さい".to_string()),
    }
}

fn parse_integer_value(value: &serde_json::Value) -> Result<Option<IniValue>, String> {
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Number(raw) => raw
            .as_i64()
            .map(|v| Some(IniValue::Integer(v)))
            .ok_or_else(|| "整数を指定して下さい".to_string()),
        serde_json::Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                trimmed
                    .parse::<i64>()
                    .map(|v| Some(IniValue::Integer(v)))
                    .map_err(|_| "整数を指定して下さい".to_string())
            }
        }
        _ => Err("整数を指定して下さい".to_string()),
    }
}

fn parse_float_value(value: &serde_json::Value) -> Result<Option<IniValue>, String> {
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Number(raw) => raw
            .as_f64()
            .map(|v| Some(IniValue::Float(v)))
            .ok_or_else(|| "数値を指定して下さい".to_string()),
        serde_json::Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                trimmed
                    .parse::<f64>()
                    .map(|v| Some(IniValue::Float(v)))
                    .map_err(|_| "数値を指定して下さい".to_string())
            }
        }
        _ => Err("数値を指定して下さい".to_string()),
    }
}

fn parse_string_value(value: &serde_json::Value) -> Result<Option<IniValue>, String> {
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(raw) => {
            if raw.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(IniValue::String(raw.clone())))
            }
        }
        serde_json::Value::Number(raw) => Ok(Some(IniValue::String(raw.to_string()))),
        serde_json::Value::Bool(raw) => Ok(Some(IniValue::String(raw.to_string()))),
        _ => Err("文字列を指定して下さい".to_string()),
    }
}

fn parse_select_value(
    value: &serde_json::Value,
    select_keys: Option<&[String]>,
) -> Result<Option<IniValue>, String> {
    let Some(keys) = select_keys else {
        return parse_string_value(value);
    };

    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            if keys.iter().any(|key| key == trimmed) {
                Ok(Some(IniValue::String(trimmed.to_string())))
            } else {
                Err("選択肢の中から指定して下さい".to_string())
            }
        }
        _ => Err("選択肢の中から指定して下さい".to_string()),
    }
}

fn parse_multiple_value(
    value: &serde_json::Value,
    select_keys: Option<&[String]>,
) -> Result<Option<IniValue>, String> {
    let Some(keys) = select_keys else {
        return parse_string_value(value);
    };

    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Array(items) => {
            let mut selected = Vec::new();
            for item in items {
                let Some(raw) = item.as_str() else {
                    return Err("選択肢の中から指定して下さい".to_string());
                };
                if !keys.iter().any(|key| key == raw) {
                    return Err("選択肢の中から指定して下さい".to_string());
                }
                selected.push(raw.to_string());
            }
            if selected.is_empty() {
                Ok(None)
            } else {
                Ok(Some(IniValue::String(selected.join(","))))
            }
        }
        serde_json::Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(IniValue::String(trimmed.to_string())))
            }
        }
        _ => Err("選択肢の中から指定して下さい".to_string()),
    }
}

fn save_replace_patterns(
    path: &FsPath,
    patterns: &[serde_json::Value],
) -> std::io::Result<()> {
    if patterns.len() > super::MAX_WEB_TAGS_PER_REQUEST {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "too many replace patterns",
        ));
    }
    let mut lines = Vec::new();
    for pattern in patterns {
        let Some(left) = pattern.get("left").and_then(|v| v.as_str()) else {
            continue;
        };
        let left = left.trim();
        if left.is_empty() {
            continue;
        }
        if left.len() > super::MAX_WEB_TAG_LENGTH {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "replace pattern is too long",
            ));
        }
        let right = pattern
            .get("right")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if right.len() > super::MAX_WEB_TEXT_INPUT_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "replace pattern replacement is too long",
            ));
        }
        lines.push(format!("{}\t{}", left, right));
    }
    std::fs::write(path, lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_numeric_values_are_removed() {
        let mut ini = IniData::new();
        let info = VarInfo {
            var_type: VarType::Integer,
            help: "",
            invisible: false,
            select_keys: None,
        };

        apply_setting_value(
            &mut ini,
            "to_page_break_threshold",
            &info,
            &serde_json::Value::String(String::new()),
        )
        .unwrap();
        assert!(ini.get_global("to_page_break_threshold").is_none());
    }

    #[test]
    fn blank_string_values_are_removed() {
        let mut ini = IniData::new();
        let info = VarInfo {
            var_type: VarType::String,
            help: "",
            invisible: false,
            select_keys: None,
        };

        apply_setting_value(
            &mut ini,
            "novel_title",
            &info,
            &serde_json::Value::String("   ".to_string()),
        )
        .unwrap();
        assert!(ini.get_global("novel_title").is_none());
    }

    #[test]
    fn replace_patterns_skip_blank_left_side() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("test-artifacts")
            .join(format!("narou-rs-replace-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        save_replace_patterns(
            &dir.join("replace.txt"),
            &[
                serde_json::json!({"left": "  ", "right": "x"}),
                serde_json::json!({"left": "a", "right": "b"}),
            ],
        )
        .unwrap();
        let content = std::fs::read_to_string(dir.join("replace.txt")).unwrap();
        assert_eq!(content, "a\tb");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn replace_patterns_reject_too_many_entries() {
        let patterns: Vec<serde_json::Value> = (0..=super::super::MAX_WEB_TAGS_PER_REQUEST)
            .map(|index| serde_json::json!({"left": format!("k{}", index), "right": "v"}))
            .collect();
        let dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("test-artifacts")
            .join(format!("narou-rs-replace-overflow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let err = save_replace_patterns(&dir.join("replace.txt"), &patterns).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let _ = std::fs::remove_dir_all(dir);
    }
}

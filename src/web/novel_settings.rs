use std::collections::HashMap;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};

use crate::converter::ini::{IniData, IniValue};
use crate::setting_info::{VarInfo, VarType, original_setting_var_infos};

use super::AppState;
use super::state::{ApiResponse, IdPath};


fn map_application_error(error: crate::application::ApplicationError) -> (StatusCode, String) {
    match error {
        crate::application::ApplicationError::InvalidRequest(message) => {
            (StatusCode::BAD_REQUEST, message)
        }
        crate::application::ApplicationError::NotFound(message) => {
            (StatusCode::NOT_FOUND, message)
        }
        crate::application::ApplicationError::Platform(message) => {
            (StatusCode::INTERNAL_SERVER_ERROR, message)
        }
    }
}

pub async fn get_settings(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let view = state
        .services
        .novel_settings
        .load(id.into())
        .await
        .map_err(map_application_error)?;
    let settings: Vec<serde_json::Value> = view
        .settings
        .iter()
        .map(|entry| {
            serde_json::json!({
                "name": entry.name,
                "help": entry.help,
                "var_type": entry.var_type,
                "select_keys": entry.select_keys,
                "value": ini_value_to_json(entry.value.as_ref()),
            })
        })
        .collect();
    let replace_patterns: Vec<serde_json::Value> = view
        .replace_patterns
        .iter()
        .map(|pattern| serde_json::json!({ "left": pattern.left, "right": pattern.right }))
        .collect();

    Ok(Json(serde_json::json!({
        "id": view.id,
        "title": view.title,
        "author": view.author,
        "settings": settings,
        "replace_patterns": replace_patterns,
    })))
}

pub async fn save_settings(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<ApiResponse>, (StatusCode, String)> {
    let setting_map = body
        .get("settings")
        .and_then(|v| v.as_array())
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "settings array required".to_string()))?;
    let known_vars: HashMap<&'static str, VarInfo> =
        original_setting_var_infos().into_iter().collect();
    let mut settings = HashMap::new();
    for item in setting_map {
        let Some(name) = item.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(info) = known_vars.get(name) else {
            return Err((StatusCode::BAD_REQUEST, format!("不明な設定名です: {name}")));
        };
        let value = item.get("value").unwrap_or(&serde_json::Value::Null);
        let mut parsed = IniData::new();
        apply_setting_value(&mut parsed, name, info, value)
            .map_err(|message| (StatusCode::BAD_REQUEST, message))?;
        settings.insert(name.to_string(), parsed.get_global(name).cloned());
    }

    let replace_patterns = body
        .get("replace_patterns")
        .map(|value| {
            let values = value.as_array().ok_or_else(|| {
                (StatusCode::BAD_REQUEST, "replace_patterns must be an array".to_string())
            })?;
            values
                .iter()
                .map(|value| {
                    let left = value
                        .get("left")
                        .and_then(|value| value.as_str())
                        .ok_or_else(|| {
                            (StatusCode::BAD_REQUEST, "replace pattern left is required".to_string())
                        })?;
                    let right = value
                        .get("right")
                        .and_then(|value| value.as_str())
                        .ok_or_else(|| {
                            (StatusCode::BAD_REQUEST, "replace pattern right is required".to_string())
                        })?;
                    Ok(crate::application::ReplacePattern {
                        left: left.to_string(),
                        right: right.to_string(),
                    })
                })
                .collect::<Result<Vec<_>, (StatusCode, String)>>()
        })
        .transpose()?;

    state
        .services
        .novel_settings
        .save(
            id.into(),
            &crate::application::NovelSettingsPatch {
                settings,
                replace_patterns,
            },
        )
        .await
        .map_err(map_application_error)?;

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

}

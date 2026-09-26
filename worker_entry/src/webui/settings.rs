//! 個別小説の設定 API (native: `src/web/novel_settings.rs`)。
//!
//! - `GET /api/settings/{id}` — native `get_settings` と同じ形
//!   (`{id, title, author, settings[], replace_patterns[]}`)。
//! - `POST /api/settings/{id}` — native `save_settings` と同じ受理形・
//!   エラーメッセージ・成功応答 (`ApiResponse`)。
//! - `GET /api/devices` — native `list_devices` と同じ形
//!   (`{devices: [{name, available}]}`)。Worker では外部プロセス依存の
//!   フォーマット (AozoraEpub3 / kindlegen 要求のもの) を `false` にする。
//!   `epub` は Lite (同梱エンジン) で生成できるので `true`。
//!
//! 値の解析・検証は native と同じ関数群 (このファイル末尾の `parse_*`
//! は `src/web/novel_settings.rs` の写し)。

use std::collections::HashMap;

use narou_rs::application::{AppServices, NovelSettingsPatch, ReplacePattern};
use narou_rs::converter::ini::{IniData, IniValue};
use narou_rs::setting_info::{VarInfo, VarType, original_setting_var_infos};
use worker::{Env, Method, Request, Response};

use super::{application_error_response, json_error};

/// `/api/settings/{id}` と `/api/devices` の振り分け。
pub async fn handle(mut req: Request, env: Env) -> worker::Result<Response> {
    let path = req.path();
    enum Route {
        GetSettings(i64),
        SaveSettings(i64),
        Devices,
    }
    let route = match (req.method(), path.as_str()) {
        (Method::Get, "/api/devices") => Route::Devices,
        (_, "/api/devices") => return json_error(405, "method_not_allowed", None),
        (method, _) => {
            let Some(id) = path
                .strip_prefix("/api/settings/")
                .and_then(|rest| rest.parse::<i64>().ok())
            else {
                return json_error(404, "not_found", None);
            };
            match method {
                Method::Get => Route::GetSettings(id),
                Method::Post => Route::SaveSettings(id),
                _ => return json_error(405, "method_not_allowed", None),
            }
        }
    };

    if let Some(response) = crate::auth_failure(&req, &env).await {
        return response;
    }
    let services = match crate::composition::build_services(&env).await {
        Ok(services) => services,
        Err(_) => return Response::error("Service unavailable", 503),
    };
    match route {
        Route::GetSettings(id) => get_settings(&services, id).await,
        Route::SaveSettings(id) => save_settings(&mut req, &services, id).await,
        Route::Devices => list_devices(),
    }
}

// ---------------------------------------------------------------------------
// GET /api/settings/{id} (native: src/web/novel_settings.rs get_settings)
// ---------------------------------------------------------------------------

async fn get_settings(services: &AppServices, id: i64) -> worker::Result<Response> {
    let view = match services.novel_settings.load(id.into()).await {
        Ok(view) => view,
        Err(error) => return application_error_response(&error),
    };
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
    Response::from_json(&serde_json::json!({
        "id": view.id,
        "title": view.title,
        "author": view.author,
        "settings": settings,
        "replace_patterns": replace_patterns,
    }))
}

// ---------------------------------------------------------------------------
// POST /api/settings/{id} (native: src/web/novel_settings.rs save_settings)
// ---------------------------------------------------------------------------

async fn save_settings(
    req: &mut Request,
    services: &AppServices,
    id: i64,
) -> worker::Result<Response> {
    // axum `Json` と同じく、本文が壊れていればここで 400。
    let body: serde_json::Value = match req.json().await {
        Ok(body) => body,
        Err(_) => return json_error(400, "bad_request", Some("invalid JSON body")),
    };
    let Some(setting_map) = body.get("settings").and_then(|v| v.as_array()) else {
        return json_error(400, "bad_request", Some("settings array required"));
    };
    let known_vars: HashMap<&'static str, VarInfo> =
        original_setting_var_infos().into_iter().collect();
    let mut settings = HashMap::new();
    for item in setting_map {
        let Some(name) = item.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(info) = known_vars.get(name) else {
            return json_error(
                400,
                "bad_request",
                Some(&format!("不明な設定名です: {name}")),
            );
        };
        let value = item.get("value").unwrap_or(&serde_json::Value::Null);
        let mut parsed = IniData::new();
        if let Err(message) = apply_setting_value(&mut parsed, name, info, value) {
            return json_error(400, "bad_request", Some(&message));
        }
        settings.insert(name.to_string(), parsed.get_global(name).cloned());
    }

    let replace_patterns = body
        .get("replace_patterns")
        .map(|value| {
            let Some(values) = value.as_array() else {
                return Err("replace_patterns must be an array".to_string());
            };
            values
                .iter()
                .map(|value| {
                    let left = value
                        .get("left")
                        .and_then(|value| value.as_str())
                        .ok_or_else(|| "replace pattern left is required".to_string())?;
                    let right = value
                        .get("right")
                        .and_then(|value| value.as_str())
                        .ok_or_else(|| "replace pattern right is required".to_string())?;
                    Ok(ReplacePattern {
                        left: left.to_string(),
                        right: right.to_string(),
                    })
                })
                .collect::<Result<Vec<_>, String>>()
        })
        .transpose();
    let replace_patterns = match replace_patterns {
        Ok(patterns) => patterns,
        Err(message) => return json_error(400, "bad_request", Some(&message)),
    };

    if let Err(error) = services
        .novel_settings
        .save(id.into(), &NovelSettingsPatch {
            settings,
            replace_patterns,
        })
        .await
    {
        return application_error_response(&error);
    }
    Response::from_json(&super::api_response(true, "Settings saved"))
}

// ---------------------------------------------------------------------------
// GET /api/devices (native: src/web/novel_settings.rs list_devices)
// ---------------------------------------------------------------------------

/// native `OutputManager::available_devices` の対応表。Worker は外部プロセス
/// (AozoraEpub3 / kindlegen) を起動できないので、それを要求するフォーマット
/// は `false`。`epub` は同梱の Lite エンジンで要求時生成するため `true`、
/// `text` / `ibunko` は出力自体がテキスト系で Worker 内で完結する。
fn list_devices() -> worker::Result<Response> {
    let devices = [
        ("text", true),
        ("epub", true),
        ("mobi", false),
        ("kobo", false),
        ("ibunko", true),
        ("reader", false),
        ("ibooks", false),
    ];
    let list: Vec<serde_json::Value> = devices
        .iter()
        .map(|(name, available)| serde_json::json!({ "name": name, "available": available }))
        .collect();
    Response::from_json(&serde_json::json!({ "devices": list }))
}

// ---------------------------------------------------------------------------
// INI 値の解析 (native src/web/novel_settings.rs の parse_* の写し)
// ---------------------------------------------------------------------------

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
        serde_json::Value::String(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "" => Ok(None),
            "true" => Ok(Some(IniValue::Boolean(true))),
            "false" => Ok(Some(IniValue::Boolean(false))),
            _ => Err("true か false を指定して下さい".to_string()),
        },
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

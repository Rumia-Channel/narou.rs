use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::setting_info::{self, VarInfo, VarType};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingScope {
    Local,
    Global,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceRelatedSettingChanges {
    pub device_display_name: &'static str,
    pub changes: Vec<(String, serde_yaml::Value)>,
}

pub fn setting_scope(name: &str) -> Option<SettingScope> {
    let vars = setting_info::setting_variables();
    if vars.local.iter().any(|(n, _)| *n == name) {
        return Some(SettingScope::Local);
    }
    if vars.global.iter().any(|(n, _)| *n == name) {
        return Some(SettingScope::Global);
    }
    if name
        .strip_prefix("default.")
        .or_else(|| name.strip_prefix("force."))
        .is_some_and(|rest| original_setting_var_info(rest).is_some())
    {
        return Some(SettingScope::Local);
    }
    if setting_info::is_known_default_arg_name(name) {
        return Some(SettingScope::Local);
    }
    None
}

pub fn cast_setting_value(name: &str, value_str: &str) -> Result<serde_yaml::Value, String> {
    if let Some(info) = setting_info::setting_variables().get(name) {
        return cast_value_for_type(info.var_type, value_str, info.select_keys.as_deref());
    }

    if let Some(rest) = name
        .strip_prefix("default.")
        .or_else(|| name.strip_prefix("force."))
    {
        if let Some(info) = original_setting_var_info(rest) {
            return cast_value_for_type(info.var_type, value_str, info.select_keys.as_deref());
        }
    }

    if setting_info::is_known_default_arg_name(name) {
        return Ok(serde_yaml::Value::String(value_str.to_string()));
    }

    Err(format!("{} は不明な名前です", name))
}

pub fn coerce_json_setting_value(
    name: &str,
    value: &serde_json::Value,
) -> Result<serde_yaml::Value, String> {
    if let Some(info) = setting_info::setting_variables().get(name) {
        return coerce_value_for_type(info, value);
    }
    if let Some(base_name) = name
        .strip_prefix("default.")
        .or_else(|| name.strip_prefix("force."))
    {
        if let Some(info) = original_setting_var_info(base_name) {
            return coerce_value_for_type(&info, value);
        }
    }
    if setting_info::is_known_default_arg_name(name) {
        return coerce_string_value(value);
    }
    Err("不明な設定名です".to_string())
}

pub fn apply_device_related_settings(
    settings: &mut HashMap<String, serde_yaml::Value>,
) -> Option<DeviceRelatedSettingChanges> {
    let device = settings.get("device").and_then(|value| match value {
        serde_yaml::Value::String(s) => Some(s.to_string()),
        _ => None,
    })?;

    let device_key = device.to_ascii_lowercase();
    let device_display_name = match device_key.as_str() {
        "kindle" => "Kindle",
        "kobo" => "Kobo",
        "epub" => "EPUB",
        "ibunko" => "i文庫",
        "reader" => "SonyReader",
        "ibooks" => "iBooks",
        _ => return None,
    };

    let related = [(
        "default.enable_half_indent_bracket",
        serde_yaml::Value::Bool(device_key == "kindle"),
    )];

    let mut changes = Vec::new();
    for (name, new_value) in related {
        if settings.get(name) != Some(&new_value) {
            settings.insert(name.to_string(), new_value.clone());
            changes.push((name.to_string(), new_value));
        }
    }

    Some(DeviceRelatedSettingChanges {
        device_display_name,
        changes,
    })
}

pub fn yaml_value_display(v: &serde_yaml::Value) -> String {
    match v {
        serde_yaml::Value::Bool(b) => b.to_string(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Null => String::new(),
        other => format!("{:?}", other),
    }
}

pub fn var_type_description(vt: VarType) -> &'static str {
    match vt {
        VarType::Boolean => "true/false  ",
        VarType::Integer => "整数        ",
        VarType::Float => "小数点数    ",
        VarType::String | VarType::Select => "文字列      ",
        VarType::Directory => "フォルダパス",
        VarType::Multiple => "文字列(複数)",
    }
}

fn original_setting_var_info(name: &str) -> Option<VarInfo> {
    setting_info::original_setting_var_infos()
        .into_iter()
        .find(|(setting_name, _)| *setting_name == name)
        .map(|(_, info)| info)
}

fn cast_value_for_type(
    var_type: VarType,
    value_str: &str,
    select_keys: Option<&[String]>,
) -> Result<serde_yaml::Value, String> {
    match var_type {
        VarType::Select => {
            if let Some(keys) = select_keys {
                if !keys.iter().any(|k| k == value_str) {
                    return Err(format!(
                        "不明な値です。{} の中から指定して下さい",
                        keys.join(", ")
                    ));
                }
            }
            Ok(serde_yaml::Value::String(value_str.to_string()))
        }
        VarType::Multiple => {
            if let Some(keys) = select_keys {
                for part in value_str.split(',') {
                    if !keys.iter().any(|k| k == part) {
                        return Err(format!(
                            "不明な値です。{} の中から指定して下さい",
                            keys.join(", ")
                        ));
                    }
                }
            }
            Ok(serde_yaml::Value::String(value_str.to_string()))
        }
        VarType::Boolean => {
            let lower = value_str.trim().to_ascii_lowercase();
            match lower.as_str() {
                "true" => Ok(serde_yaml::Value::Bool(true)),
                "false" => Ok(serde_yaml::Value::Bool(false)),
                _ => Err(format!(
                    "値が {} ではありません",
                    var_type_description(VarType::Boolean).trim_end()
                )),
            }
        }
        VarType::Integer => value_str
            .parse::<i64>()
            .map(|i| serde_yaml::Value::Number(i.into()))
            .map_err(|_| {
                format!(
                    "値が {} ではありません",
                    var_type_description(VarType::Integer).trim_end()
                )
            }),
        VarType::Float => value_str
            .parse::<f64>()
            .map(|f| serde_yaml::Value::Number(serde_yaml::Number::from(f)))
            .map_err(|_| {
                format!(
                    "値が {} ではありません",
                    var_type_description(VarType::Float).trim_end()
                )
            }),
        VarType::String => Ok(serde_yaml::Value::String(value_str.to_string())),
        VarType::Directory => {
            let path = Path::new(value_str);
            if !path.is_dir() {
                return Err(format!(
                    "値が {} ではありません",
                    var_type_description(VarType::Directory).trim_end()
                ));
            }
            let expanded = fs::canonicalize(path).map_err(|_| {
                format!(
                    "値が {} ではありません",
                    var_type_description(VarType::Directory).trim_end()
                )
            })?;
            Ok(serde_yaml::Value::String(
                strip_extended_path_prefix(expanded)
                    .to_string_lossy()
                    .to_string(),
            ))
        }
    }
}

fn coerce_value_for_type(
    info: &VarInfo,
    value: &serde_json::Value,
) -> Result<serde_yaml::Value, String> {
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
        for part in raw
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
        {
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
    let canonical =
        fs::canonicalize(path).map_err(|_| "存在するフォルダを指定して下さい".to_string())?;
    Ok(serde_yaml::Value::String(
        strip_extended_path_prefix(canonical)
            .to_string_lossy()
            .to_string(),
    ))
}

fn strip_extended_path_prefix(path: std::path::PathBuf) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return std::path::PathBuf::from(rest);
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_dynamic_setting_names_are_validated() {
        assert!(matches!(
            setting_scope("default.enable_auto_indent"),
            Some(SettingScope::Local)
        ));
        assert!(matches!(
            setting_scope("force.enable_auto_indent"),
            Some(SettingScope::Local)
        ));
        assert!(matches!(
            setting_scope("default_args.trace"),
            Some(SettingScope::Local)
        ));
        assert!(matches!(
            setting_scope("default_args.console"),
            Some(SettingScope::Local)
        ));

        assert!(setting_scope("default.not_exists").is_none());
        assert!(setting_scope("force.not_exists").is_none());
        assert!(setting_scope("default_args.not_exists").is_none());
    }

    #[test]
    fn unknown_dynamic_values_are_rejected() {
        assert!(cast_setting_value("default_args.not_exists", "-n").is_err());
        assert!(cast_setting_value("default.not_exists", "true").is_err());
        assert!(cast_setting_value("webui.table.reload-timing", "invalid").is_err());
        assert!(cast_setting_value("webui.table.reload-timing", "every").is_ok());
        assert!(cast_setting_value("webui.theme", "unknown").is_err());
        assert!(cast_setting_value("webui.theme", "Cerulean").is_ok());
        assert!(cast_setting_value("default.title_date_align", "middle").is_err());
        assert!(cast_setting_value("default.title_date_align", "left").is_ok());
    }

    #[test]
    fn json_coercion_supports_web_input_shapes() {
        let value =
            coerce_json_setting_value("update.interval", &serde_json::json!("1.5")).unwrap();
        assert_eq!(
            value,
            serde_yaml::Value::Number(serde_yaml::Number::from(1.5))
        );

        let value = coerce_json_setting_value(
            "convert.copy-to-grouping",
            &serde_json::json!(["device", "site"]),
        )
        .unwrap();
        assert_eq!(value, serde_yaml::Value::String("device,site".to_string()));
    }

    #[test]
    fn json_coercion_rejects_unknown_select_value() {
        assert!(
            coerce_json_setting_value("webui.table.reload-timing", &serde_json::json!("invalid"))
                .is_err()
        );
    }

    #[test]
    fn device_related_settings_update_half_indent() {
        let mut settings = HashMap::from([(
            "device".to_string(),
            serde_yaml::Value::String("kobo".to_string()),
        )]);
        let changed = apply_device_related_settings(&mut settings).unwrap();

        assert_eq!(changed.device_display_name, "Kobo");
        assert_eq!(
            settings.get("default.enable_half_indent_bracket"),
            Some(&serde_yaml::Value::Bool(false))
        );
    }
}

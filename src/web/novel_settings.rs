use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};

use crate::db::with_database;
use crate::error::NarouError;

use super::AppState;
use super::state::{ApiResponse, IdPath};

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
        Ok(crate::db::existing_novel_dir_for_record(
            db.archive_root(),
            &record,
        ))
    })
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let settings = crate::converter::settings::NovelSettings::load_for_novel(
        id,
        &record.title,
        &record.author,
        &novel_dir,
    );

    let value = serde_json::to_value(&settings).unwrap_or_else(|_| serde_json::json!({}));
    Ok(Json(value))
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
        Ok(crate::db::existing_novel_dir_for_record(
            db.archive_root(),
            &record,
        ))
    })
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let ini_path = novel_dir.join("setting.ini");
    std::fs::create_dir_all(&novel_dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut ini = crate::converter::ini::IniData::load_file(&ini_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if let Some(obj) = body.as_object() {
        for (key, value) in obj {
            if key == "id"
                || key == "title"
                || key == "author"
                || key == "archive_path"
                || key == "replace_patterns"
            {
                continue;
            }
            let ini_value = match value {
                serde_json::Value::Bool(b) => crate::converter::ini::IniValue::Boolean(*b),
                serde_json::Value::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        crate::converter::ini::IniValue::Integer(i)
                    } else if let Some(f) = n.as_f64() {
                        crate::converter::ini::IniValue::Float(f)
                    } else {
                        continue;
                    }
                }
                serde_json::Value::String(s) => crate::converter::ini::IniValue::String(s.clone()),
                serde_json::Value::Null => crate::converter::ini::IniValue::Null,
                _ => continue,
            };
            ini.set_global(key, ini_value);
        }
    }

    ini.save(&ini_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

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

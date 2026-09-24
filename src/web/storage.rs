//! Storage backend of this library (`/api/storage/mode`).
//!
//! The settings page switches between the legacy YAML files and the SQLite
//! database here; the same `.narou/storage-backend` marker is what the CLI
//! and the downloader read. Rolling back to YAML writes the legacy bundle
//! first, so nothing is lost by the switch.

use axum::Json;

use crate::web::state::ApiResponse;

/// Current backend, for the settings page and the feature tour.
pub fn status_payload() -> serde_json::Value {
    #[cfg(feature = "native-runtime")]
    {
        use crate::native::sqlite::state;

        if state::legacy_yaml_active() {
            return serde_json::json!({
                "mode": "yaml",
                "locked_by_env": true,
                "reason": "NAROU_RS_LEGACY_YAML=1 が設定されています",
            });
        }
        let Ok(root) = crate::db::inventory::Inventory::with_default_root()
            .map(|inventory| inventory.root_dir().to_path_buf())
        else {
            return serde_json::json!({ "mode": "yaml", "locked_by_env": false });
        };
        let narou_dir = root.join(".narou");
        let mode = state::read_mode(&narou_dir);
        serde_json::json!({
            "mode": match mode {
                state::StorageMode::Sqlite => "sqlite",
                state::StorageMode::Yaml => "yaml",
            },
            "locked_by_env": false,
            "marker": narou_dir.join(state::MARKER_FILE).to_string_lossy(),
            "database": narou_dir.join("db.sqlite").to_string_lossy(),
            "database_exists": narou_dir.join("db.sqlite").exists(),
        })
    }
    #[cfg(not(feature = "native-runtime"))]
    {
        serde_json::json!({ "mode": "yaml", "locked_by_env": true })
    }
}

/// GET /api/storage/mode — which backend manages this library.
pub async fn storage_mode_get() -> Json<serde_json::Value> {
    let mut payload = status_payload();
    payload["success"] = serde_json::json!(true);
    Json(payload)
}

/// POST /api/storage/mode — switch the backend.
///
/// `sqlite` imports the legacy files on first use; `yaml` writes the legacy
/// bundle back (`narou db export-yaml --in-place` 相当) before switching, so
/// the library stays complete either way.
pub async fn storage_mode_set(Json(body): Json<serde_json::Value>) -> Json<ApiResponse> {
    #[cfg(feature = "native-runtime")]
    {
        use crate::native::sqlite::state;

        if state::legacy_yaml_active() {
            return Json(ApiResponse {
                success: false,
                message: "NAROU_RS_LEGACY_YAML=1 が設定されているため切り替えできません".to_string(),
            });
        }
        let requested = body["mode"].as_str().unwrap_or("").trim().to_string();
        let mode = match requested.as_str() {
            "sqlite" => state::StorageMode::Sqlite,
            "yaml" => state::StorageMode::Yaml,
            _ => {
                return Json(ApiResponse {
                    success: false,
                    message: "mode must be 'sqlite' or 'yaml'".to_string(),
                })
            }
        };
        let narou_dir = match crate::db::inventory::Inventory::with_default_root() {
            Ok(inventory) => inventory.root_dir().join(".narou"),
            Err(error) => {
                return Json(ApiResponse {
                    success: false,
                    message: error.to_string(),
                })
            }
        };
        // YAML へ戻すときは実位置へ書き出してから切り替える (`in_place` は
        // 書き出しと同時にマーカーも YAML へ戻すので、順序はこれで足りる)。
        if mode == state::StorageMode::Yaml
            && crate::native::sqlite::export_yaml::sqlite_active(&narou_dir)
            && let Err(error) = crate::native::sqlite::export_yaml::export_yaml(None, true)
        {
            return Json(ApiResponse {
                success: false,
                message: format!("YAML の書き出しに失敗しました: {error}"),
            });
        }
        if let Err(error) = state::write_mode(&narou_dir, mode) {
            return Json(ApiResponse {
                success: false,
                message: error.to_string(),
            });
        }
        return match crate::db::init_database() {
            Ok(()) => Json(ApiResponse {
                success: true,
                message: match mode {
                    state::StorageMode::Sqlite => {
                        "SQLite 管理へ移行しました (要再起動)".to_string()
                    }
                    state::StorageMode::Yaml => {
                        "YAML 管理へ戻しました (要再起動)".to_string()
                    }
                },
            }),
            Err(error) => Json(ApiResponse {
                success: false,
                message: error.to_string(),
            }),
        };
    }
    #[cfg(not(feature = "native-runtime"))]
    {
        let _ = body;
        Json(ApiResponse {
            success: false,
            message: "storage switching is unavailable in this build".to_string(),
        })
    }
}

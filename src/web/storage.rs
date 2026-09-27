//! Storage backend of this library (`/api/storage/mode`).
//!
//! The settings page switches between the legacy YAML files and the SQLite
//! database here; the same `.narou/storage-backend` marker is what the CLI
//! and the downloader read. Rolling back to YAML writes the legacy bundle
//! first, so nothing is lost by the switch.

use axum::extract::State;
use axum::Json;

use super::AppState;

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
///
/// `AppState` が保持する `Inventory` / `NativeStore` は構築時のストレージ
/// モードを保持したままなので、モードが実際に変わる場合はこのプロセスを
/// 再起動して差し替える (差し替え可能にすると読み書き経路が二重管理になる)。
pub async fn storage_mode_set(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    #[cfg(feature = "native-runtime")]
    {
        use crate::native::sqlite::state;
        let fail = |message: String| serde_json::json!({ "success": false, "message": message });

        if state::legacy_yaml_active() {
            return Json(fail(
                "NAROU_RS_LEGACY_YAML=1 が設定されているため切り替えできません".to_string(),
            ));
        }
        let requested = body["mode"].as_str().unwrap_or("").trim().to_string();
        let mode = match requested.as_str() {
            "sqlite" => state::StorageMode::Sqlite,
            "yaml" => state::StorageMode::Yaml,
            _ => return Json(fail("mode must be 'sqlite' or 'yaml'".to_string())),
        };
        let narou_dir = match crate::db::inventory::Inventory::with_default_root() {
            Ok(inventory) => inventory.root_dir().join(".narou"),
            Err(error) => return Json(fail(error.to_string())),
        };
        // YAML へ戻すときは実位置へ書き出してから切り替える (`in_place` は
        // 書き出しと同時にマーカーも YAML へ戻すので、順序はこれで足りる)。
        if mode == state::StorageMode::Yaml
            && crate::native::sqlite::export_yaml::sqlite_active(&narou_dir)
            && let Err(error) = crate::native::sqlite::export_yaml::export_yaml(None, true)
        {
            return Json(fail(format!("YAML の書き出しに失敗しました: {error}")));
        }
        let previous = state::read_mode(&narou_dir);
        if let Err(error) = state::write_mode(&narou_dir, mode) {
            return Json(fail(error.to_string()));
        }
        match crate::db::init_database() {
            Ok(()) => {
                // モードが変わった場合だけ再起動する。同一モードへの書き込み
                // (ツアーの「YAML を継続」など) は即座に完了でよい。
                if previous != mode {
                    return match super::jobs::schedule_server_reboot(state.clone()).await {
                        Ok(()) => {
                            state.push_server.broadcast_event("reboot", "");
                            Json(serde_json::json!({
                                "success": true,
                                "reboot": true,
                                "message": "管理バックエンドを切り替えました。サーバを再起動します",
                            }))
                        }
                        Err(error) => Json(serde_json::json!({
                            "success": true,
                            "reboot": false,
                            "message": format!("管理バックエンドを切り替えました。自動再起動に失敗したのでサーバを手動で再起動して下さい: {error}"),
                        })),
                    };
                }
                Json(serde_json::json!({
                    "success": true,
                    "reboot": false,
                    "message": match mode {
                        state::StorageMode::Sqlite => "SQLite管理へ移行しました",
                        state::StorageMode::Yaml => "YAML管理を継続します",
                    },
                }))
            }
            Err(error) => Json(fail(error.to_string())),
        }
    }
    #[cfg(not(feature = "native-runtime"))]
    {
        let _ = (state, body);
        Json(serde_json::json!({
            "success": false,
            "message": "storage switching is unavailable in this build",
        }))
    }
}

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
/// そのため順序が重要:
///
/// 1. 切り替え前のモードを先に読む (インプレース書き出しは自身でマーカーを
///    YAML へ戻すので、後から読むと「変化なし」に見えて再起動が漏れる)。
/// 2. 書き出しに失敗したらそのまま SQLite 管理を維持して失敗を返す。
/// 3. マーカーを書いてから再起動を予約する。予約できないときはマーカーを元へ
///    戻し、切り替えたと報告しない (このプロセスは古いモードの実装のまま
///    動き続けるため、実態とマーカーを食い違わせない)。
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

        // このプロセスが実際に動いているモード。書き出しより前に読むこと。
        let previous = state::read_mode(&narou_dir);

        if mode == state::StorageMode::Yaml && previous == state::StorageMode::Sqlite {
            // 実位置へ書き出してから切り替える。失敗したらマーカーを元へ戻し、
            // SQLite 管理のまま終わる (不完全なバンドルで YAML へ倒さない)。
            if let Err(error) =
                crate::native::sqlite::export_yaml::export_yaml_async(None, true).await
            {
                if let Err(restore) = state::write_mode(&narou_dir, previous) {
                    return Json(fail(format!(
                        "YAML の書き出しに失敗しました: {error} (ストレージモードを戻せませんでした: {restore})"
                    )));
                }
                return Json(fail(format!("YAML の書き出しに失敗しました: {error}")));
            }
        }

        // SQLite へ切り替えるときは、データベースを開けることを先に確かめる
        // (開けなければ再起動後のプロセスが YAML へ落ちる)。ここでは取り込み
        // (レガシーファイルの .imported- への改名) はしない: 再起動に失敗して
        // マーカーを戻したときに、レガシー状態を動かしたまま残さないため。
        if mode == state::StorageMode::Sqlite
            && previous != mode
            && let Err(error) = probe_sqlite_database(&narou_dir)
        {
            return Json(fail(format!(
                "SQLite 管理へ切り替えられません (データベースを開けませんでした): {error}"
            )));
        }

        match apply_mode(&narou_dir, previous, mode) {
            // モードが実際に変わったときだけ再起動してストアを組み直す。
            // 成功すればこのプロセスは終了し、同じ引数の子プロセスが新しい
            // モードで立ち上がる (取り込みもそちらで行う)。
            Ok(StorageSwitch::Changed(_)) => {
                match super::jobs::schedule_server_reboot(state.clone()).await {
                    Ok(()) => {
                        state.push_server.broadcast_event("reboot", "");
                        Json(serde_json::json!({
                            "success": true,
                            "reboot": true,
                            "message": "管理バックエンドを切り替えました。サーバを再起動します",
                        }))
                    }
                    Err(error) => {
                        // 再起動できないなら切り替えは完結しない。このプロセスは
                        // 古いモードのストアを持ったままなので、マーカーを元に
                        // 戻して「切り替えた」と報告しない。
                        let message = match state::write_mode(&narou_dir, previous) {
                            Ok(()) => format!(
                                "自動再起動に失敗したため切り替えを元に戻しました: {error}。サーバを手動で再起動してからもう一度お試し下さい"
                            ),
                            Err(restore) => format!(
                                "自動再起動に失敗し、切り替えも元に戻せませんでした: {error} / {restore}"
                            ),
                        };
                        Json(fail(message))
                    }
                }
            }
            // 同一モードへの書き込み (ツアーの「YAML を継続」など) は再起動
            // せずに即座に完了でよい。
            Ok(StorageSwitch::Continued(mode)) => {
                if mode == state::StorageMode::Sqlite
                    && let Err(error) = probe_sqlite_database(&narou_dir)
                {
                    return Json(fail(error));
                }
                match crate::db::init_database() {
                Ok(()) => Json(serde_json::json!({
                    "success": true,
                    "reboot": false,
                    "message": match mode {
                        state::StorageMode::Sqlite => "SQLite管理へ移行しました",
                        state::StorageMode::Yaml => "YAML管理を継続します",
                    },
                })),
                Err(error) => Json(fail(error.to_string())),
                }
            },
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

/// Pre-flight for a YAML→SQLite cutover.
///
/// The restarted process opens this database; when it cannot, `state::active_for`
/// silently falls back to the YAML files while the marker (and this API) claim
/// SQLite. Opening the file and switching it to WAL is exactly what
/// `state::configure` starts with, so a failure here means the cutover would
/// not take. The legacy import stays out of this probe: it renames
/// `.narou/*.yaml` to `.imported-*` and must only run once the restart is
/// certain.
fn probe_sqlite_database(narou_dir: &std::path::Path) -> std::result::Result<(), String> {
    std::fs::create_dir_all(narou_dir).map_err(|error| error.to_string())?;
    let connection = rusqlite::Connection::open(narou_dir.join("db.sqlite"))
        .map_err(|error| error.to_string())?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(|error| format!("journal_mode: {error}"))?;
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// What a `/api/storage/mode` request means for this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StorageSwitch {
    /// The requested mode is already active: no store rebuild, no restart.
    Continued(crate::native::sqlite::state::StorageMode),
    /// Real cutover: the marker changed, so only a restart can replace the
    /// stores this process was built with (or the marker must be rolled back).
    Changed(crate::native::sqlite::state::StorageMode),
}

/// Write the requested mode to the marker and report whether a restart is due.
///
/// `previous` must be the mode captured *before* anything else ran: an
/// in-place export flips the marker to YAML itself, so reading it afterwards
/// reported `Continued` for a real SQLite→YAML cutover and skipped the
/// restart, leaving the server serving from its SQLite stores.
fn apply_mode(
    narou_dir: &std::path::Path,
    previous: crate::native::sqlite::state::StorageMode,
    requested: crate::native::sqlite::state::StorageMode,
) -> crate::error::Result<StorageSwitch> {
    // 同一モードでも書く (ツアーの「YAML を継続」で明示的に選ばれる)。
    crate::native::sqlite::state::write_mode(narou_dir, requested)?;
    Ok(if previous == requested {
        StorageSwitch::Continued(requested)
    } else {
        StorageSwitch::Changed(requested)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::sqlite::state::{self, StorageMode};

    /// A real cutover in either direction must schedule a restart; a same-mode
    /// write (the feature tour's "continue with YAML") must not.
    #[test]
    fn mode_switch_reports_a_restart_only_for_a_real_cutover() {
        let temp = tempfile::tempdir().unwrap();
        let narou_dir = temp.path().join(".narou");

        // A library without a marker runs the legacy files.
        assert_eq!(state::read_mode(&narou_dir), StorageMode::Yaml);

        // YAML→SQLite: the captured mode differs, so the process restarts.
        let switch = apply_mode(&narou_dir, state::read_mode(&narou_dir), StorageMode::Sqlite)
            .expect("marker is writable");
        assert_eq!(switch, StorageSwitch::Changed(StorageMode::Sqlite));
        assert_eq!(state::read_mode(&narou_dir), StorageMode::Sqlite);

        // Re-selecting the active mode must not restart.
        let switch = apply_mode(&narou_dir, state::read_mode(&narou_dir), StorageMode::Sqlite)
            .expect("marker is writable");
        assert_eq!(switch, StorageSwitch::Continued(StorageMode::Sqlite));

        // SQLite→YAML: the in-place export rewrites the marker to `yaml`
        // before the decision runs, so only the mode captured *before* it
        // makes this a cutover (reading the marker afterwards is the bug that
        // skipped the restart).
        state::write_mode(&narou_dir, StorageMode::Yaml).unwrap();
        assert_eq!(state::read_mode(&narou_dir), StorageMode::Yaml);
        let switch = apply_mode(&narou_dir, StorageMode::Sqlite, StorageMode::Yaml)
            .expect("marker is writable");
        assert_eq!(switch, StorageSwitch::Changed(StorageMode::Yaml));
        let after_export = apply_mode(&narou_dir, state::read_mode(&narou_dir), StorageMode::Yaml)
            .expect("marker is writable");
        assert_eq!(after_export, StorageSwitch::Continued(StorageMode::Yaml));
    }
}

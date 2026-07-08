use axum::{Json, extract::State};
use serde::Serialize;
use std::collections::HashMap;

use crate::db::inventory::InventoryScope;
use crate::db::with_database;
use crate::version;

use super::AppState;
use super::state::ApiResponse;

const SEEN_VERSION_KEY: &str = "webui.feature-tour.seen-version";
const DISABLED_KEY: &str = "webui.feature-tour.disabled";

#[derive(Clone, Copy, Debug, Serialize)]
pub struct FeatureTourEntry {
    version: &'static str,
    title: &'static str,
    body: &'static str,
    items: &'static [&'static str],
}

const FEATURE_TOURS: &[FeatureTourEntry] = &[
    FeatureTourEntry {
        version: "0.2.0",
        title: "narou.rb から広がった Web UI",
        body: "Narou.rs の Web UI では、narou.rb 互換の管理データを使いながら、まとめて扱う操作が増えています。",
        items: &[
            "複数作品を選択した update / convert / tag / freeze / remove",
            "キュー管理と進捗表示",
            "タグ指定 update と最新話掲載日の確認",
        ],
    },
    FeatureTourEntry {
        version: "0.2.0",
        title: "シリーズ URL の一括登録",
        body: "作品 URL だけでなく、シリーズやコレクションの URL から個別作品を展開して登録できます。",
        items: &[
            "小説家になろうのシリーズ URL",
            "ノクターンなど R18 系のシリーズ URL",
            "カクヨムのコレクション URL",
        ],
    },
    FeatureTourEntry {
        version: "0.2.2",
        title: "更新後の自動変換とコピー設定",
        body: "更新後の convert と copy-to 周りの挙動を見直し、Web UI と CLI の update からの変換結果が揃うようにしています。",
        items: &[
            "convert.multi-device を update 後の自動変換でも反映",
            "EPUB 変換時の copy-to 出力を優先",
            "text-only 変換時に txt を EPUB 保存先へコピーしない",
        ],
    },
    FeatureTourEntry {
        version: "0.2.3",
        title: "新機能ツアー",
        body: "バージョンごとの追加機能を、必要な分だけ起動時に表示するようになりました。",
        items: &[
            "表示済みのツアー version を local_setting.yaml に保存",
            "次の更新では、未表示のツアーだけを表示",
            "ツアー項目が追加されないバージョンでは何も表示しない",
        ],
    },
    FeatureTourEntry {
        version: "0.3.0",
        title: "ハーメルン R18 作品に対応",
        body: "h.syosetu.org に分離されたハーメルンの R18 作品を、h あり・なしどちらの URL からでも登録・ダウンロードできます。",
        items: &[
            "h.syosetu.org と syosetu.org を同一サイトとして扱い、重複登録を防止",
            "R18 作品の取得時は h ドメインへの転送を自動で追跡",
            "R18 作品は over18(18歳以上確認)設定が有効な場合のみ取得",
        ],
    },
    FeatureTourEntry {
        version: "0.3.0",
        title: "新規タグの既定色設定",
        body: "新しく作られるタグに割り当てる色を webui.new-tag-color 設定で固定できます。",
        items: &[
            "設定画面の WebUI タブから色を選択",
            "default のままなら従来どおりタグの追加順に色を巡回",
            "Web UI と CLI のどちらでタグを追加しても同じ色を適用",
        ],
    },
    FeatureTourEntry {
        version: "0.3.0",
        title: "失敗したジョブの自動リトライ",
        body: "キューのジョブが一時的なエラーで失敗した場合、間隔を広げながら自動で再試行します。",
        items: &[
            "再試行回数は queue.max-retries 設定で調整(既定 3 回)",
            "再試行間隔は queue.retry-backoff 設定で調整(既定 1m,5m,15m)",
            "再試行の予定は queue_retry 通知として Web UI に配信",
        ],
    },
];

pub async fn pending(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let seen_version = load_seen_version();
    let disabled = load_disabled();
    let entries = pending_entries(seen_version.as_deref(), version::VERSION);
    let latest_pending_version = entries
        .iter()
        .map(|entry| entry.version)
        .max_by(|a, b| compare_versions(a, b))
        .unwrap_or("");

    Json(serde_json::json!({
        "success": true,
        "current_version": version::create_version_string(),
        "seen_version": seen_version,
        "disabled": disabled,
        "latest_pending_version": latest_pending_version,
        "entries": if disabled { Vec::new() } else { entries },
    }))
}

pub async fn all(State(_state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "success": true,
        "current_version": version::create_version_string(),
        "seen_version": load_seen_version(),
        "disabled": load_disabled(),
        "latest_pending_version": latest_tour_version(),
        "entries": current_entries(version::VERSION),
    }))
}

pub async fn mark_seen(
    State(_state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<ApiResponse> {
    let requested = body["version"].as_str().unwrap_or("").trim();
    if requested.is_empty() {
        return Json(ApiResponse {
            success: false,
            message: "version is required".to_string(),
        });
    }
    if !is_known_tour_version(requested) {
        return Json(ApiResponse {
            success: false,
            message: "unknown tour version".to_string(),
        });
    }

    let version_to_save = load_seen_version()
        .filter(|seen| version_greater(seen, requested))
        .unwrap_or_else(|| requested.to_string());

    match save_seen_version(&version_to_save) {
        Ok(()) => Json(ApiResponse {
            success: true,
            message: "OK".to_string(),
        }),
        Err(e) => Json(ApiResponse {
            success: false,
            message: e.to_string(),
        }),
    }
}

pub async fn configure(
    State(_state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<ApiResponse> {
    let disabled = body["disabled"].as_bool().unwrap_or(false);
    match save_disabled(disabled) {
        Ok(()) => Json(ApiResponse {
            success: true,
            message: "OK".to_string(),
        }),
        Err(e) => Json(ApiResponse {
            success: false,
            message: e.to_string(),
        }),
    }
}

fn load_seen_version() -> Option<String> {
    crate::compat::load_local_setting_string(SEEN_VERSION_KEY)
}

fn load_disabled() -> bool {
    crate::compat::load_local_setting_bool(DISABLED_KEY)
}

fn save_seen_version(version: &str) -> crate::error::Result<()> {
    update_local_settings(|settings| {
        settings.insert(
            SEEN_VERSION_KEY.to_string(),
            serde_yaml::Value::String(version.to_string()),
        );
    })
}

fn save_disabled(disabled: bool) -> crate::error::Result<()> {
    update_local_settings(|settings| {
        settings.insert(DISABLED_KEY.to_string(), serde_yaml::Value::Bool(disabled));
    })
}

fn update_local_settings(
    update: impl FnOnce(&mut HashMap<String, serde_yaml::Value>),
) -> crate::error::Result<()> {
    with_database(|db| {
        let inv = db.inventory();
        let mut settings: HashMap<String, serde_yaml::Value> = inv
            .load("local_setting", InventoryScope::Local)
            .unwrap_or_default();
        update(&mut settings);
        inv.save("local_setting", InventoryScope::Local, &settings)
    })
}

fn current_entries(current_version: &str) -> Vec<FeatureTourEntry> {
    FEATURE_TOURS
        .iter()
        .copied()
        .filter(|entry| !version_greater(entry.version, current_version))
        .collect()
}

fn pending_entries(seen_version: Option<&str>, current_version: &str) -> Vec<FeatureTourEntry> {
    FEATURE_TOURS
        .iter()
        .copied()
        .filter(|entry| {
            !version_greater(entry.version, current_version)
                && seen_version
                    .map(|seen| version_greater(entry.version, seen))
                    .unwrap_or(true)
        })
        .collect()
}

fn latest_tour_version() -> &'static str {
    FEATURE_TOURS
        .iter()
        .map(|entry| entry.version)
        .max_by(|a, b| compare_versions(a, b))
        .unwrap_or("")
}

fn is_known_tour_version(version: &str) -> bool {
    FEATURE_TOURS.iter().any(|entry| entry.version == version)
}

fn version_greater(left: &str, right: &str) -> bool {
    compare_versions(left, right).is_gt()
}

fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    normalize_version_parts(left).cmp(&normalize_version_parts(right))
}

fn normalize_version_parts(version: &str) -> [u64; 3] {
    let mut parts = [0, 0, 0];
    let normalized = version
        .trim()
        .trim_start_matches('v')
        .split_whitespace()
        .next()
        .unwrap_or("")
        .split(['-', '+'])
        .next()
        .unwrap_or("");
    for (index, part) in normalized.split('.').take(3).enumerate() {
        parts[index] = part.parse::<u64>().unwrap_or(0);
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::{normalize_version_parts, pending_entries, version_greater};

    #[test]
    fn version_comparison_handles_multi_digit_segments() {
        assert!(version_greater("0.10.0", "0.2.9"));
        assert!(version_greater("v0.2.3", "0.2.2"));
        assert!(!version_greater("0.2.3", "0.2.3"));
        assert_eq!(normalize_version_parts("0.2.3 (local-build)"), [0, 2, 3]);
    }

    #[test]
    fn pending_entries_returns_only_unseen_current_tours() {
        let entries = pending_entries(Some("0.2.0"), "0.2.3");
        assert!(
            entries
                .iter()
                .all(|entry| version_greater(entry.version, "0.2.0"))
        );
        assert!(entries.iter().any(|entry| entry.version == "0.2.3"));
    }

    #[test]
    fn pending_entries_does_not_return_future_tours() {
        let entries = pending_entries(None, "0.2.2");
        assert!(
            entries
                .iter()
                .all(|entry| !version_greater(entry.version, "0.2.2"))
        );
    }

    #[test]
    fn pending_entries_without_seen_version_returns_current_tours() {
        let entries = pending_entries(None, super::version::VERSION);
        assert!(!entries.is_empty());
        assert!(
            entries
                .iter()
                .all(|entry| !version_greater(entry.version, super::version::VERSION))
        );
    }
}

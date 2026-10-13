//! Web API の入出力ペイロード（native の Web サーバーと Worker で共有する）。
//!
//! `src/web/*` は native 専用ビルドに閉じているため、Worker 側ハンドラが同じ契約を
//! 満たすには型と規則を共有できる場所が必要になる。ここに置いた型と一覧の規則は
//! serde と可搬層の型だけに依存し、どちらのビルドからも使える。

use serde::{Deserialize, Serialize};

use super::library::LibrarySortColumn;

/// 保存・削除など「成否とメッセージ」だけを返す応答。
#[derive(Debug, Serialize)]
pub struct ApiResponse {
    pub success: bool,
    pub message: String,
}

/// `GET/POST /api/list` のパラメータ。
///
/// axum はクエリ文字列と `application/x-www-form-urlencoded` 本文を同じ形に
/// 展開するため、キー名はブラケット込みのリテラル (`search[value]` 等) になる。
#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    pub draw: Option<u64>,
    pub start: Option<u64>,
    pub length: Option<u64>,
    pub all: Option<bool>,
    pub filter: Option<String>,
    #[serde(rename = "search[value]")]
    pub search_value: Option<String>,
    #[serde(rename = "order[0][column]")]
    pub order_column: Option<u64>,
    #[serde(rename = "order[0][dir]")]
    pub order_dir: Option<String>,
}

/// DataTables 形式の一覧応答。
#[derive(Debug, Serialize)]
pub struct NovelListResponse {
    pub draw: u64,
    pub records_total: u64,
    pub records_filtered: u64,
    pub data: Vec<NovelListItem>,
}

/// 一覧 1 行分。日時は Unix epoch 秒で、`Option` は `null` になる。
#[derive(Debug, Serialize)]
pub struct NovelListItem {
    pub id: i64,
    pub title: String,
    pub author: String,
    pub sitename: String,
    pub novel_type: u8,
    pub end: bool,
    pub last_update: i64,
    pub general_lastup: Option<i64>,
    pub last_check_date: Option<i64>,
    pub new_arrivals_date: Option<i64>,
    pub tags: Vec<String>,
    pub new_arrivals: bool,
    pub frozen: bool,
    pub suspend: bool,
    pub length: Option<i64>,
    pub toc_url: String,
    /// Link to show (the page the novel was registered from). Falls back to
    /// `toc_url` inside the record before it reaches here.
    pub display_url: String,
    pub ncode: Option<String>,
    pub general_all_no: Option<i64>,
}

/// `/api/list` の `length` に許すページ長上限。
pub const MAX_WEB_PAGE_LENGTH: u64 = 500;

/// `/api/list` の `filter` + `search[value]` の合計バイト上限。
pub const MAX_WEB_SEARCH_BYTES: usize = 4096;

/// `/api/list` の `length` 未指定時に使う既定ページ長。
pub const DEFAULT_WEB_PAGE_LENGTH: u64 = 50;

/// `filter` (列フィルタ) と `search[value]` (全体検索) を空白区切りで AND 連結する。
/// 空文字列は未指定として扱い、どちらも空なら `None`。
pub fn combine_search_terms(filter: Option<String>, search: Option<String>) -> Option<String> {
    match (filter, search) {
        (Some(filter), Some(search)) if !filter.is_empty() && !search.is_empty() => {
            Some(format!("{filter} {search}"))
        }
        (Some(filter), _) if !filter.is_empty() => Some(filter),
        (_, Some(search)) if !search.is_empty() => Some(search),
        _ => None,
    }
}

/// DataTables の `order[0][column]` を [`LibrarySortColumn`] に写す。
/// 未指定や対応のない番号は `Id`。列番号は共有 `db::SORT_KEYS` と一致する。
pub fn sort_column_from_index(index: Option<u64>) -> LibrarySortColumn {
    match index.unwrap_or(0) {
        1 => LibrarySortColumn::LastUpdate,
        2 => LibrarySortColumn::GeneralLastup,
        3 => LibrarySortColumn::LastCheckDate,
        4 => LibrarySortColumn::Title,
        5 => LibrarySortColumn::Author,
        6 => LibrarySortColumn::SiteName,
        7 => LibrarySortColumn::NovelType,
        9 => LibrarySortColumn::GeneralAllNo,
        10 => LibrarySortColumn::Length,
        14 => LibrarySortColumn::AverageLength,
        _ => LibrarySortColumn::Id,
    }
}

/// `/api/version/current.json` と `/api/version/latest.json` のキー集合
/// (native `src/version.rs` と Worker `webui/read_views.rs` で共有)。
/// 値は各ランタイムの事実 (native は `crate::version::*`、Worker は固定値)。
#[derive(Debug, Clone)]
pub struct VersionInfoPayload {
    pub version: String,
    pub name: String,
    pub develop: bool,
    pub local_build: bool,
    pub container: bool,
    pub self_update_supported: bool,
    /// native `version::BUILD_VARIANT` / Worker は `"gpl"` 固定。
    pub build_variant: &'static str,
}

impl VersionInfoPayload {
    /// `GET /api/version/current.json` の本体。
    pub fn current(&self) -> serde_json::Value {
        serde_json::json!({
            "version": self.version,
            "name": self.name,
            "develop": self.develop,
            "local_build": self.local_build,
            "container": self.container,
            "self_update_supported": self.self_update_supported,
            "build_variant": self.build_variant,
        })
    }

    /// `GET /api/version/latest.json` の成功本体。
    /// `self_update_unavailable_reason` は `None` のとき `null` になる
    /// (native はローカルビルド判定の結果、Worker は常に固定文言)。
    pub fn latest(
        &self,
        latest_version: &str,
        update_available: bool,
        url: &str,
        self_update_unavailable_reason: Option<&str>,
        variant_choice_required: bool,
    ) -> serde_json::Value {
        serde_json::json!({
            "success": true,
            "current_version": self.version,
            "latest_version": latest_version,
            "update_available": update_available,
            "develop": self.develop,
            "local_build": self.local_build,
            "container": self.container,
            "self_update_supported": self.self_update_supported,
            "self_update_unavailable_reason": self_update_unavailable_reason,
            "build_variant": self.build_variant,
            "variant_choice_required": variant_choice_required,
            "url": url,
        })
    }

    /// `GET /api/version/latest.json` の取得失敗本体。
    pub fn latest_failure(&self, message: &str, url: &str) -> serde_json::Value {
        serde_json::json!({
            "success": false,
            "current_version": self.version,
            "message": message,
            "url": url,
        })
    }
}

/// 更新あり判定: 最新版が現行より新しいか。バージョンコアの数値比較で判定し、
/// 解釈できないときは「コアが異なれば更新あり」に倒す (native/Worker 同一)。
pub fn version_update_available(latest: &str, current: &str) -> bool {
    if latest.is_empty() {
        return false;
    }
    match crate::application::version_compare::version_compare(latest, current) {
        Some(ordering) => ordering == std::cmp::Ordering::Greater,
        None => latest != current,
    }
}

#[cfg(test)]
mod version_payload_tests {
    use super::*;

    fn native_info() -> VersionInfoPayload {
        VersionInfoPayload {
            version: "0.4.10".to_string(),
            name: "narou.rs".to_string(),
            develop: false,
            local_build: true,
            container: false,
            self_update_supported: false,
            build_variant: "standard",
        }
    }

    #[test]
    fn version_info_payload_has_the_native_key_set() {
        assert_eq!(
            native_info().current(),
            serde_json::json!({
                "version": "0.4.10",
                "name": "narou.rs",
                "develop": false,
                "local_build": true,
                "container": false,
                "self_update_supported": false,
                "build_variant": "standard",
            })
        );
    }

    #[test]
    fn version_latest_payload_carries_optional_reason_and_variant_choice() {
        let value = native_info().latest(
            "0.4.11",
            true,
            "https://example.com/releases",
            None,
            true,
        );
        assert_eq!(value["success"], true);
        assert_eq!(value["current_version"], "0.4.10");
        assert_eq!(value["latest_version"], "0.4.11");
        assert_eq!(value["update_available"], true);
        assert_eq!(value["self_update_unavailable_reason"], serde_json::Value::Null);
        assert_eq!(value["variant_choice_required"], true);
        assert_eq!(value["url"], "https://example.com/releases");

        assert_eq!(
            native_info().latest_failure("boom", "https://example.com/releases"),
            serde_json::json!({
                "success": false,
                "current_version": "0.4.10",
                "message": "boom",
                "url": "https://example.com/releases",
            })
        );
    }

    #[test]
    fn update_available_uses_numeric_comparison() {
        assert!(!version_update_available("", "0.4.10"));
        assert!(version_update_available("0.4.11", "0.4.10"));
        assert!(version_update_available("0.5", "0.4.10"));
        assert!(!version_update_available("0.4.10", "0.4.10"));
        assert!(!version_update_available("0.4.9", "0.4.10"));
        // 解釈できない場合はコアの不一致で判定する。
        assert!(version_update_available("develop", "0.4.10"));
    }
}

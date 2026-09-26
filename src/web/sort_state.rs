use serde_yaml::Value;

use crate::db::{
    NovelRecord,
    inventory::{Inventory, InventoryScope},
    sort_keys,
};

/// Web UI / CLI 共通のソートキー一覧。`db::SORT_KEYS` を再エクスポートして
/// 単一の真実源から派生させる。新規キーを追加するときは `db::SORT_KEYS` 側を
/// 編集するだけで `SORT_COLUMN_LABELS` の並びも合わせて調整すること。
pub use crate::db::SORT_KEYS as SORT_COLUMN_KEYS;

/// Web UI の表示ラベル。`SORT_COLUMN_KEYS` (≒ `db::sort_keys()`) と長さを揃え、
/// 同じインデックスで日本語ラベルを参照できるようにする。定義は Worker と共有する
/// [`crate::application::settings_view`] 側にある。
pub use crate::application::settings_view::{SORT_COLUMN_LABELS, sort_column_label_for_key};

// ソート状態の型・既定値・正規化・レコード比較は Worker でも同じ規則を使うため、
// 唯一の定義を可搬層 `crate::application::webui` に置き、ここでは再エクスポートする。
pub(crate) use crate::application::webui::{
    CurrentSortState, current_sort_from_server_setting, default_current_sort_state,
    normalize_current_sort_request, sort_column_key, sort_column_label, sort_records,
};
pub use crate::application::webui::sort_record_ordering;

pub(crate) fn load_current_sort_state() -> CurrentSortState {
    let sort_state = (|| {
        let inventory = Inventory::with_default_root().ok()?;
        let server_setting: Value = inventory.load("server_setting", InventoryScope::Global).ok()?;
        current_sort_from_server_setting(&server_setting)
    })();
    sort_state.unwrap_or_else(default_current_sort_state)
}

pub(crate) fn request_sort_state(
    _sort_state: Option<&serde_json::Value>,
    _timestamp: Option<u64>,
) -> Option<CurrentSortState> {
    // Single source of truth: never honor request-supplied sort state.
    None
}

pub(crate) fn request_preserves_input_order(
    _sort_state: Option<&serde_json::Value>,
    _timestamp: Option<u64>,
) -> bool {
    // Server-stored sort is always authoritative.
    false
}

pub(crate) fn requested_or_current_sort_state(
    _sort_state: Option<&serde_json::Value>,
    _timestamp: Option<u64>,
) -> CurrentSortState {
    load_current_sort_state()
}

pub fn normalize_sort_key(key: &str) -> Option<&'static str> {
    sort_keys().iter().copied().find(|candidate| *candidate == key)
}

pub(crate) fn sort_ids_from_records(
    ids: &[i64],
    records: &[NovelRecord],
    sort_state: Option<&serde_json::Value>,
    timestamp: Option<u64>,
) -> Vec<i64> {
    if request_preserves_input_order(sort_state, timestamp) {
        return ids.to_vec();
    }
    let sort_state = requested_or_current_sort_state(sort_state, timestamp);
    crate::application::webui::sort_ids_from_records(ids, records, &sort_state)
}

#[cfg(test)]
mod tests {
    use super::{
        CurrentSortState, current_sort_from_server_setting, default_current_sort_state,
        normalize_current_sort_request, normalize_sort_key, request_preserves_input_order,
        request_sort_state, sort_column_key, sort_column_label, sort_column_label_for_key,
        sort_record_ordering, sort_records,
    };
    // 既定値は可搬層が唯一の定義（native / Worker で同じ値を使う）。
    use crate::application::webui::{DEFAULT_CURRENT_SORT_COLUMN, DEFAULT_CURRENT_SORT_DIR};
    use crate::db::NovelRecord;
    use crate::web::sort_state::SORT_COLUMN_KEYS;
    use chrono::{TimeZone, Utc};
    use std::cmp::Ordering;

    fn sample_record(id: i64, last_check_ts: i64) -> NovelRecord {
        NovelRecord {
            id,
            author: format!("author-{id}"),
            title: format!("title-{id}"),
            file_title: format!("file-{id}"),
            toc_url: format!("https://example.com/{id}/"),
            sitename: "site".to_string(),
            novel_type: 1,
            end: false,
            last_update: Utc.timestamp_opt(1_700_000_000 + id, 0).unwrap(),
            new_arrivals_date: None,
            use_subdirectory: false,
            general_firstup: None,
            novelupdated_at: None,
            general_lastup: None,
            last_mail_date: None,
            tags: Vec::new(),
            ncode: None,
            domain: None,
            general_all_no: Some(id),
            length: Some(id),
            suspend: false,
            is_narou: true,
            last_check_date: Some(Utc.timestamp_opt(last_check_ts, 0).unwrap()),
            convert_failure: false,
            requires_login: false,
            login_session: None,
            extra_fields: Default::default(),
        }
    }

    #[test]
    fn current_sort_from_server_setting_accepts_integer_and_numeric_string_columns() {
        let numeric_server_setting: serde_yaml::Value =
            serde_yaml::from_str("current_sort:\n  column: 4\n  dir: desc\n").unwrap();
        let string_server_setting: serde_yaml::Value =
            serde_yaml::from_str("current_sort:\n  column: \"4\"\n  dir: desc\n").unwrap();

        let numeric_sort_state = current_sort_from_server_setting(&numeric_server_setting).unwrap();
        let string_sort_state = current_sort_from_server_setting(&string_server_setting).unwrap();

        assert_eq!(numeric_sort_state.column, 4);
        assert_eq!(numeric_sort_state.dir, "desc");
        assert_eq!(string_sort_state.column, 4);
        assert_eq!(string_sort_state.dir, "desc");
    }

    #[test]
    fn current_sort_request_is_normalized_to_integer_column() {
        let sort_state = normalize_current_sort_request(&serde_json::json!({
            "column": "2",
            "dir": "desc",
        }))
        .unwrap();

        assert_eq!(sort_state.column, 2);
        assert_eq!(sort_state.dir, "desc");
        assert!(
            normalize_current_sort_request(&serde_json::json!({
                "column": "title",
                "dir": "asc",
            }))
            .is_none()
        );
    }

    #[test]
    fn request_sort_state_is_always_ignored() {
        // Server-stored current_sort is the single source of truth; request
        // payloads (sort_state/timestamp) must never be honored.
        assert!(request_sort_state(
            Some(&serde_json::json!({ "column": 2, "dir": "desc" })),
            Some(123)
        )
        .is_none());
        assert!(request_sort_state(None, None).is_none());
    }

    #[test]
    fn request_never_preserves_input_order() {
        assert!(!request_preserves_input_order(None, Some(123)));
        assert!(!request_preserves_input_order(
            Some(&serde_json::json!({ "column": 2, "dir": "desc" })),
            Some(123)
        ));
        assert!(!request_preserves_input_order(None, None));
    }

    #[test]
    fn sort_records_supports_last_check_date_descending() {
        let first = sample_record(1, 1_700_000_100);
        let second = sample_record(2, 1_700_000_300);
        let third = sample_record(3, 1_700_000_200);
        let sort_state = CurrentSortState {
            column: 3, // last_check_date
            dir: "desc".to_string(),
        };
        let mut records = vec![first.clone(), second.clone(), third.clone()];

        sort_records(&mut records, &sort_state);

        assert_eq!(
            records.into_iter().map(|record| record.id).collect::<Vec<_>>(),
            vec![2, 3, 1]
        );
    }

    #[test]
    fn default_current_sort_matches_ruby_web_api() {
        let default_sort = default_current_sort_state();

        assert_eq!(default_sort.column, DEFAULT_CURRENT_SORT_COLUMN);
        assert_eq!(default_sort.dir, DEFAULT_CURRENT_SORT_DIR);
        // デフォルト (general_lastup desc) がインデックス 4 を指していること。
        assert_eq!(sort_column_key(&default_sort), Some("general_lastup"));
        assert_eq!(sort_column_label(&default_sort), Some("最新話掲載日"));
    }

    #[test]
    fn sort_record_ordering_supports_tags_status_and_url_columns() {
        let mut first = sample_record(1, 1_700_000_100);
        first.tags = vec!["zeta".to_string()];
        first.toc_url = "https://example.com/z".to_string();
        let mut second = sample_record(2, 1_700_000_200);
        second.tags = vec!["alpha".to_string()];
        second.end = true;
        second.toc_url = "https://example.com/a".to_string();

        assert_eq!(sort_record_ordering(&first, &second, "tags"), Ordering::Greater);
        assert_eq!(sort_record_ordering(&first, &second, "status"), Ordering::Less);
        assert_eq!(sort_record_ordering(&first, &second, "toc_url"), Ordering::Greater);
    }

    #[test]
    fn sort_column_keys_come_from_db_layer() {
        let _global = crate::test_support::global_state_guard();
        // SORT_COLUMN_KEYS は db::sort_keys() と同じ slice を参照する。
        assert_eq!(SORT_COLUMN_KEYS, crate::db::sort_keys());
        // `normalize_sort_key` も db::sort_keys() と同じ受理集合を持つ。
        for key in crate::db::sort_keys() {
            assert_eq!(normalize_sort_key(key), Some(*key));
        }
        assert_eq!(normalize_sort_key("not_a_key"), None);
    }

    #[test]
    fn sort_column_label_for_key_tracks_db_sort_keys() {
        // 既知キーはラベルが返る。
        assert_eq!(sort_column_label_for_key("id"), Some("ID"));
        assert_eq!(
            sort_column_label_for_key("new_arrivals_date"),
            Some("新着日")
        );
        // 未知キーは None。
        assert_eq!(sort_column_label_for_key("not_a_key"), None);
    }

    #[test]
    fn sort_records_breaks_ties_with_id_for_typed_keys() {
        // general_all_no / length / tags などで値が完全に一致しても、id で安定
        // 順序が決まることを保証する (BUG-9 で導入された安定フォールバック)。
        let mut first = sample_record(1, 1_700_000_100);
        first.general_all_no = Some(5);
        first.length = Some(5);
        first.tags.clear();
        let mut second = sample_record(2, 1_700_000_200);
        second.general_all_no = Some(5);
        second.length = Some(5);
        second.tags.clear();

        // column=9 は general_all_no (compare_optional 経由) だが、両方 Some(5) で
        // Equal になるため、id 安定順序 [1, 2] を期待する。
        let sort_state = CurrentSortState {
            column: 9, // general_all_no
            dir: "asc".to_string(),
        };
        let mut records = vec![second.clone(), first.clone()];
        sort_records(&mut records, &sort_state);
        assert_eq!(
            records.into_iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn sort_records_uses_compare_optional_for_missing_general_lastup() {
        // 全レコードが general_lastup=None でも id で安定順序になる。
        let first = sample_record(1, 1_700_000_100);
        let second = sample_record(2, 1_700_000_200);
        let sort_state = CurrentSortState {
            column: 2, // general_lastup
            dir: "asc".to_string(),
        };
        let mut records = vec![second.clone(), first.clone()];
        sort_records(&mut records, &sort_state);
        assert_eq!(
            records.into_iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }
}

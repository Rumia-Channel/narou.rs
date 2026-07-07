use std::cmp::Ordering;

use serde_yaml::{Mapping, Value};

use crate::db::{
    NovelRecord, compare_records_by_key, inventory::{Inventory, InventoryScope}, sort_keys,
    with_database,
};

/// Web UI / CLI 共通のソートキー一覧。`db::SORT_KEYS` を再エクスポートして
/// 単一の真実源から派生させる。新規キーを追加するときは `db::SORT_KEYS` 側を
/// 編集するだけで `SORT_COLUMN_LABELS` の並びも合わせて調整すること。
pub use crate::db::SORT_KEYS as SORT_COLUMN_KEYS;

/// Web UI の表示ラベル。`SORT_COLUMN_KEYS` (≒ `db::sort_keys()`) と長さを揃え、
/// 同じインデックスで日本語ラベルを参照できるようにする。
pub const SORT_COLUMN_LABELS: &[&str] = &[
    "ID",             // 0  id
    "最終更新日",     // 1  last_update
    "最新話掲載日",   // 2  general_lastup
    "最終確認日",     // 3  last_check_date
    "タイトル",       // 4  title
    "作者",           // 5  author
    "サイト名",       // 6  sitename
    "小説種別",       // 7  novel_type
    "タグ",           // 8  tags
    "話数",           // 9  general_all_no
    "文字数",         // 10 length
    "状態",           // 11 status
    "URL",            // 12 toc_url
    "新着日",         // 13 new_arrivals_date
];

pub(crate) const DEFAULT_CURRENT_SORT_COLUMN: usize = 2;
pub(crate) const DEFAULT_CURRENT_SORT_DIR: &str = "desc";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CurrentSortState {
    pub(crate) column: usize,
    pub(crate) dir: String,
}

impl CurrentSortState {
    pub(crate) fn to_json_value(&self) -> serde_json::Value {
        serde_json::json!({
            "column": self.column,
            "dir": self.dir,
        })
    }

    pub(crate) fn to_yaml_value(&self) -> Value {
        let mut mapping = Mapping::new();
        mapping.insert(
            Value::String("column".to_string()),
            serde_yaml::to_value(self.column).expect("serialize sort column"),
        );
        mapping.insert(
            Value::String("dir".to_string()),
            Value::String(self.dir.clone()),
        );
        Value::Mapping(mapping)
    }
}

pub(crate) fn default_current_sort_state() -> CurrentSortState {
    CurrentSortState {
        column: DEFAULT_CURRENT_SORT_COLUMN,
        dir: DEFAULT_CURRENT_SORT_DIR.to_string(),
    }
}

pub(crate) fn load_current_sort_state() -> CurrentSortState {
    let sort_state = (|| {
        let inventory = Inventory::with_default_root().ok()?;
        let server_setting: Value = inventory.load("server_setting", InventoryScope::Global).ok()?;
        current_sort_from_server_setting(&server_setting)
    })();
    sort_state.unwrap_or_else(default_current_sort_state)
}

pub(crate) fn current_sort_from_server_setting(server_setting: &Value) -> Option<CurrentSortState> {
    server_setting
        .as_mapping()?
        .get(Value::String("current_sort".to_string()))
        .and_then(normalize_current_sort_value)
}

pub(crate) fn normalize_current_sort_request(body: &serde_json::Value) -> Option<CurrentSortState> {
    let value = serde_yaml::to_value(body).ok()?;
    normalize_current_sort_value(&value)
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

pub(crate) fn sort_column_key(sort_state: &CurrentSortState) -> Option<&'static str> {
    sort_keys().get(sort_state.column).copied()
}

pub(crate) fn sort_column_label(sort_state: &CurrentSortState) -> Option<&'static str> {
    SORT_COLUMN_LABELS.get(sort_state.column).copied()
}

pub fn normalize_sort_key(key: &str) -> Option<&'static str> {
    sort_keys().iter().copied().find(|candidate| *candidate == key)
}

pub fn sort_column_label_for_key(key: &str) -> Option<&'static str> {
    let index = sort_keys().iter().position(|candidate| *candidate == key)?;
    SORT_COLUMN_LABELS.get(index).copied()
}

/// `db::compare_records_by_key` の薄いラッパ。CLI / Web 双方から共有される
/// 「ソートキー 1 個分の比較」であり、BUG-9 で導入された型付き + None 安定順の
/// セマンティクス (`compare_optional` を経由) をそのまま使う。
///
/// 未知のキーは `db::compare_records_by_key` と同じく id 比較へフォールバックする。
pub fn sort_record_ordering(a: &NovelRecord, b: &NovelRecord, sort_key: &str) -> Ordering {
    compare_records_by_key(a, b, sort_key)
}

pub(crate) fn sort_records(records: &mut Vec<&NovelRecord>, sort_state: &CurrentSortState) {
    let sort_key = sort_column_key(sort_state).unwrap_or("id");
    let reverse = sort_state.dir == "desc";
    records.sort_by(|a, b| {
        // 安定ソート: 主キーが Equal の場合は id で順序を決める。これにより
        // 同じ general_lastup / length / タグなどを共有するレコード間の順序が
        // db::sort_by と一致する。
        let ordering = sort_record_ordering(a, b, sort_key).then_with(|| a.id.cmp(&b.id));
        if reverse {
            ordering.reverse()
        } else {
            ordering
        }
    });
}

pub(crate) fn sort_ids_for_request(
    ids: &[i64],
    sort_state: Option<&serde_json::Value>,
    timestamp: Option<u64>,
) -> Vec<i64> {
    if request_preserves_input_order(sort_state, timestamp) {
        return ids.to_vec();
    }
    let sort_state = requested_or_current_sort_state(sort_state, timestamp);
    with_database(|db| {
        let mut records: Vec<_> = ids.iter().filter_map(|id| db.get(*id)).collect();
        sort_records(&mut records, &sort_state);
        Ok(records.into_iter().map(|record| record.id).collect())
    })
    .unwrap_or_else(|_| ids.to_vec())
}

fn normalize_current_sort_value(sort_state: &Value) -> Option<CurrentSortState> {
    let sort_state = sort_state.as_mapping()?;
    let column = sort_state
        .get(Value::String("column".to_string()))
        .or_else(|| sort_state.get(Value::String(":column".to_string())))
        .and_then(normalize_sort_column)?;
    let dir = sort_state
        .get(Value::String("dir".to_string()))
        .or_else(|| sort_state.get(Value::String(":dir".to_string())))
        .and_then(normalize_sort_dir)?;
    Some(CurrentSortState { column, dir })
}

fn normalize_sort_column(value: &Value) -> Option<usize> {
    let column = match value {
        Value::Number(number) => number.as_u64().map(|value| value as usize)?,
        Value::String(text) if text.chars().all(|ch| ch.is_ascii_digit()) => {
            text.parse::<usize>().ok()?
        }
        _ => return None,
    };
    sort_keys().get(column).map(|_| column)
}

fn normalize_sort_dir(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(text) => text.as_str(),
        _ => return None,
    };
    let text = text.trim_start_matches(':');
    match text {
        "asc" | "desc" => Some(text.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_CURRENT_SORT_COLUMN, DEFAULT_CURRENT_SORT_DIR, CurrentSortState,
        current_sort_from_server_setting, default_current_sort_state, normalize_current_sort_request,
        normalize_sort_key, request_preserves_input_order, request_sort_state, sort_column_key,
        sort_column_label, sort_column_label_for_key, sort_record_ordering, sort_records,
    };
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
        let mut records = vec![&first, &second, &third];

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
        let mut records = vec![&second, &first];
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
        let mut records = vec![&second, &first];
        sort_records(&mut records, &sort_state);
        assert_eq!(
            records.into_iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }
}

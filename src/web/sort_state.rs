use std::cmp::Ordering;

use serde_yaml::{Mapping, Value};

use crate::db::{NovelRecord, inventory::{Inventory, InventoryScope}, with_database};

pub const SORT_COLUMN_KEYS: &[&str] = &[
    "id",
    "last_update",
    "general_lastup",
    "last_check_date",
    "title",
    "author",
    "sitename",
    "novel_type",
    "tags",
    "general_all_no",
    "length",
    "status",
    "toc_url",
];

pub const SORT_COLUMN_LABELS: &[&str] = &[
    "ID",
    "最終更新日",
    "最新話掲載日",
    "最終確認日",
    "タイトル",
    "作者",
    "サイト名",
    "小説種別",
    "タグ",
    "話数",
    "文字数",
    "状態",
    "URL",
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
    SORT_COLUMN_KEYS.get(sort_state.column).copied()
}

pub(crate) fn sort_column_label(sort_state: &CurrentSortState) -> Option<&'static str> {
    SORT_COLUMN_LABELS.get(sort_state.column).copied()
}

pub fn normalize_sort_key(key: &str) -> Option<&'static str> {
    SORT_COLUMN_KEYS.iter().copied().find(|candidate| *candidate == key)
}

pub fn sort_column_label_for_key(key: &str) -> Option<&'static str> {
    let index = SORT_COLUMN_KEYS.iter().position(|candidate| *candidate == key)?;
    SORT_COLUMN_LABELS.get(index).copied()
}

pub fn sort_record_ordering(a: &NovelRecord, b: &NovelRecord, sort_key: &str) -> Ordering {
    match sort_key {
        "id" => a.id.cmp(&b.id),
        "title" => a.title.to_lowercase().cmp(&b.title.to_lowercase()),
        "author" => a.author.to_lowercase().cmp(&b.author.to_lowercase()),
        "last_update" => a.last_update.cmp(&b.last_update),
        "general_lastup" => a
            .general_lastup
            .unwrap_or_default()
            .cmp(&b.general_lastup.unwrap_or_default()),
        "last_check_date" => a
            .last_check_date
            .unwrap_or_default()
            .cmp(&b.last_check_date.unwrap_or_default()),
        "sitename" => a.sitename.cmp(&b.sitename),
        "novel_type" => a.novel_type.cmp(&b.novel_type),
        "tags" => record_tags_key(a).cmp(&record_tags_key(b)),
        "general_all_no" => a
            .general_all_no
            .unwrap_or(0)
            .cmp(&b.general_all_no.unwrap_or(0)),
        "length" => a.length.unwrap_or(0).cmp(&b.length.unwrap_or(0)),
        "status" => record_status_key(a).cmp(&record_status_key(b)),
        "toc_url" => a.toc_url.cmp(&b.toc_url),
        _ => a.id.cmp(&b.id),
    }
}

pub(crate) fn sort_records(records: &mut Vec<&NovelRecord>, sort_state: &CurrentSortState) {
    let sort_key = sort_column_key(sort_state).unwrap_or("id");
    let reverse = sort_state.dir == "desc";
    records.sort_by(|a, b| {
        let ordering = sort_record_ordering(a, b, sort_key);
        if reverse { ordering.reverse() } else { ordering }
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
    SORT_COLUMN_KEYS.get(column).map(|_| column)
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

fn record_tags_key(record: &NovelRecord) -> String {
    record
        .tags
        .iter()
        .map(|tag| tag.to_lowercase())
        .collect::<Vec<_>>()
        .join("\u{0}")
}

fn record_status_key(record: &NovelRecord) -> String {
    let mut status = Vec::new();
    if record.tags.iter().any(|tag| tag == "end") || record.end {
        status.push("完結");
    }
    if record.tags.iter().any(|tag| tag == "404") {
        status.push("削除");
    }
    if record.suspend {
        status.push("中断");
    }
    status.join(", ").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_CURRENT_SORT_COLUMN, DEFAULT_CURRENT_SORT_DIR, CurrentSortState,
        current_sort_from_server_setting, default_current_sort_state, normalize_current_sort_request,
        request_preserves_input_order, request_sort_state, sort_record_ordering, sort_records,
    };
    use crate::db::NovelRecord;
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
            column: 3,
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
}

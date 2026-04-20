use serde_yaml::{Mapping, Value};

pub(crate) const SORT_COLUMN_KEYS: &[&str] = &[
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

pub(crate) const SORT_COLUMN_LABELS: &[&str] = &[
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

pub(crate) fn sort_column_key(sort_state: &CurrentSortState) -> Option<&'static str> {
    SORT_COLUMN_KEYS.get(sort_state.column).copied()
}

pub(crate) fn sort_column_label(sort_state: &CurrentSortState) -> Option<&'static str> {
    SORT_COLUMN_LABELS.get(sort_state.column).copied()
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

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_CURRENT_SORT_COLUMN, DEFAULT_CURRENT_SORT_DIR, current_sort_from_server_setting,
        default_current_sort_state, normalize_current_sort_request,
    };

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
    fn default_current_sort_matches_ruby_web_api() {
        let default_sort = default_current_sort_state();

        assert_eq!(default_sort.column, DEFAULT_CURRENT_SORT_COLUMN);
        assert_eq!(default_sort.dir, DEFAULT_CURRENT_SORT_DIR);
    }
}

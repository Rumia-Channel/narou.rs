//! Record ↔ row mapping, ported from `worker_entry/src/d1_repository.rs`
//! (`record_binds` / `NovelRow::into_record` / fold & time helpers). Keep the
//! two in sync until the shared-SQL extraction lands.

use std::collections::BTreeMap;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::types::Value;

use crate::db::NovelRecord;
use crate::error::{NarouError, Result};

/// Serialized extra fields must stay bounded (mirrors D1 limit).
pub(crate) const MAX_EXTRA_FIELDS_BYTES: usize = 64 * 1024;

pub(crate) fn fold(value: &str) -> String {
    value.trim().to_lowercase()
}

pub(crate) fn format_time(value: Option<DateTime<Utc>>) -> Option<String> {
    value.map(|value| value.to_rfc3339_opts(SecondsFormat::Nanos, true))
}

pub(crate) fn parse_time(value: String) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| NarouError::Platform(format!("invalid sqlite timestamp: {error}")))
}

pub(crate) fn parse_optional_time(value: Option<String>) -> Result<Option<DateTime<Utc>>> {
    match value {
        // The UPSERT writes optional dates as plain parameters; an absent
        // timestamp is stored as an empty string, not SQL NULL.
        Some(text) if text.is_empty() => Ok(None),
        other => other.map(parse_time).transpose(),
    }
}

pub(crate) fn parse_extra_fields(value: &str) -> Result<BTreeMap<String, serde_yaml::Value>> {
    if value.len() > MAX_EXTRA_FIELDS_BYTES {
        return Err(NarouError::Platform(
            "sqlite extra fields payload exceeds limit".to_string(),
        ));
    }
    let yaml: serde_yaml::Value = serde_yaml::from_str(value)
        .map_err(|error| NarouError::Platform(format!("invalid sqlite extra fields YAML: {error}")))?;
    let serde_yaml::Value::Mapping(mapping) = yaml else {
        return Err(NarouError::Platform(
            "sqlite extra fields must be a mapping".to_string(),
        ));
    };
    Ok(mapping
        .into_iter()
        .filter_map(|(key, value)| key.as_str().map(|key| (key.to_string(), value)))
        .collect())
}

pub(crate) struct RecordParams {
    pub values: Vec<Value>,
}

impl RecordParams {
    pub fn extra_fields_len(&self) -> usize {
        // Last two binds are extra_fields_yaml + its byte length.
        match self.values.as_slice() {
            [.., Value::Text(yaml), Value::Integer(_)] => yaml.len(),
            _ => 0,
        }
    }
}

fn text(value: impl Into<String>) -> Value {
    Value::Text(value.into())
}

fn opt_text(value: Option<String>) -> Value {
    // UPSERT uses NULLIF(?, '') for these columns; empty string means NULL.
    Value::Text(value.unwrap_or_default())
}

fn opt_int(value: Option<i64>) -> Value {
    // UPSERT uses NULLIF(?, -1); -1 means NULL.
    Value::Integer(value.unwrap_or(-1))
}

/// Encode a record into positional SQL parameters matching
/// `query.rs::UPSERT_SQL`.
pub(crate) fn record_params(record: &NovelRecord) -> Result<RecordParams> {
    let tags_json = serde_json::to_string(&record.tags)
        .map_err(|error| NarouError::Platform(format!("cannot serialize tags: {error}")))?;
    let tags_fold = record
        .tags
        .iter()
        .map(|tag| fold(tag))
        .collect::<Vec<_>>()
        .join("\n");
    let tags_sort = record
        .tags
        .iter()
        .map(|tag| fold(tag))
        .collect::<Vec<_>>()
        .join("\u{1f}");
    let extra_fields_yaml = serde_yaml::to_string(&record.extra_fields)
        .map_err(|error| NarouError::Platform(format!("cannot serialize extra fields: {error}")))?;
    let extra_fields_len = extra_fields_yaml.len();
    if extra_fields_len > MAX_EXTRA_FIELDS_BYTES {
        return Err(NarouError::Platform(
            "extra fields payload exceeds limit".to_string(),
        ));
    }

    Ok(RecordParams {
        values: vec![
            Value::Integer(record.id),
            text(record.author.clone()),
            text(fold(&record.author)),
            text(record.title.clone()),
            text(fold(&record.title)),
            text(record.file_title.clone()),
            text(record.toc_url.clone()),
            text(fold(&record.toc_url)),
            text(record.sitename.clone()),
            text(fold(&record.sitename)),
            Value::Integer(i64::from(record.novel_type)),
            Value::Integer(record.end as i64),
            text(format_time(Some(record.last_update)).unwrap_or_default()),
            opt_text(format_time(record.new_arrivals_date)),
            Value::Integer(record.use_subdirectory as i64),
            opt_text(format_time(record.general_firstup)),
            opt_text(format_time(record.novelupdated_at)),
            opt_text(format_time(record.general_lastup)),
            opt_text(format_time(record.last_mail_date)),
            text(tags_json),
            text(tags_fold),
            text(tags_sort),
            opt_text(record.ncode.clone()),
            opt_text(record.ncode.as_deref().map(fold)),
            opt_text(record.domain.clone()),
            opt_text(record.domain.as_deref().map(fold)),
            opt_int(record.general_all_no),
            opt_int(record.length),
            Value::Integer(record.suspend as i64),
            Value::Integer(record.is_narou as i64),
            opt_text(format_time(record.last_check_date)),
            Value::Integer(record.convert_failure as i64),
            text(extra_fields_yaml),
            Value::Integer(extra_fields_len as i64),
        ],
    })
}

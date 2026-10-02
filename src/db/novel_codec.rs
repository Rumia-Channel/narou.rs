//! Shared `NovelRecord` ↔ SQL codec used by both storage drivers
//! (`src/native/sqlite` on rusqlite, `worker_entry/src/d1_repository.rs` on
//! D1). Column names, ordering and value conventions live here exactly once;
//! each driver only converts [`NovelBind`]s to its own bind type.
//!
//! History: the two drivers used to keep positional bind lists in sync with
//! `UPSERT_SQL` by hand. The Worker list drifted and `login_session`'s NULL
//! landed in the NOT NULL `extra_fields_bytes` column, breaking every novel
//! upsert. [`novel_binds`] therefore returns *named* values and
//! [`ordered_binds`] derives the positional order from the SQL itself, so a
//! wrong order is structurally impossible and a wrong name set is a loud
//! error instead of silent column skew.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, SecondsFormat, Utc};

use crate::db::NovelRecord;
use crate::error::{NarouError, Result};

/// The UPSERT statement shared by both drivers. The literal lives in a `.sql`
/// file so editors/tooling keep working on it; `NOVEL_COLUMNS` is pinned to
/// its column list by tests.
pub const NOVEL_UPSERT_SQL: &str = include_str!("sql/upsert.sql");

/// The SELECT statement shared by both drivers. Its column order is
/// `NOVEL_SELECT_COLUMNS`; both are pinned to each other by tests.
pub const NOVEL_SELECT_SQL: &str = "SELECT n.id, n.author, n.author_fold, n.title, n.file_title, n.toc_url, n.sitename, n.novel_type, n.end, n.last_update, n.new_arrivals_date, n.use_subdirectory, n.general_firstup, n.novelupdated_at, n.general_lastup, n.last_mail_date, n.tags_json, n.ncode, n.domain, n.general_all_no, n.length, n.suspend, n.is_narou, n.last_check_date, n.convert_failure, n.extra_fields_yaml, n.requires_login, n.login_session FROM novels n";

/// Columns of the `novels` UPSERT, in the order `NOVEL_UPSERT_SQL` declares
/// them. This is the single source of truth both drivers' bind lists used to
/// duplicate (and drift from).
pub const NOVEL_COLUMNS: &[&str] = &[
    "id",
    "author",
    "author_fold",
    "title",
    "title_fold",
    "file_title",
    "toc_url",
    "toc_url_fold",
    "sitename",
    "sitename_fold",
    "novel_type",
    "end",
    "last_update",
    "new_arrivals_date",
    "use_subdirectory",
    "general_firstup",
    "novelupdated_at",
    "general_lastup",
    "last_mail_date",
    "tags_json",
    "tags_fold",
    "tags_sort",
    "ncode",
    "ncode_fold",
    "domain",
    "domain_fold",
    "general_all_no",
    "length",
    "suspend",
    "is_narou",
    "last_check_date",
    "convert_failure",
    "extra_fields_yaml",
    "extra_fields_bytes",
    "requires_login",
    "login_session",
];

/// Columns `NOVEL_SELECT_SQL` returns, in row order. Drivers map these names
/// to row positions instead of hand-maintained indices.
pub const NOVEL_SELECT_COLUMNS: &[&str] = &[
    "id",
    "author",
    "author_fold",
    "title",
    "file_title",
    "toc_url",
    "sitename",
    "novel_type",
    "end",
    "last_update",
    "new_arrivals_date",
    "use_subdirectory",
    "general_firstup",
    "novelupdated_at",
    "general_lastup",
    "last_mail_date",
    "tags_json",
    "ncode",
    "domain",
    "general_all_no",
    "length",
    "suspend",
    "is_narou",
    "last_check_date",
    "convert_failure",
    "extra_fields_yaml",
    "requires_login",
    "login_session",
];

/// Bound on serialized `extra_fields` enforced by the native store. D1
/// tolerates much larger payloads and keeps its own (larger) limit in
/// `worker_entry/src/d1_repository.rs`.
pub const MAX_EXTRA_FIELDS_BYTES: usize = 64 * 1024;

/// One column value in a `novels` row, for both encoding (bind parameter) and
/// decoding (cell value). Drivers convert this to their native types
/// (`rusqlite::types::Value`, the Worker's `JsValue` mapping).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NovelBind {
    Text(String),
    Int(i64),
    Null,
}

/// Folded form used for `*_fold` columns and tag indexes.
pub fn fold(value: &str) -> String {
    value.trim().to_lowercase()
}

/// Drop duplicate tags in place, preserving the order of first occurrence.
///
/// Registration paths can merge default tags with retained ones, so a record
/// may carry the same name twice (`["favorite","end","favorite","end"]`).
/// `novel_tags` enforces `UNIQUE (novel_id, tag)` and `tags_json`/`tags_fold`/
/// `tags_sort` must agree with the indexed rows, so both storage drivers
/// normalize `record.tags` through this before writing either
/// representation.
pub fn dedup_tags(tags: &mut Vec<String>) {
    let mut seen = HashSet::new();
    tags.retain(|tag| seen.insert(tag.clone()));
}

/// RFC 3339 with nanoseconds — the timestamp format every column stores.
pub fn format_time(value: Option<DateTime<Utc>>) -> Option<String> {
    value.map(|value| value.to_rfc3339_opts(SecondsFormat::Nanos, true))
}

pub fn parse_time(value: String) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| NarouError::Platform(format!("invalid stored timestamp: {error}")))
}

pub fn parse_optional_time(value: Option<String>) -> Result<Option<DateTime<Utc>>> {
    match value {
        // The UPSERT writes optional dates as plain parameters; an absent
        // timestamp is stored as an empty string, not SQL NULL.
        Some(text) if text.is_empty() => Ok(None),
        other => other.map(parse_time).transpose(),
    }
}

/// Encode a record into named column values. The returned order is
/// `NOVEL_COLUMNS`; callers must not rely on it and go through
/// [`ordered_binds`], which re-derives the order from the actual SQL.
///
/// NULL conventions encoded here:
/// - `ncode`, `domain`, `general_all_no`, `length` and the optional dates are
///   written as `""`/`-1` because the UPSERT wraps them in `NULLIF(?, '')` /
///   `NULLIF(?, -1)`.
/// - `login_session` is written as NULL; the native adapter rewrites it to
///   `""` to preserve what its historical `opt_text` stored.
///
/// This function does not enforce a size cap on `extra_fields`; the cap
/// differs per backend — call [`check_extra_fields_limit`] with the driver's
/// limit.
pub fn novel_binds(record: &NovelRecord) -> Result<Vec<(&'static str, NovelBind)>> {
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
    let extra_fields_len = extra_fields_yaml.len() as i64;

    // NULLIF columns store "" / -1 for None instead of NULL.
    let nullif_text = |value: Option<String>| NovelBind::Text(value.unwrap_or_default());
    let nullif_int = |value: Option<i64>| NovelBind::Int(value.unwrap_or(-1));

    Ok(vec![
        ("id", NovelBind::Int(record.id)),
        ("author", NovelBind::Text(record.author.clone())),
        ("author_fold", NovelBind::Text(fold(&record.author))),
        ("title", NovelBind::Text(record.title.clone())),
        ("title_fold", NovelBind::Text(fold(&record.title))),
        ("file_title", NovelBind::Text(record.file_title.clone())),
        ("toc_url", NovelBind::Text(record.toc_url.clone())),
        ("toc_url_fold", NovelBind::Text(fold(&record.toc_url))),
        ("sitename", NovelBind::Text(record.sitename.clone())),
        ("sitename_fold", NovelBind::Text(fold(&record.sitename))),
        ("novel_type", NovelBind::Int(i64::from(record.novel_type))),
        ("end", NovelBind::Int(record.end as i64)),
        (
            "last_update",
            NovelBind::Text(format_time(Some(record.last_update)).unwrap_or_default()),
        ),
        ("new_arrivals_date", nullif_text(format_time(record.new_arrivals_date))),
        ("use_subdirectory", NovelBind::Int(record.use_subdirectory as i64)),
        ("general_firstup", nullif_text(format_time(record.general_firstup))),
        ("novelupdated_at", nullif_text(format_time(record.novelupdated_at))),
        ("general_lastup", nullif_text(format_time(record.general_lastup))),
        ("last_mail_date", nullif_text(format_time(record.last_mail_date))),
        ("tags_json", NovelBind::Text(tags_json)),
        ("tags_fold", NovelBind::Text(tags_fold)),
        ("tags_sort", NovelBind::Text(tags_sort)),
        ("ncode", nullif_text(record.ncode.clone())),
        ("ncode_fold", nullif_text(record.ncode.as_deref().map(fold))),
        ("domain", nullif_text(record.domain.clone())),
        ("domain_fold", nullif_text(record.domain.as_deref().map(fold))),
        ("general_all_no", nullif_int(record.general_all_no)),
        ("length", nullif_int(record.length)),
        ("suspend", NovelBind::Int(record.suspend as i64)),
        ("is_narou", NovelBind::Int(record.is_narou as i64)),
        ("last_check_date", nullif_text(format_time(record.last_check_date))),
        ("convert_failure", NovelBind::Int(record.convert_failure as i64)),
        ("extra_fields_yaml", NovelBind::Text(extra_fields_yaml)),
        ("extra_fields_bytes", NovelBind::Int(extra_fields_len)),
        ("requires_login", NovelBind::Int(record.requires_login as i64)),
        ("login_session", match &record.login_session {
            Some(session) => NovelBind::Text(session.clone()),
            None => NovelBind::Null,
        }),
    ])
}

/// Reject records whose serialized `extra_fields` exceed the driver's cap.
/// `extra_fields_bytes` already carries the serialized length, so this reads
/// the value the row will store rather than re-serializing.
pub fn check_extra_fields_limit(
    binds: &[(&'static str, NovelBind)],
    max_bytes: usize,
) -> Result<()> {
    let Some((_, NovelBind::Int(bytes))) =
        binds.iter().find(|(name, _)| *name == "extra_fields_bytes")
    else {
        return Err(NarouError::Database(
            "novel binds lack extra_fields_bytes".to_string(),
        ));
    };
    if *bytes as usize > max_bytes {
        return Err(NarouError::Platform(
            "extra fields payload exceeds limit".to_string(),
        ));
    }
    Ok(())
}

/// Extract the column list from `INSERT INTO novels (<columns>) VALUES …`.
/// The list is parenthesis-balanced so `NULLIF(?, '')` inside VALUES cannot
/// confuse the scan.
pub fn upsert_columns(sql: &str) -> Result<Vec<&str>> {
    upsert_column_list(sql).map(|(columns, _)| columns)
}

fn upsert_column_list(sql: &str) -> Result<(Vec<&str>, usize)> {
    const MARKER: &str = "INSERT INTO novels (";
    let start = sql
        .find(MARKER)
        .ok_or_else(|| {
            NarouError::Database(format!("upsert SQL lacks '{MARKER}'"))
        })?
        + MARKER.len();
    let mut depth = 0usize;
    let mut end = None;
    for (offset, byte) in sql.as_bytes()[start..].iter().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => {
                if depth == 0 {
                    end = Some(start + offset);
                    break;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    let end = end.ok_or_else(|| {
        NarouError::Database("upsert column list is unterminated".to_string())
    })?;
    let columns: Vec<&str> = sql[start..end]
        .split(',')
        .map(|name| name.trim())
        .collect();
    if columns.iter().any(|name| !is_column_name(name)) {
        return Err(NarouError::Database(
            "cannot parse the upsert column list".to_string(),
        ));
    }
    Ok((columns, end))
}

fn is_column_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// Order `binds` to match the `INSERT INTO novels (…)` column list of `sql`
/// and verify the VALUES clause has one `?` marker per column. Any mismatch —
/// a missing, duplicated or unexpected bind name, or a column/marker count
/// mismatch — is an error, so column skew fails loudly instead of shifting
/// values into neighbouring columns.
///
/// The pairs are returned (not bare values) so a driver can still apply
/// name-specific conversions such as the native `login_session` "" write.
pub fn ordered_binds<'a>(
    sql: &str,
    binds: &'a [(&'static str, NovelBind)],
) -> Result<Vec<(&'static str, &'a NovelBind)>> {
    let (columns, columns_end) = upsert_column_list(sql)?;
    let markers = sql[columns_end..]
        .split("VALUES")
        .nth(1)
        .ok_or_else(|| {
            NarouError::Database("upsert SQL lacks a VALUES clause".to_string())
        })?
        .matches('?')
        .count();
    if markers != columns.len() {
        return Err(NarouError::Database(format!(
            "upsert SQL binds {markers} markers for {} columns",
            columns.len()
        )));
    }
    let mut ordered = Vec::with_capacity(columns.len());
    for column in columns {
        let mut matching = binds.iter().filter(|(name, _)| *name == column);
        let Some(&(name, ref value)) = matching.next() else {
            return Err(NarouError::Database(format!(
                "novel binds lack column {column}"
            )));
        };
        if matching.next().is_some() {
            return Err(NarouError::Database(format!(
                "novel binds contain {column} twice"
            )));
        }
        ordered.push((name, value));
    }
    for (name, _) in binds {
        if !ordered.iter().any(|(column, _)| *column == *name) {
            return Err(NarouError::Database(format!(
                "novel binds contain unexpected column {name}"
            )));
        }
    }
    Ok(ordered)
}

/// Parse the `SELECT <columns> FROM novels n` list into bare column names
/// (table prefixes stripped). Drivers and tests pin it to
/// `NOVEL_SELECT_COLUMNS`.
pub fn select_columns(sql: &str) -> Result<Vec<&str>> {
    let body = sql.strip_prefix("SELECT ").ok_or_else(|| {
        NarouError::Database("select SQL lacks a SELECT prefix".to_string())
    })?;
    let end = body.find(" FROM ").ok_or_else(|| {
        NarouError::Database("select SQL lacks a FROM clause".to_string())
    })?;
    let columns: Vec<&str> = body[..end]
        .split(',')
        .map(|item| item.trim().rsplit('.').next().unwrap_or(""))
        .collect();
    if columns.iter().any(|name| !is_column_name(name)) {
        return Err(NarouError::Database(
            "cannot parse the select column list".to_string(),
        ));
    }
    Ok(columns)
}

/// Extract the string-keyed mapping from parsed `extra_fields` YAML. Kept
/// separate from [`parse_extra_fields`] so the Worker can apply its legacy
/// JSON fallback with its own error context first.
pub fn extra_fields_mapping(
    yaml: serde_yaml::Value,
) -> Result<BTreeMap<String, serde_yaml::Value>> {
    let serde_yaml::Value::Mapping(mapping) = yaml else {
        return Err(NarouError::Platform(
            "extra fields must be a mapping".to_string(),
        ));
    };
    Ok(mapping
        .into_iter()
        .filter_map(|(key, value)| key.as_str().map(|key| (key.to_string(), value)))
        .collect())
}

/// Strict YAML parse of the `extra_fields_yaml` column (native convention:
/// [`MAX_EXTRA_FIELDS_BYTES`] cap, no legacy-JSON fallback — the Worker keeps
/// its own wrapper for that).
pub fn parse_extra_fields(value: &str) -> Result<BTreeMap<String, serde_yaml::Value>> {
    if value.len() > MAX_EXTRA_FIELDS_BYTES {
        return Err(NarouError::Platform(
            "extra fields payload exceeds limit".to_string(),
        ));
    }
    let yaml: serde_yaml::Value = serde_yaml::from_str(value).map_err(|error| {
        NarouError::Platform(format!("invalid extra fields YAML: {error}"))
    })?;
    extra_fields_mapping(yaml)
}

fn column_error(name: &'static str, detail: impl std::fmt::Display) -> NarouError {
    NarouError::Platform(format!("invalid novels.{name} value: {detail}"))
}

fn text_at(
    columns: &mut BTreeMap<&'static str, NovelBind>,
    name: &'static str,
) -> Result<String> {
    match columns.remove(name) {
        Some(NovelBind::Text(value)) => Ok(value),
        Some(NovelBind::Null) => Err(column_error(name, "unexpected NULL")),
        Some(NovelBind::Int(value)) => Err(column_error(name, format!("integer {value}"))),
        None => Err(NarouError::Platform(format!(
            "novels.{name} is missing from the row"
        ))),
    }
}

fn opt_text_at(
    columns: &mut BTreeMap<&'static str, NovelBind>,
    name: &'static str,
) -> Result<Option<String>> {
    match columns.remove(name) {
        Some(NovelBind::Text(value)) => Ok(Some(value)),
        None | Some(NovelBind::Null) => Ok(None),
        Some(NovelBind::Int(value)) => Err(column_error(name, format!("integer {value}"))),
    }
}

fn int_at(
    columns: &mut BTreeMap<&'static str, NovelBind>,
    name: &'static str,
) -> Result<i64> {
    match columns.remove(name) {
        Some(NovelBind::Int(value)) => Ok(value),
        Some(NovelBind::Null) => Err(column_error(name, "unexpected NULL")),
        Some(NovelBind::Text(value)) => Err(column_error(name, format!("text {value:?}"))),
        None => Err(NarouError::Platform(format!(
            "novels.{name} is missing from the row"
        ))),
    }
}

fn opt_int_at(
    columns: &mut BTreeMap<&'static str, NovelBind>,
    name: &'static str,
) -> Result<Option<i64>> {
    match columns.remove(name) {
        Some(NovelBind::Int(value)) => Ok(Some(value)),
        None | Some(NovelBind::Null) => Ok(None),
        Some(NovelBind::Text(value)) => Err(column_error(name, format!("text {value:?}"))),
    }
}

fn flag_at(
    columns: &mut BTreeMap<&'static str, NovelBind>,
    name: &'static str,
) -> Result<bool> {
    Ok(int_at(columns, name)? != 0)
}

fn time_at(
    columns: &mut BTreeMap<&'static str, NovelBind>,
    name: &'static str,
) -> Result<DateTime<Utc>> {
    parse_time(text_at(columns, name)?)
}

fn opt_time_at(
    columns: &mut BTreeMap<&'static str, NovelBind>,
    name: &'static str,
) -> Result<Option<DateTime<Utc>>> {
    parse_optional_time(opt_text_at(columns, name)?)
}

/// Build a record from named column values (a decoded `novels` row).
/// `columns` must use names from `NOVEL_SELECT_COLUMNS`; unknown names are an
/// error, missing optional columns default to `None`, and `requires_login`
/// keeps its serde-default of `0` when absent. `parse_extra_fields` decodes
/// `extra_fields_yaml` so each backend keeps its own payload cap / fallback.
pub fn record_from_columns<F>(
    columns: impl IntoIterator<Item = (&'static str, NovelBind)>,
    parse_extra_fields: F,
) -> Result<NovelRecord>
where
    F: FnOnce(&str) -> Result<BTreeMap<String, serde_yaml::Value>>,
{
    let mut map: BTreeMap<&'static str, NovelBind> = BTreeMap::new();
    for (name, value) in columns.into_iter() {
        if !NOVEL_SELECT_COLUMNS.contains(&name) {
            return Err(NarouError::Platform(format!(
                "novels.{name} is not a selected column"
            )));
        }
        if map.insert(name, value).is_some() {
            return Err(NarouError::Platform(format!(
                "novels.{name} appears twice in the row"
            )));
        }
    }

    let tags: Vec<String> = serde_json::from_str(&text_at(&mut map, "tags_json")?)
        .map_err(|error| NarouError::Platform(format!("invalid tags JSON: {error}")))?;
    let extra_fields = parse_extra_fields(&text_at(&mut map, "extra_fields_yaml")?)?;
    // `#[serde(default)]` on the Worker's row type: a row without the column
    // (old D1 snapshots) reads as 0.
    let requires_login = match map.remove("requires_login") {
        None => false,
        Some(NovelBind::Int(value)) => value != 0,
        Some(NovelBind::Null) => {
            return Err(column_error("requires_login", "unexpected NULL"));
        }
        Some(NovelBind::Text(value)) => {
            return Err(column_error("requires_login", format!("text {value:?}")));
        }
    };

    Ok(NovelRecord {
        id: int_at(&mut map, "id")?,
        author: text_at(&mut map, "author")?,
        title: text_at(&mut map, "title")?,
        file_title: text_at(&mut map, "file_title")?,
        toc_url: text_at(&mut map, "toc_url")?,
        sitename: text_at(&mut map, "sitename")?,
        novel_type: u8::try_from(int_at(&mut map, "novel_type")?).unwrap_or_default(),
        end: flag_at(&mut map, "end")?,
        last_update: time_at(&mut map, "last_update")?,
        new_arrivals_date: opt_time_at(&mut map, "new_arrivals_date")?,
        use_subdirectory: flag_at(&mut map, "use_subdirectory")?,
        general_firstup: opt_time_at(&mut map, "general_firstup")?,
        novelupdated_at: opt_time_at(&mut map, "novelupdated_at")?,
        general_lastup: opt_time_at(&mut map, "general_lastup")?,
        last_mail_date: opt_time_at(&mut map, "last_mail_date")?,
        tags,
        ncode: opt_text_at(&mut map, "ncode")?,
        domain: opt_text_at(&mut map, "domain")?,
        general_all_no: opt_int_at(&mut map, "general_all_no")?,
        length: opt_int_at(&mut map, "length")?,
        suspend: flag_at(&mut map, "suspend")?,
        is_narou: flag_at(&mut map, "is_narou")?,
        last_check_date: opt_time_at(&mut map, "last_check_date")?,
        convert_failure: flag_at(&mut map, "convert_failure")?,
        requires_login,
        login_session: opt_text_at(&mut map, "login_session")?,
        extra_fields,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample_record() -> NovelRecord {
        NovelRecord {
            id: 7,
            author: "Author".to_string(),
            title: "Title".to_string(),
            file_title: "[Author] Title".to_string(),
            toc_url: "https://ncode.syosetu.com/n0007aa/".to_string(),
            sitename: "小説家になろう".to_string(),
            novel_type: 2,
            end: true,
            last_update: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            new_arrivals_date: Some(Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap()),
            use_subdirectory: false,
            general_firstup: Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()),
            novelupdated_at: None,
            general_lastup: Some(Utc.with_ymd_and_hms(2026, 1, 3, 0, 0, 0).unwrap()),
            last_mail_date: None,
            tags: vec!["Tag A".to_string(), "タグ".to_string()],
            ncode: Some("n0007aa".to_string()),
            domain: Some("ncode.syosetu.com".to_string()),
            general_all_no: Some(3),
            length: Some(1000),
            suspend: false,
            is_narou: true,
            last_check_date: None,
            convert_failure: false,
            requires_login: true,
            login_session: Some("main".to_string()),
            extra_fields: BTreeMap::from([
                ("raw_title".to_string(), serde_yaml::Value::String("作品名".to_string())),
                (
                    "answer".to_string(),
                    serde_yaml::Value::Number(serde_yaml::Number::from(42_i64)),
                ),
            ]),
        }
    }

    fn bind<'a>(binds: &'a [(&'static str, NovelBind)], name: &str) -> &'a NovelBind {
        binds
            .iter()
            .find(|(bind_name, _)| *bind_name == name)
            .map(|(_, value)| value)
            .unwrap_or_else(|| panic!("missing bind {name}"))
    }

    #[test]
    fn upsert_sql_columns_match_shared_list() {
        assert_eq!(upsert_columns(NOVEL_UPSERT_SQL).unwrap(), NOVEL_COLUMNS);
    }

    #[test]
    fn select_sql_columns_match_shared_list() {
        assert_eq!(select_columns(NOVEL_SELECT_SQL).unwrap(), NOVEL_SELECT_COLUMNS);
    }

    #[test]
    fn novel_binds_cover_every_upsert_column() {
        let binds = novel_binds(&sample_record()).unwrap();
        let names: Vec<&str> = binds.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, NOVEL_COLUMNS);
    }

    /// Regression test for the Worker upsert bug (extra_fields_bytes received
    /// login_session's NULL and every insert failed with NOT NULL).
    #[test]
    fn extra_fields_binds_carry_yaml_and_its_byte_length() {
        let mut record = sample_record();
        record.login_session = None;
        let binds = novel_binds(&record).unwrap();

        let NovelBind::Text(yaml) = bind(&binds, "extra_fields_yaml") else {
            panic!("extra_fields_yaml must be text")
        };
        let fields: BTreeMap<String, serde_yaml::Value> = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            fields.get("raw_title").and_then(serde_yaml::Value::as_str),
            Some("作品名")
        );
        assert_eq!(
            bind(&binds, "extra_fields_bytes"),
            &NovelBind::Int(yaml.len() as i64),
            "extra_fields_bytes must be the YAML byte length"
        );
        assert_eq!(
            bind(&binds, "login_session"),
            &NovelBind::Null,
            "absent login_session must be NULL, not an empty string"
        );
        assert_eq!(
            bind(&binds, "requires_login"),
            &NovelBind::Int(1),
        );
    }

    /// The pre-shared-codec failure shape: binds emitted in the wrong order
    /// must not silently pass — `ordered_binds` orders by the SQL column list
    /// regardless of input order.
    #[test]
    fn ordered_binds_follows_sql_columns_not_input_order() {
        let mut binds = novel_binds(&sample_record()).unwrap();
        binds.swap(32, 34); // scramble extra_fields_yaml / requires_login
        let ordered = ordered_binds(NOVEL_UPSERT_SQL, &binds).unwrap();
        let names: Vec<&str> = ordered.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, NOVEL_COLUMNS);
    }

    #[test]
    fn ordered_binds_rejects_missing_and_unexpected_columns() {
        // Swapped-in wrong value (the original bug): a bind named for a column
        // it does not belong to is impossible, but missing/extra names must be
        // errors.
        let mut binds = novel_binds(&sample_record()).unwrap();
        binds.retain(|(name, _)| *name != "extra_fields_bytes");
        let error = ordered_binds(NOVEL_UPSERT_SQL, &binds).unwrap_err();
        assert!(error.to_string().contains("extra_fields_bytes"), "{error}");

        let mut binds = novel_binds(&sample_record()).unwrap();
        binds.push(("bogus_column", NovelBind::Int(0)));
        let error = ordered_binds(NOVEL_UPSERT_SQL, &binds).unwrap_err();
        assert!(error.to_string().contains("bogus_column"), "{error}");

        let mut binds = novel_binds(&sample_record()).unwrap();
        binds.push(("author", NovelBind::Text("dup".to_string())));
        let error = ordered_binds(NOVEL_UPSERT_SQL, &binds).unwrap_err();
        assert!(error.to_string().contains("author"), "{error}");
    }

    #[test]
    fn ordered_binds_rejects_marker_count_mismatch() {
        let binds = novel_binds(&sample_record()).unwrap();
        let sql = NOVEL_UPSERT_SQL.replacen("NULLIF(?, '')", "NULLIF(0, '')", 1);
        assert!(ordered_binds(&sql, &binds).is_err());
    }

    #[test]
    fn optional_time_decodes_empty_string_as_none() {
        assert_eq!(parse_optional_time(None).unwrap(), None);
        assert_eq!(parse_optional_time(Some(String::new())).unwrap(), None);
        let stamp = Some("2026-01-01T00:00:00Z".to_string());
        assert_eq!(
            parse_optional_time(stamp).unwrap(),
            Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap())
        );
    }

    #[test]
    fn encode_decode_round_trip_through_named_binds() {
        let record = sample_record();
        let binds = novel_binds(&record).unwrap();
        // Simulate a row read: keep the columns SELECT returns, in select
        // order.
        let row: Vec<(&'static str, NovelBind)> = NOVEL_SELECT_COLUMNS
            .iter()
            .filter_map(|name| {
                binds
                    .iter()
                    .find(|(bind_name, _)| bind_name == name)
                    .map(|(name, value)| (*name, value.clone()))
            })
            .collect();
        let decoded = record_from_columns(row, parse_extra_fields).unwrap();
        assert_eq!(
            serde_json::to_value(&record).unwrap(),
            serde_json::to_value(&decoded).unwrap()
        );
    }

    #[test]
    fn decode_tolerates_missing_optional_columns() {
        let record = sample_record();
        let binds = novel_binds(&record).unwrap();
        let row: Vec<(&'static str, NovelBind)> = binds
            .into_iter()
            .filter(|(name, _)| {
                NOVEL_SELECT_COLUMNS.contains(name)
                    && !["requires_login", "login_session", "ncode"].contains(name)
            })
            .collect();
        let decoded = record_from_columns(row, parse_extra_fields).unwrap();
        assert!(!decoded.requires_login);
        assert_eq!(decoded.login_session, None);
        assert_eq!(decoded.ncode, None);
    }

    #[test]
    fn decode_rejects_unknown_and_unexpected_null_columns() {
        let mut row: Vec<(&'static str, NovelBind)> =
            novel_binds(&sample_record())
                .unwrap()
                .into_iter()
                .filter(|(name, _)| NOVEL_SELECT_COLUMNS.contains(name))
                .collect();
        row.push(("bogus_column", NovelBind::Int(1)));
        assert!(record_from_columns(row, parse_extra_fields).is_err());

        let row: Vec<(&'static str, NovelBind)> = novel_binds(&sample_record())
            .unwrap()
            .into_iter()
            .filter(|(name, _)| NOVEL_SELECT_COLUMNS.contains(name))
            .map(|(name, value)| {
                if name == "title" {
                    (name, NovelBind::Null)
                } else {
                    (name, value)
                }
            })
            .collect();
        assert!(record_from_columns(row, parse_extra_fields).is_err());
    }

    /// Mirrors the native `removed_novel_can_be_reregistered_with_duplicate_tags`
    /// regression at the shared layer: both drivers normalize through
    /// `dedup_tags` before encoding, so duplicate names reach neither
    /// `novel_tags` (`UNIQUE (novel_id, tag)`) nor `tags_json`/`tags_fold`/
    /// `tags_sort`.
    #[test]
    fn dedup_tags_drops_duplicates_keeping_first_occurrence_order() {
        let mut tags = vec![
            "favorite".to_string(),
            "end".to_string(),
            "favorite".to_string(),
            "end".to_string(),
        ];
        dedup_tags(&mut tags);
        assert_eq!(tags, vec!["favorite", "end"]);

        // Exact-match semantics: case-folded twins are distinct tags.
        let mut tags = vec!["Tag".to_string(), "tag".to_string(), "Tag".to_string()];
        dedup_tags(&mut tags);
        assert_eq!(tags, vec!["Tag", "tag"]);
    }

    #[test]
    fn normalized_record_encodes_deduped_tag_binds() {
        let mut record = sample_record();
        record.tags = vec![
            "favorite".to_string(),
            "end".to_string(),
            "favorite".to_string(),
            "end".to_string(),
        ];
        dedup_tags(&mut record.tags);
        let binds = novel_binds(&record).unwrap();
        assert_eq!(
            bind(&binds, "tags_json"),
            &NovelBind::Text("[\"favorite\",\"end\"]".to_string())
        );
        assert_eq!(
            bind(&binds, "tags_fold"),
            &NovelBind::Text("favorite\nend".to_string())
        );
        assert_eq!(
            bind(&binds, "tags_sort"),
            &NovelBind::Text("favorite\u{1f}end".to_string())
        );
    }
}

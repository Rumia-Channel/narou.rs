//! Record ↔ row mapping. The codec (column names, bind values, time/fold/YAML
//! conventions) lives in `crate::db::novel_codec` and is shared with the
//! Worker D1 adapter; this file only converts [`NovelBind`] to
//! `rusqlite::types::Value` and applies sqlite's local conventions.

use rusqlite::types::Value;
use rusqlite::Row;

use crate::db::novel_codec::{
    self, MAX_EXTRA_FIELDS_BYTES, NOVEL_SELECT_COLUMNS, NovelBind,
};
use crate::db::NovelRecord;
use crate::error::{NarouError, Result};
use crate::native::sqlite::query::UPSERT_SQL;

fn driver_bind(value: Value, name: &'static str) -> Result<NovelBind> {
    Ok(match value {
        Value::Null => NovelBind::Null,
        Value::Integer(value) => NovelBind::Int(value),
        Value::Text(value) => NovelBind::Text(value),
        Value::Real(value) => {
            return Err(NarouError::Platform(format!(
                "sqlite novels.{name} is a real: {value}"
            )))
        }
        Value::Blob(_) => {
            return Err(NarouError::Platform(format!(
                "sqlite novels.{name} is a blob"
            )))
        }
    })
}

/// Decode one `select_sql()` row. Column names (not positional indices) carry
/// each cell to its record field, so the SELECT list cannot silently drift.
pub(crate) fn record_from_row(row: &Row<'_>) -> Result<NovelRecord> {
    let mut columns = Vec::with_capacity(NOVEL_SELECT_COLUMNS.len());
    for (index, name) in NOVEL_SELECT_COLUMNS.iter().enumerate() {
        let value: Value = row
            .get(index)
            .map_err(|error| NarouError::Platform(format!("sqlite column {name}: {error}")))?;
        columns.push((*name, driver_bind(value, name)?));
    }
    novel_codec::record_from_columns(columns, novel_codec::parse_extra_fields)
}

/// Convert one named bind to a `rusqlite::types::Value`.
fn to_sql_value(name: &'static str, bind: &NovelBind) -> Value {
    match bind {
        NovelBind::Text(text) => Value::Text(text.clone()),
        NovelBind::Int(int) => Value::Integer(*int),
        // The UPSERT wraps `login_session` in no NULLIF: the Worker stores a
        // real NULL while the native adapter historically wrote "" via
        // `opt_text`. Keep the native convention so stored rows do not change.
        NovelBind::Null if name == "login_session" => Value::Text(String::new()),
        NovelBind::Null => Value::Null,
    }
}

/// Encode a record into positional SQL parameters for `UPSERT_SQL`. The order
/// is derived from the SQL statement itself, not from a hand-maintained list.
pub(crate) fn record_params(record: &NovelRecord) -> Result<Vec<Value>> {
    let binds = novel_codec::novel_binds(record)?;
    novel_codec::check_extra_fields_limit(&binds, MAX_EXTRA_FIELDS_BYTES)?;
    Ok(novel_codec::ordered_binds(UPSERT_SQL, &binds)?
        .into_iter()
        .map(|(name, bind)| to_sql_value(name, bind))
        .collect())
}

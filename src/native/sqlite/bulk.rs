//! Bulk record load/persist used by the `Database` persistence seam (P2).
//!
//! The web/CLI layers keep working on the in-memory `Database` struct; these
//! helpers swap its backing store from `database.yaml` to the relational
//! `novels`/`novel_tags` tables without touching call sites.

use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::db::NovelRecord;
use crate::error::Result;

use super::repository::{record_from_row, upsert_record_conn};
use super::sqlite_error;

/// Load every novel as `(id, record)`, ordered by id.
pub(crate) fn load_all_records(
    conn: &Connection,
) -> Result<BTreeMap<i64, NovelRecord>> {
    let sql = format!("{} ORDER BY n.id", super::query::select_sql());
    let mut statement = conn.prepare(&sql).map_err(sqlite_error)?;
    let mut rows = statement.query([]).map_err(sqlite_error)?;
    let mut map = BTreeMap::new();
    while let Some(row) = rows.next().map_err(sqlite_error)? {
        let record = record_from_row(row)?;
        map.insert(record.id, record);
    }
    Ok(map)
}

/// Replace the whole novel table contents with `records` inside one
/// transaction: upserts every row (refreshing tags + derived status), deletes
/// ids that disappeared, and resynchronizes the id sequence.
pub(crate) fn replace_all_records(
    conn: &mut Connection,
    records: &BTreeMap<i64, NovelRecord>,
) -> Result<()> {
    let existing: Vec<i64> = {
        let mut statement = conn
            .prepare("SELECT id FROM novels ORDER BY id")
            .map_err(sqlite_error)?;
        let rows = statement.query_map([], |row| row.get(0)).map_err(sqlite_error)?;
        let mut ids = Vec::new();
        for id in rows {
            ids.push(id.map_err(|error| crate::error::NarouError::Platform(error.to_string()))?);
        }
        ids
    };

    let tx = conn.transaction().map_err(sqlite_error)?;
    for record in records.values() {
        upsert_record_conn(&tx, record)?;
    }
    for id in existing {
        if !records.contains_key(&id) {
            // Cascades to novel_tags / frozen_novels.
            tx.execute("DELETE FROM novels WHERE id = ?", [id])
                .map_err(sqlite_error)?;
        }
    }
    let max_id = records.keys().copied().max().unwrap_or(0);
    tx.execute(
        "UPDATE novel_id_sequence SET next_id = ? WHERE id = 1",
        [max_id.saturating_add(1)],
    )
    .map_err(sqlite_error)?;
    tx.commit().map_err(sqlite_error)
}

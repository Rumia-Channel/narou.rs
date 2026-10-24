//! Bulk record load/persist used by the `Database` persistence seam (P2).
//!
//! The web/CLI layers keep working on the in-memory `Database` struct; these
//! helpers swap its backing store from `database.yaml` to the relational
//! `novels`/`novel_tags` tables without touching call sites.

use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::db::novel_query::select_sql;
use crate::db::NovelRecord;
use crate::error::Result;

use super::record_map::record_from_row;
use super::repository::upsert_record_conn;
use super::sqlite_error;

/// Maximum number of ids bound into one `DELETE ... IN (...)` statement.
const DELETE_CHUNK: usize = 200;

/// Load every novel as `(id, record)`, ordered by id.
pub(crate) fn load_all_records(
    conn: &Connection,
) -> Result<BTreeMap<i64, NovelRecord>> {
    let sql = format!("{} ORDER BY n.id", select_sql());
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
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(sqlite_error)?;
    replace_records_conn(&tx, records)?;
    tx.commit().map_err(sqlite_error)
}

/// Persist an authoritative snapshot inside the caller's write transaction.
/// The caller must acquire that transaction before reading/mutating records.
pub(crate) fn replace_records_conn(
    conn: &Connection,
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

    for (&id, record) in records {
        let mut normalized = record.clone();
        normalized.id = id;
        upsert_record_conn(conn, &normalized)?;
    }
    let stale: Vec<i64> = existing
        .into_iter()
        .filter(|id| !records.contains_key(id))
        .collect();
    // Cascades to novel_tags / frozen_novels per row; chunking the `IN` list
    // turns one statement per removed id into one statement per 200 ids.
    for chunk in stale.chunks(DELETE_CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("DELETE FROM novels WHERE id IN ({placeholders})");
        conn.execute(&sql, rusqlite::params_from_iter(chunk.iter()))
            .map_err(sqlite_error)?;
    }
    // upsert_record_conn only advances the sequence. Never rewind it to the
    // maximum published ID: higher IDs may belong to live reservations.
    Ok(())
}

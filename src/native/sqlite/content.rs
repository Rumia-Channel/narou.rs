//! P4 content + version history storage.
//!
//! Working set contract: the authoritative sections stay in the ObjectStore
//! (`本文/*.yaml`); these tables mirror them per novel so download-time EPUB
//! and version tooling can run without touching the filesystem. Mirrors are
//! refreshed on convert (native, `lite` feature) and version snapshots are
//! created from the mirrored working set.

use std::collections::BTreeMap;

use chrono::Utc;
use rusqlite::{params, Connection};

use crate::error::{NarouError, Result};

use super::sqlite_error;

/// Store (or replace) the mirrored section set of one novel.
pub fn store_sections(
    conn: &mut Connection,
    novel_id: i64,
    sections: &BTreeMap<String, (Option<String>, String)>,
) -> Result<()> {
    // sections: idx -> (subtitle, body_yaml)
    let tx = conn.transaction().map_err(sqlite_error)?;
    tx.execute("DELETE FROM novel_sections WHERE novel_id = ?", [novel_id])
        .map_err(sqlite_error)?;
    for (idx, (subtitle, body)) in sections {
        tx.execute(
            "INSERT INTO novel_sections (novel_id, idx, subtitle, body_yaml) VALUES (?, ?, ?, ?)",
            params![novel_id, idx, subtitle, body],
        )
        .map_err(sqlite_error)?;
    }
    tx.commit().map_err(sqlite_error)
}

/// Load the mirrored section set; `Ok(None)` when nothing is mirrored yet
/// (caller falls back to the ObjectStore / lazy migration).
pub fn load_sections(
    conn: &Connection,
    novel_id: i64,
) -> Result<Option<BTreeMap<String, (Option<String>, String)>>> {
    let mut statement = conn
        .prepare("SELECT idx, subtitle, body_yaml FROM novel_sections WHERE novel_id = ? ORDER BY idx")
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map([novel_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(sqlite_error)?;
    let mut map = BTreeMap::new();
    for row in rows {
        let (idx, subtitle, body) = row.map_err(|error| NarouError::Platform(error.to_string()))?;
        map.insert(idx, (subtitle, body));
    }
    if map.is_empty() {
        Ok(None)
    } else {
        Ok(Some(map))
    }
}

/// Store a generated output blob (`kind` examples: `converted_text`).
pub fn store_output(conn: &Connection, novel_id: i64, kind: &str, payload: &[u8]) -> Result<()> {
    conn.execute(
        "INSERT INTO novel_outputs (novel_id, kind, payload, updated_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(novel_id, kind) DO UPDATE SET payload = excluded.payload, updated_at = excluded.updated_at",
        params![
            novel_id,
            kind,
            payload,
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
        ],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

pub fn load_output(conn: &Connection, novel_id: i64, kind: &str) -> Result<Option<Vec<u8>>> {
    let mut statement = conn
        .prepare("SELECT payload FROM novel_outputs WHERE novel_id = ? AND kind = ?")
        .map_err(sqlite_error)?;
    let mut rows = statement
        .query(params![novel_id, kind])
        .map_err(sqlite_error)?;
    match rows.next().map_err(sqlite_error)? {
        Some(row) => {
            let payload: Vec<u8> =
                row.get(0).map_err(|error| NarouError::Platform(error.to_string()))?;
            Ok(Some(payload))
        }
        None => Ok(None),
    }
}

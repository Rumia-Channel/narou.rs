//! P4b version history over the mirrored working set (`novel_sections`).
//!
//! Snapshots are immutable; restore/merge are copy-forward operations that
//! create a new head instead of rewriting history (plan §11.3).

use std::collections::BTreeMap;

use chrono::Utc;
use rusqlite::{params, Connection};
use similar::TextDiff;

use crate::error::{NarouError, Result};

use super::sqlite_error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionInfo {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub origin: String,
    pub note: Option<String>,
    pub created_at: String,
    pub section_count: i64,
}

fn render_for_diff(sections: &BTreeMap<String, (Option<String>, String)>) -> String {
    let mut text = String::new();
    for (idx, (subtitle, body)) in sections {
        text.push_str(&format!("=== {idx} {}\n", subtitle.as_deref().unwrap_or("")));
        text.push_str(body);
        text.push('\n');
    }
    text
}

fn unified_diff(old: &str, new: &str) -> String {
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header("old", "new")
        .to_string()
}

/// Snapshot the mirrored working set as a new immutable version and record
/// the unified diff against the previous head. Fails when nothing is
/// mirrored yet (callers must refresh the mirror first).
pub fn snapshot_working_set(
    conn: &mut Connection,
    novel_id: i64,
    origin: &str,
    note: Option<&str>,
) -> Result<i64> {
    let Some((current, _prev_head)) = current_and_prev(conn, novel_id)? else {
        return Err(NarouError::Platform(
            "version snapshot requires a mirrored working set (novel_sections)".into(),
        ));
    };
    let prev_head = _prev_head;
    let tx = conn.transaction().map_err(sqlite_error)?;
    tx.execute(
        "INSERT INTO novel_versions (novel_id, parent_id, origin, note, created_at) VALUES (?, ?, ?, ?, ?)",
        params![
            novel_id,
            prev_head,
            origin,
            note,
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
        ],
    )
    .map_err(sqlite_error)?;
    let version_id = tx.last_insert_rowid();
    for (idx, (subtitle, body)) in &current {
        tx.execute(
            "INSERT INTO novel_version_sections (version_id, idx, subtitle, body_yaml) VALUES (?, ?, ?, ?)",
            params![version_id, idx, subtitle, body],
        )
        .map_err(sqlite_error)?;
    }
    if let Some(prev_id) = prev_head {
        let old = load_sections_text(&tx, prev_id)?;
        let diff = unified_diff(&old, &render_for_diff(&current));
        tx.execute(
            "INSERT INTO novel_version_diffs (version_id, prev_version_id, unified_diff) VALUES (?, ?, ?)",
            params![version_id, prev_id, diff],
        )
        .map_err(sqlite_error)?;
    } else {
        tx.execute(
            "INSERT INTO novel_version_diffs (version_id, prev_version_id, unified_diff) VALUES (?, NULL, '')",
            [version_id],
        )
        .map_err(sqlite_error)?;
    }
    tx.commit().map_err(sqlite_error)?;
    Ok(version_id)
}

fn current_and_prev(
    conn: &Connection,
    novel_id: i64,
) -> Result<Option<(BTreeMap<String, (Option<String>, String)>, Option<i64>)>> {
    let working = super::content::load_sections(conn, novel_id)?;
    let Some(working) = working else {
        return Ok(None);
    };
    let prev: Option<i64> = conn
        .query_row(
            "SELECT id FROM novel_versions WHERE novel_id = ? ORDER BY id DESC LIMIT 1",
            [novel_id],
            |row| row.get(0),
        )
        .map(Some)
        .unwrap_or(None);
    Ok(Some((working, prev)))
}

fn load_sections_text(
    conn: &Connection,
    version_id: i64,
) -> Result<String> {
    let mut statement = conn
        .prepare("SELECT idx, subtitle, body_yaml FROM novel_version_sections WHERE version_id = ? ORDER BY idx")
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map([version_id], |row| {
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
    Ok(render_for_diff(&map))
}

/// Version headers, newest first.
pub fn list_versions(conn: &Connection, novel_id: i64) -> Result<Vec<VersionInfo>> {
    let mut statement = conn
        .prepare(
            "SELECT v.id, v.parent_id, v.origin, v.note, v.created_at,
                    (SELECT COUNT(*) FROM novel_version_sections s WHERE s.version_id = v.id)
             FROM novel_versions v WHERE v.novel_id = ? ORDER BY v.id DESC",
        )
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map([novel_id], |row| {
            Ok(VersionInfo {
                id: row.get(0)?,
                parent_id: row.get(1)?,
                origin: row.get(2)?,
                note: row.get(3)?,
                created_at: row.get(4)?,
                section_count: row.get(5)?,
            })
        })
        .map_err(sqlite_error)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|error| NarouError::Platform(error.to_string()))?);
    }
    Ok(out)
}

/// Stored unified diff for one version (empty for the initial snapshot).
pub fn version_diff(conn: &Connection, version_id: i64) -> Result<Option<String>> {
    let mut statement = conn
        .prepare("SELECT unified_diff FROM novel_version_diffs WHERE version_id = ?")
        .map_err(sqlite_error)?;
    let mut rows = statement.query([version_id]).map_err(sqlite_error)?;
    match rows.next().map_err(sqlite_error)? {
        Some(row) => {
            let diff: String =
                row.get(0).map_err(|error| NarouError::Platform(error.to_string()))?;
            Ok(Some(diff))
        }
        None => Ok(None),
    }
}

/// Sections stored in one version; `None` when the version does not exist.
pub fn version_sections(
    conn: &Connection,
    version_id: i64,
) -> Result<Option<BTreeMap<String, (Option<String>, String)>>> {
    let mut statement = conn
        .prepare("SELECT idx, subtitle, body_yaml FROM novel_version_sections WHERE version_id = ? ORDER BY idx")
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map([version_id], |row| {
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

fn version_exists(conn: &Connection, novel_id: i64, version_id: i64) -> Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT id FROM novel_versions WHERE id = ? AND novel_id = ?",
            params![version_id, novel_id],
            |row| row.get(0),
        )
        .map(Some)
        .unwrap_or(None);
    Ok(found.is_some())
}

/// Copy-forward restore: the target version's sections become the new working
/// set AND a new `rollback` head records the operation (non-destructive).
pub fn restore_version(
    conn: &mut Connection,
    novel_id: i64,
    version_id: i64,
    drop_newer: bool,
) -> Result<i64> {
    if !version_exists(conn, novel_id, version_id)? {
        return Err(NarouError::Platform(format!("version {version_id} not found")));
    }
    let target = version_sections(conn, version_id)?
        .ok_or_else(|| NarouError::Platform(format!("version {version_id} has no sections")))?;

    if drop_newer {
        let tx = conn.transaction().map_err(sqlite_error)?;
        tx.execute(
            "DELETE FROM novel_versions WHERE novel_id = ? AND id > ?",
            params![novel_id, version_id],
        )
        .map_err(sqlite_error)?;
        tx.commit().map_err(sqlite_error)?;
    }

    // Replace working mirror with the restored content.
    super::content::store_sections(conn, novel_id, &target)?;

    // Record the rollback as a new head (parent = restored version).
    let prev_head: Option<i64> = conn
        .query_row(
            "SELECT id FROM novel_versions WHERE novel_id = ? ORDER BY id DESC LIMIT 1",
            [novel_id],
            |row| row.get(0),
        )
        .map(Some)
        .unwrap_or(None);
    let tx = conn.transaction().map_err(sqlite_error)?;
    tx.execute(
        "INSERT INTO novel_versions (novel_id, parent_id, origin, note, created_at) VALUES (?, ?, 'rollback', ?, ?)",
        params![
            novel_id,
            version_id,
            format!("restore of version {version_id}"),
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
        ],
    )
    .map_err(sqlite_error)?;
    let new_id = tx.last_insert_rowid();
    for (idx, (subtitle, body)) in &target {
        tx.execute(
            "INSERT INTO novel_version_sections (version_id, idx, subtitle, body_yaml) VALUES (?, ?, ?, ?)",
            params![new_id, idx, subtitle, body],
        )
        .map_err(sqlite_error)?;
    }
    tx.execute(
        "INSERT INTO novel_version_diffs (version_id, prev_version_id, unified_diff) VALUES (?, NULL, '')",
        [new_id],
    )
    .map_err(sqlite_error)?;
    tx.commit().map_err(sqlite_error)?;
    let _ = prev_head;
    Ok(new_id)
}

/// Selective merge: copy the requested subset (or all) sections of one
/// version onto the working mirror, then snapshot the result as a `manual`
/// head so every change stays revertible.
pub fn merge_from_version(
    conn: &mut Connection,
    novel_id: i64,
    version_id: i64,
    only_sections: Option<&[String]>,
    note: Option<&str>,
) -> Result<i64> {
    let source = version_sections(conn, version_id)?
        .ok_or_else(|| NarouError::Platform(format!("version {version_id} not found")))?;

    // Apply overlay onto the working mirror.
    let mut working = super::content::load_sections(conn, novel_id)?
        .ok_or_else(|| NarouError::Platform("working set not mirrored yet".into()))?;
    for (idx, value) in &source {
        let selected = only_sections
            .as_ref()
            .map(|list| list.iter().any(|wanted| wanted == idx))
            .unwrap_or(true);
        if selected {
            working.insert(idx.clone(), value.clone());
        }
    }
    super::content::store_sections(conn, novel_id, &working)?;

    // Head snapshot of the merged result (diff vs previous head).
    let (current, prev_head) = current_and_prev(conn, novel_id)?.expect("just stored");
    let tx = conn.transaction().map_err(sqlite_error)?;
    tx.execute(
        "INSERT INTO novel_versions (novel_id, parent_id, origin, note, created_at) VALUES (?, ?, 'manual', ?, ?)",
        params![
            novel_id,
            prev_head,
            note.unwrap_or("merge"),
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
        ],
    )
    .map_err(sqlite_error)?;
    let new_id = tx.last_insert_rowid();
    for (idx, (subtitle, body)) in &current {
        tx.execute(
            "INSERT INTO novel_version_sections (version_id, idx, subtitle, body_yaml) VALUES (?, ?, ?, ?)",
            params![new_id, idx, subtitle, body],
        )
        .map_err(sqlite_error)?;
    }
    if let Some(prev_id) = prev_head {
        let old = load_sections_text(&tx, prev_id)?;
        let diff = unified_diff(&old, &render_for_diff(&current));
        tx.execute(
            "INSERT INTO novel_version_diffs (version_id, prev_version_id, unified_diff) VALUES (?, ?, ?)",
            params![new_id, prev_id, diff],
        )
        .map_err(sqlite_error)?;
    } else {
        tx.execute(
            "INSERT INTO novel_version_diffs (version_id, prev_version_id, unified_diff) VALUES (?, NULL, '')",
            [new_id],
        )
        .map_err(sqlite_error)?;
    }
    tx.commit().map_err(sqlite_error)?;
    Ok(new_id)
}

/// Keep at most `keep` versions per novel (newest survive).
pub fn prune_history(conn: &Connection, novel_id: i64, keep: usize) -> Result<usize> {
    // Detach children first: newer heads may reference a pruned parent.
    conn.execute(
        "UPDATE novel_versions SET parent_id = NULL WHERE novel_id = ? AND parent_id IN (
            SELECT id FROM novel_versions WHERE novel_id = ? AND id NOT IN (
                SELECT id FROM novel_versions WHERE novel_id = ? ORDER BY id DESC LIMIT ?
             )
         )",
        params![novel_id, novel_id, novel_id, keep as i64],
    )
    .map_err(sqlite_error)?;
    let deleted = conn
        .execute(
            "DELETE FROM novel_versions WHERE novel_id = ? AND id NOT IN (
                SELECT id FROM novel_versions WHERE novel_id = ? ORDER BY id DESC LIMIT ?
             )",
            params![novel_id, novel_id, keep as i64],
        )
        .map_err(sqlite_error)?;
    Ok(deleted)
}

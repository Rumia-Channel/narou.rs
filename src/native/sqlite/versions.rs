//! P4b version history over the mirrored working set (`novel_sections`).
//!
//! Snapshots are immutable; restore/merge are copy-forward operations that
//! create a new head instead of rewriting history (plan §11.3).
//!
//! Section reads go through the body-joined query in `content` (one query per
//! version instead of one per section); unified diffs and their compression are
//! computed *before* the write transaction opens so brotli/TextDiff CPU does
//! not extend the shared-connection write lock.


use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use similar::TextDiff;

use crate::error::{NarouError, Result};
use crate::platform::{ObjectEncoding, compress_object_payload};

use super::content::SectionMap;
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

fn render_for_diff(sections: &SectionMap) -> String {
    let mut text = String::new();
    for (idx, (subtitle, body)) in sections {
        text.push_str(&format!(
            "=== {idx} {}\n",
            subtitle.as_deref().unwrap_or("")
        ));
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

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
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
    let Some((current, prev_head)) = current_and_prev(conn, novel_id)? else {
        return Err(NarouError::Platform(
            "version snapshot requires a mirrored working set (novel_sections)".into(),
        ));
    };
    let (diff_stored, diff_encoding) = prepare_diff(conn, prev_head, &current)?;

    let tx = conn.transaction().map_err(sqlite_error)?;
    tx.execute(
        "INSERT INTO novel_versions (novel_id, parent_id, origin, note, created_at) VALUES (?, ?, ?, ?, ?)",
        params![novel_id, prev_head, origin, note, now_rfc3339()],
    )
    .map_err(sqlite_error)?;
    let version_id = tx.last_insert_rowid();
    write_version_sections(&tx, version_id, &current)?;
    store_diff(&tx, version_id, prev_head, &diff_stored, diff_encoding)?;
    tx.commit().map_err(sqlite_error)?;
    Ok(version_id)
}

fn current_and_prev(conn: &Connection, novel_id: i64) -> Result<Option<(SectionMap, Option<i64>)>> {
    let working = super::content::load_sections(conn, novel_id)?;
    let Some(working) = working else {
        return Ok(None);
    };
    let prev = head_id(conn, novel_id)?;
    Ok(Some((working, prev)))
}

/// Latest head id for a novel (`None` before the first snapshot).
fn head_id(conn: &Connection, novel_id: i64) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM novel_versions WHERE novel_id = ? ORDER BY id DESC LIMIT 1",
        [novel_id],
        |row| row.get(0),
    ).optional().map_err(super::sqlite_error)
}

fn load_sections_text(conn: &Connection, version_id: i64) -> Result<String> {
    let sections = version_sections(conn, version_id)?.unwrap_or_default();
    Ok(render_for_diff(&sections))
}

/// Compressed diff of `current` against `prev_head`, computed outside the
/// write transaction. Without a previous head the initial (empty) diff is
/// stored, matching `compress_object_payload(b"")`.
fn prepare_diff(
    conn: &Connection,
    prev_head: Option<i64>,
    current: &SectionMap,
) -> Result<(Vec<u8>, ObjectEncoding)> {
    let Some(prev_id) = prev_head else {
        return Ok((Vec::new(), ObjectEncoding::None));
    };
    let old = load_sections_text(conn, prev_id)?;
    Ok(compress_object_payload(
        unified_diff(&old, &render_for_diff(current)).as_bytes(),
    ))
}

/// Insert one version's section rows, storing each body once in
/// `section_bodies` and linking by hash. The bodies normally already exist
/// (snapshot/restore copy them from the working set); the batch lookup in
/// [`super::content::store_bodies`] resolves them without a query per row.
fn write_version_sections(tx: &Connection, version_id: i64, sections: &SectionMap) -> Result<()> {
    let bodies: Vec<&str> = sections
        .values()
        .map(|(_, body)| body.as_str())
        .collect();
    let hashes = super::content::store_bodies(tx, &bodies)?;
    let mut insert = tx
        .prepare_cached(
            "INSERT INTO novel_version_sections (version_id, idx, subtitle, body_hash) VALUES (?, ?, ?, ?)",
        )
        .map_err(sqlite_error)?;
    for ((idx, (subtitle, _)), hash) in sections.iter().zip(hashes.iter()) {
        insert
            .execute(params![version_id, idx, subtitle, hash])
            .map_err(sqlite_error)?;
    }
    Ok(())
}

/// Store a unified diff already compressed with
/// [`compress_object_payload`].
fn store_diff(
    tx: &Connection,
    version_id: i64,
    prev_version_id: Option<i64>,
    stored: &[u8],
    encoding: ObjectEncoding,
) -> Result<()> {
    tx.execute(
        "INSERT INTO novel_version_diffs (version_id, prev_version_id, unified_diff, encoding) VALUES (?, ?, ?, ?)",
        params![version_id, prev_version_id, stored, encoding.as_str()],
    )
    .map_err(sqlite_error)?;
    Ok(())
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
        .prepare("SELECT unified_diff, encoding FROM novel_version_diffs WHERE version_id = ?")
        .map_err(sqlite_error)?;
    let mut rows = statement.query([version_id]).map_err(sqlite_error)?;
    match rows.next().map_err(sqlite_error)? {
        Some(row) => {
            let stored: Vec<u8> = row
                .get(0)
                .map_err(|error| NarouError::Platform(error.to_string()))?;
            let encoding: String = row
                .get(1)
                .map_err(|error| NarouError::Platform(error.to_string()))?;
            let encoding = crate::platform::ObjectEncoding::parse(&encoding)?;
            let data = crate::platform::decompress_object_payload(&stored, encoding)?;
            String::from_utf8(data)
                .map(Some)
                .map_err(|error| NarouError::Platform(format!("diff is not UTF-8: {error}")))
        }
        None => Ok(None),
    }
}

/// Sections stored in one version; `None` when the version does not exist.
pub fn version_sections(conn: &Connection, version_id: i64) -> Result<Option<SectionMap>> {
    super::content::load_section_map(conn, "novel_version_sections", "version_id", version_id)
}

fn version_exists(conn: &Connection, novel_id: i64, version_id: i64) -> Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT id FROM novel_versions WHERE id = ? AND novel_id = ?",
            params![version_id, novel_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(super::sqlite_error)?;
    Ok(found.is_some())
}

/// Copy-forward restore: the target version's sections become the new working
/// set AND a new `rollback` head records the operation (non-destructive).
///
/// Reading the target, dropping newer heads, swapping the working mirror and
/// recording the rollback share one transaction, so a failure can no longer
/// leave a restored working set without its rollback head (or vice versa).
pub fn restore_version(
    conn: &mut Connection,
    novel_id: i64,
    version_id: i64,
    drop_newer: bool,
) -> Result<i64> {
    if !version_exists(conn, novel_id, version_id)? {
        return Err(NarouError::Platform(format!(
            "version {version_id} not found"
        )));
    }
    let target = version_sections(conn, version_id)?
        .ok_or_else(|| NarouError::Platform(format!("version {version_id} has no sections")))?;

    let tx = conn.transaction().map_err(sqlite_error)?;
    if drop_newer {
        tx.execute(
            "DELETE FROM novel_versions WHERE novel_id = ? AND id > ?",
            params![novel_id, version_id],
        )
        .map_err(sqlite_error)?;
    }

    // Replace working mirror with the restored content.
    super::content::store_sections_in_tx(&tx, novel_id, &target)?;

    // Record the rollback as a new head (parent = restored version).
    tx.execute(
        "INSERT INTO novel_versions (novel_id, parent_id, origin, note, created_at) VALUES (?, ?, 'rollback', ?, ?)",
        params![
            novel_id,
            version_id,
            format!("restore of version {version_id}"),
            now_rfc3339()
        ],
    )
    .map_err(sqlite_error)?;
    let new_id = tx.last_insert_rowid();
    write_version_sections(&tx, new_id, &target)?;
    store_diff(&tx, new_id, None, &[], ObjectEncoding::None)?;
    tx.commit().map_err(sqlite_error)?;
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

    // The merged map is exactly what will be persisted, so it doubles as the
    // new head's snapshot; re-reading it (and the previous head) after the
    // store would repeat the whole section load for nothing.
    let prev_head = head_id(conn, novel_id)?;
    let (diff_stored, diff_encoding) = prepare_diff(conn, prev_head, &working)?;

    let tx = conn.transaction().map_err(sqlite_error)?;
    super::content::store_sections_in_tx(&tx, novel_id, &working)?;
    tx.execute(
        "INSERT INTO novel_versions (novel_id, parent_id, origin, note, created_at) VALUES (?, ?, 'manual', ?, ?)",
        params![
            novel_id,
            prev_head,
            note.unwrap_or("merge"),
            now_rfc3339()
        ],
    )
    .map_err(sqlite_error)?;
    let new_id = tx.last_insert_rowid();
    write_version_sections(&tx, new_id, &working)?;
    store_diff(&tx, new_id, prev_head, &diff_stored, diff_encoding)?;
    tx.commit().map_err(sqlite_error)?;
    Ok(new_id)
}

/// Keep at most `keep` versions per novel (newest survive).
pub fn prune_history(conn: &Connection, novel_id: i64, keep: usize) -> Result<usize> {
    // Both statements are one logical prune: detaching a parent without
    // deleting it (or the reverse) must not be observable.
    let tx = conn.unchecked_transaction().map_err(sqlite_error)?;
    // Detach children first: newer heads may reference a pruned parent.
    tx.execute(
        "UPDATE novel_versions SET parent_id = NULL WHERE novel_id = ? AND parent_id IN (
            SELECT id FROM novel_versions WHERE novel_id = ? AND id NOT IN (
                SELECT id FROM novel_versions WHERE novel_id = ? ORDER BY id DESC LIMIT ?
             )
         )",
        params![novel_id, novel_id, novel_id, keep as i64],
    )
    .map_err(sqlite_error)?;
    let deleted = tx
        .execute(
            "DELETE FROM novel_versions WHERE novel_id = ? AND id NOT IN (
                SELECT id FROM novel_versions WHERE novel_id = ? ORDER BY id DESC LIMIT ?
             )",
            params![novel_id, novel_id, keep as i64],
        )
        .map_err(sqlite_error)?;
    tx.commit().map_err(sqlite_error)?;
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::sqlite::content::{load_sections, store_sections};

    /// Fresh in-memory schema plus the minimal novel row the version tables'
    /// foreign key needs. `parking_lot` keeps the guard poisoning-free (the
    /// production handle keeps its documented `Arc<std::sync::Mutex<_>>`).
    fn test_conn() -> parking_lot::Mutex<Connection> {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::native::sqlite::migrations::apply(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO novels (id, author, author_fold, title, title_fold, file_title, toc_url, toc_url_fold, sitename, sitename_fold, last_update)
             VALUES (7, 'a', 'a', 't', 't', 'ft', 'https://x/7', 'https://x/7', 's', 's', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        parking_lot::Mutex::new(conn)
    }

    /// Snapshot/restore/merge keep ordered sections, deduplicated bodies and
    /// diff semantics for a working set larger than one hash-lookup chunk.
    #[test]
    fn snapshot_restore_merge_across_hash_chunk_boundary() {
        let conn = test_conn();
        let mut guard = conn.lock();
        let mut sections = SectionMap::new();
        for index in 1..=260u32 {
            sections.insert(
                index.to_string(),
                (Some(format!("第{index}話")), format!("body: {index}\n")),
            );
        }
        store_sections(&mut guard, 7, &sections).unwrap();
        let first = snapshot_working_set(&mut guard, 7, "update", None).unwrap();

        // Rewrite one body and append one section for the second snapshot.
        sections.insert(
            "1".to_string(),
            (Some("第一話".to_string()), "body: rewritten\n".to_string()),
        );
        sections.insert(
            "261".to_string(),
            (Some("第二百六十一話".to_string()), "body: 261\n".to_string()),
        );
        store_sections(&mut guard, 7, &sections).unwrap();
        let second = snapshot_working_set(&mut guard, 7, "update", None).unwrap();

        let versions = list_versions(&guard, 7).unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].id, second);
        assert_eq!(versions[0].section_count, 261);
        let diff = version_diff(&guard, second).unwrap().unwrap();
        assert!(diff.contains("+body: rewritten"), "{diff}");
        assert!(diff.contains("+body: 261"), "{diff}");

        // Restore the first snapshot (copy-forward): 260 sections, original body.
        let restored = restore_version(&mut guard, 7, first, false).unwrap();
        let working = load_sections(&guard, 7).unwrap().unwrap();
        assert_eq!(working.len(), 260);
        assert_eq!(working["1"].1, "body: 1\n");
        let versions = list_versions(&guard, 7).unwrap();
        assert_eq!(versions[0].id, restored);
        assert_eq!(versions[0].origin, "rollback");
        assert_eq!(versions.len(), 3);

        // Merge the rewritten section back out of the second snapshot.
        let merged = merge_from_version(
            &mut guard,
            7,
            second,
            Some(&["1".to_string()]),
            Some("take one"),
        )
        .unwrap();
        let working = load_sections(&guard, 7).unwrap().unwrap();
        assert_eq!(working.len(), 260, "unselected sections stay as restored");
        assert_eq!(working["1"].1, "body: rewritten\n");
        let versions = list_versions(&guard, 7).unwrap();
        assert_eq!(versions[0].id, merged);
        assert_eq!(versions[0].origin, "manual");

        // Prune keeps the newest heads and detaches the pruned ones.
        let pruned = prune_history(&guard, 7, 2).unwrap();
        assert_eq!(pruned, 2);
        assert_eq!(list_versions(&guard, 7).unwrap().len(), 2);
    }
}

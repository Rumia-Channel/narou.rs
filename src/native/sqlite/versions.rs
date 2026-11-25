//! P4b version history over the mirrored working set (`novel_sections`).
//!
//! Snapshots are immutable; restore/merge are copy-forward operations that
//! create a new head instead of rewriting history (plan §11.3).
//!
//! Section reads go through the body-joined query in `content` (one query per
//! version instead of one per section); unified diffs and their compression are
//! computed *before* the write transaction opens so brotli/TextDiff CPU does
//! not extend the shared-connection write lock.
//!
//! A snapshot of a working set whose logical relation (`idx`, `subtitle`,
//! `body_hash`) did not change is written without reading any body: the diff
//! is empty by construction and the new head copies the previous head's rows
//! with one `INSERT ... SELECT`.


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
pub fn snapshot_working_set(conn:&mut Connection,novel_id:i64,origin:&str,note:Option<&str>)->Result<i64>{
    let tx=conn.transaction().map_err(sqlite_error)?;let id=snapshot_working_set_in_tx(&tx,novel_id,origin,note)?;tx.commit().map_err(sqlite_error)?;Ok(id)
}

/// Caller owns the transaction and its original novel fencing epoch.
pub fn snapshot_working_set_in_tx(
    conn: &Connection,
    novel_id: i64,
    origin: &str,
    note: Option<&str>,
) -> Result<i64> {
    let prev_head = head_id(conn, novel_id)?;
    // An unchanged working set renders to the same text as the previous head,
    // so its diff is empty and its section rows are the previous head's rows.
    // That case never has to read a body.
    if let Some(version_id) = snapshot_unchanged_in_tx(conn, novel_id, prev_head, origin, note)? {
        return Ok(version_id);
    }
    let Some(current) = super::content::load_sections(conn, novel_id)? else {
        return Err(NarouError::Platform(
            "version snapshot requires a mirrored working set (novel_sections)".into(),
        ));
    };
    let (diff_stored, diff_encoding) = prepare_diff(conn, prev_head, &current)?;

    conn.execute(
        "INSERT INTO novel_versions (novel_id, parent_id, origin, note, created_at) VALUES (?, ?, ?, ?, ?)",
        params![novel_id, prev_head, origin, note, now_rfc3339()],
    ).map_err(sqlite_error)?;
    let version_id=conn.last_insert_rowid();write_version_sections(conn,version_id,&current)?;
    store_diff(conn,version_id,prev_head,&diff_stored,diff_encoding)?;Ok(version_id)
}

/// Snapshot a working set whose logical relation did not change.
///
/// The working set's `(idx, subtitle, body_hash)` rows are compared with the
/// previous head's rows in SQL. Equal rows mean equal rendered text, so the
/// unified diff is empty and the new head carries the previous head's section
/// rows via one `INSERT ... SELECT`. Verify the shared bodies once without
/// loading both novels into memory or rebuilding the full-text diff.
///
/// `None` is returned when the fast path does not apply. The caller then
/// takes the full path, which reads and verifies every body — that path also
/// keeps the "a missing body row is skipped" behaviour, so a working set with
/// a missing body never reaches the copy above.
fn snapshot_unchanged_in_tx(
    conn: &Connection,
    novel_id: i64,
    prev_head: Option<i64>,
    origin: &str,
    note: Option<&str>,
) -> Result<Option<i64>> {
    let Some(prev_id) = prev_head else {
        return Ok(None);
    };
    if !super::content::working_set_matches_version(conn, novel_id, prev_id)? {
        return Ok(None);
    }
    if !super::content::working_set_body_addresses_match(conn, novel_id)? {
        return Ok(None);
    }
    conn.execute(
        "INSERT INTO novel_versions (novel_id, parent_id, origin, note, created_at) VALUES (?, ?, ?, ?, ?)",
        params![novel_id, prev_id, origin, note, now_rfc3339()],
    )
    .map_err(sqlite_error)?;
    let version_id = conn.last_insert_rowid();
    write_version_sections_from_working_set(conn, novel_id, version_id)?;
    // The diff of an unchanged working set is empty. `compress_object_payload`
    // encodes an empty payload as no bytes with the `none` codec, which is
    // exactly what the full path stores for an unchanged set.
    store_diff(conn, version_id, Some(prev_id), &[], ObjectEncoding::None)?;
    Ok(Some(version_id))
}

/// Copy the working set's section rows into one version.
///
/// The working set already resolves every body to its content hash, so the
/// version needs no body read, hash or insert of its own.
fn write_version_sections_from_working_set(
    tx: &Connection,
    novel_id: i64,
    version_id: i64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO novel_version_sections (version_id, idx, subtitle, body_hash)
         SELECT ?1, s.idx, s.subtitle, s.body_hash FROM novel_sections s WHERE s.novel_id = ?2",
        params![version_id, novel_id],
    )
    .map_err(sqlite_error)?;
    Ok(())
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

/// Stored unified diff for a version owned by the requested novel.
pub fn version_diff(conn: &Connection, novel_id: i64, version_id: i64) -> Result<Option<String>> {
    let mut statement = conn
        .prepare("SELECT d.unified_diff, d.encoding FROM novel_version_diffs d JOIN novel_versions v ON v.id = d.version_id WHERE v.id = ? AND v.novel_id = ?")
        .map_err(sqlite_error)?;
    let mut rows = statement.query(params![version_id, novel_id]).map_err(sqlite_error)?;
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
    if deleted > 0 {
        tx.execute(
            "DELETE FROM section_bodies
             WHERE NOT EXISTS (SELECT 1 FROM novel_sections s WHERE s.body_hash = section_bodies.body_hash)
               AND NOT EXISTS (SELECT 1 FROM novel_version_sections v WHERE v.body_hash = section_bodies.body_hash)",
            [],
        )
        .map_err(sqlite_error)?;
    }
    tx.commit().map_err(sqlite_error)?;
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::sqlite::content::{load_sections, store_sections};
    use sha2::{Digest, Sha256};

    /// Fresh in-memory schema plus the minimal novel row the version tables'
    /// foreign key needs.
    fn test_conn() -> parking_lot::Mutex<Connection> {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::native::sqlite::migrations::apply(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO novels (id, author, author_fold, title, title_fold, file_title, toc_url, toc_url_fold, sitename, sitename_fold, last_update)
             VALUES (7, 'a', 'a', 't', 't', 'ft', 'https://x/7', 'https://x/7', 's', 's', '2026-01-01T00:00:00Z'),
                    (8, 'a', 'a', 'other', 'other', 'other', 'https://x/8', 'https://x/8', 's', 's', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        parking_lot::Mutex::new(conn)
    }

    /// Five ordered sections with distinct bodies.
    fn sample_sections() -> SectionMap {
        (1..=5)
            .map(|index| {
                (
                    index.to_string(),
                    (Some(format!("第{index}話")), format!("body: {index}\n")),
                )
            })
            .collect()
    }

    /// The rows of one version, in `idx` order.
    fn version_rows(conn: &Connection, version_id: i64) -> Vec<(String, Option<String>, Vec<u8>)> {
        let mut statement = conn
            .prepare(
                "SELECT idx, subtitle, body_hash FROM novel_version_sections WHERE version_id = ?1 ORDER BY idx",
            )
            .unwrap();
        let rows = statement
            .query_map([version_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        let mut out = Vec::new();
        for row in rows {
            out.push(row.unwrap());
        }
        out
    }

    /// The stored diff of one version: payload bytes and codec.
    fn diff_row(conn: &Connection, version_id: i64) -> (Vec<u8>, String) {
        conn.query_row(
            "SELECT unified_diff, encoding FROM novel_version_diffs WHERE version_id = ?1",
            [version_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
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
        let diff = version_diff(&guard, 7, second).unwrap().unwrap();
        assert!(diff.contains("+body: rewritten"), "{diff}");
        assert!(diff.contains("+body: 261"), "{diff}");
        assert_eq!(version_diff(&guard, 8, second).unwrap(), None);
        assert_eq!(version_diff(&guard, 7, i64::MAX).unwrap(), None);

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

    /// An unchanged working set still records one history row: the new head
    /// keeps the previous head as parent, carries the same sections and
    /// stores the empty diff with the exact encoding of an empty payload.
    #[test]
    fn unchanged_working_set_snapshots_an_empty_diff() {
        let conn = test_conn();
        let mut guard = conn.lock();
        let sections = sample_sections();
        store_sections(&mut guard, 7, &sections).unwrap();
        let first = snapshot_working_set(&mut guard, 7, "update", None).unwrap();
        let second = snapshot_working_set(&mut guard, 7, "update", Some("convert")).unwrap();

        let versions = list_versions(&guard, 7).unwrap();
        assert_eq!(versions.len(), 2, "one history row per snapshot");
        assert_eq!(versions[0].id, second);
        assert_eq!(versions[0].parent_id, Some(first));
        assert_eq!(versions[0].origin, "update");
        assert_eq!(versions[0].note.as_deref(), Some("convert"));
        assert_eq!(versions[0].section_count, 5);
        assert_eq!(version_sections(&guard, second).unwrap().unwrap(), sections);

        // The empty diff is stored like the full path stores it: no bytes
        // with the `none` codec, and it reads back as an empty diff.
        let (payload, encoding) = diff_row(&guard, second);
        assert!(payload.is_empty(), "{payload:?}");
        assert_eq!(encoding, "none");
        assert_eq!(version_diff(&guard, 7, second).unwrap().unwrap(), "");
    }

    /// The unchanged snapshot stores what the full path stores: the same
    /// section rows and the same diff encoding for the same content.
    #[test]
    fn unchanged_snapshot_matches_the_full_path_output() {
        let conn = test_conn();
        let mut guard = conn.lock();
        let sections = sample_sections();
        store_sections(&mut guard, 7, &sections).unwrap();
        snapshot_working_set(&mut guard, 7, "update", None).unwrap();
        let unchanged = snapshot_working_set(&mut guard, 7, "update", None).unwrap();

        // The same content snapshotted as a first version takes the full path.
        store_sections(&mut guard, 8, &sections).unwrap();
        let reference = snapshot_working_set(&mut guard, 8, "update", None).unwrap();

        assert_eq!(
            version_rows(&guard, unchanged),
            version_rows(&guard, reference)
        );
        assert_eq!(diff_row(&guard, unchanged), diff_row(&guard, reference));
        assert_eq!(
            version_sections(&guard, unchanged).unwrap(),
            version_sections(&guard, reference).unwrap()
        );
    }

    /// A changed working set keeps the full diff: a rewritten body and a
    /// changed subtitle both change the rendered text, so the snapshot must
    /// not take the unchanged path.
    #[test]
    fn changed_working_set_keeps_the_full_diff() {
        let conn = test_conn();
        let mut guard = conn.lock();
        let sections = sample_sections();
        store_sections(&mut guard, 7, &sections).unwrap();
        snapshot_working_set(&mut guard, 7, "update", None).unwrap();

        let mut changed = sections.clone();
        changed.insert(
            "2".to_string(),
            (Some("第二章".to_string()), "body: rewritten\n".to_string()),
        );
        store_sections(&mut guard, 7, &changed).unwrap();
        let second = snapshot_working_set(&mut guard, 7, "update", None).unwrap();

        let diff = version_diff(&guard, 7, second).unwrap().unwrap();
        assert!(diff.contains("-=== 2 第2話"), "{diff}");
        assert!(diff.contains("+=== 2 第二章"), "{diff}");
        assert!(diff.contains("-body: 2"), "{diff}");
        assert!(diff.contains("+body: rewritten"), "{diff}");
        assert_eq!(version_sections(&guard, second).unwrap().unwrap(), changed);
    }

    /// A working set whose body row vanished is not unchanged: the version
    /// skips that section like the body-joined read does, and the version
    /// never links a body that does not exist.
    #[test]
    fn snapshot_skips_a_missing_body_instead_of_linking_it() {
        let conn = test_conn();
        let mut guard = conn.lock();
        let sections = sample_sections();
        store_sections(&mut guard, 7, &sections).unwrap();
        snapshot_working_set(&mut guard, 7, "update", None).unwrap();

        let hash = Sha256::digest(b"body: 2\n").to_vec();
        guard
            .execute("DELETE FROM section_bodies WHERE body_hash = ?", [&hash])
            .unwrap();
        let second = snapshot_working_set(&mut guard, 7, "update", None).unwrap();

        let stored = version_sections(&guard, second).unwrap().unwrap();
        assert_eq!(stored.len(), 4, "the section without a body is skipped");
        assert!(!stored.contains_key("2"));
        let linked: i64 = guard
            .query_row(
                "SELECT COUNT(*) FROM novel_version_sections WHERE version_id = ?1 AND body_hash = ?2",
                params![second, hash],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(linked, 0, "a missing body must not be linked");
        assert_eq!(list_versions(&guard, 7).unwrap()[0].section_count, 4);
        // The missing body is invisible on both sides of the diff: the
        // previous head's row for that section links the same missing body,
        // so the section drops out of the comparison as well.
        let diff = version_diff(&guard, 7, second).unwrap().unwrap();
        assert_eq!(diff, "", "{diff}");
    }

    /// The unchanged snapshot is written inside the caller's transaction: a
    /// rollback takes the new head, its rows and its diff with it.
    #[test]
    fn unchanged_snapshot_rolls_back_with_the_callers_transaction() {
        let conn = test_conn();
        let mut guard = conn.lock();
        let sections = sample_sections();
        store_sections(&mut guard, 7, &sections).unwrap();
        snapshot_working_set(&mut guard, 7, "update", None).unwrap();

        let tx = guard.transaction().unwrap();
        snapshot_working_set_in_tx(&tx, 7, "update", Some("convert")).unwrap();
        tx.rollback().unwrap();

        let versions = list_versions(&guard, 7).unwrap();
        assert_eq!(versions.len(), 1, "the rolled back head is gone");
        assert_eq!(load_sections(&guard, 7).unwrap().unwrap(), sections);
    }

    #[test]
    fn unchanged_snapshot_still_rejects_damaged_bodies() {
        let conn = test_conn();
        let mut guard = conn.lock();
        let sections = sample_sections();
        store_sections(&mut guard, 7, &sections).unwrap();
        snapshot_working_set(&mut guard, 7, "update", None).unwrap();

        guard
            .execute("UPDATE section_bodies SET crc32 = crc32 + 1", [])
            .unwrap();
        let error = snapshot_working_set(&mut guard, 7, "update", None).unwrap_err();
        assert!(error.to_string().contains("CRC"), "{error}");
        assert_eq!(list_versions(&guard, 7).unwrap().len(), 1);
    }
}

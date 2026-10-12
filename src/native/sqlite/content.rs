//! P4 content + version history storage.
//!
//! Working set contract: the authoritative sections stay in the ObjectStore
//! (`本文/*.yaml`); `novel_sections` mirrors them per novel so download-time
//! EPUB and version tooling can run without touching the filesystem.
//!
//! Section bodies are content-addressed in `section_bodies` (migration 0009):
//! `novel_sections`/`novel_version_sections` store only the SHA-256
//! `body_hash`, so identical bodies across versions — and across novels —
//! are stored once, brotli-compressed. `novel_outputs` payloads and
//! `novel_version_diffs.unified_diff` are likewise compressed when their
//! `encoding` column is not 'none'.
//!
//! Reads join `section_bodies` in the section query and batch writes resolve
//! hashes with one `IN (...)` lookup per chunk, so neither path runs a query
//! per section.

use std::collections::{BTreeMap, HashSet};

use chrono::Utc;
use rusqlite::{Connection, params, params_from_iter};
use sha2::{Digest, Sha256};

use crate::error::{NarouError, Result};
use crate::platform::{
    ObjectEncoding, compress_object_payload, decompress_object_payload, object_crc32,
    verify_object_crc32,
};

use super::sqlite_error;

/// Maximum number of hashes bound into one existence-check statement.
/// Chunking keeps the statement small (well under SQLite's variable limit)
/// while collapsing thousands of per-row probes into a handful of queries.
const BODY_LOOKUP_CHUNK: usize = 200;

/// Insert `body` into `section_bodies` if absent; returns its SHA-256 hash.
/// Runs inside the caller's transaction. Single-body wrapper around
/// [`store_bodies`].
pub fn store_body(tx: &Connection, body: &str) -> Result<Vec<u8>> {
    Ok(store_bodies(tx, &[body])?
        .pop()
        .expect("one hash for one body"))
}

/// Insert every distinct body once and return the hashes in input order.
///
/// Existing rows are found with one `IN (...)` query per chunk instead of a
/// `SELECT` per body, and only bodies that are actually new are compressed and
/// inserted. `ON CONFLICT DO NOTHING` keeps reruns of a batch safe.
pub fn store_bodies(tx: &Connection, bodies: &[&str]) -> Result<Vec<Vec<u8>>> {
    let hashes: Vec<Vec<u8>> = bodies
        .iter()
        .map(|body| Sha256::digest(body.as_bytes()).to_vec())
        .collect();

    let mut known: HashSet<Vec<u8>> = HashSet::new();
    for chunk in hashes.chunks(BODY_LOOKUP_CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql =
            format!("SELECT body_hash FROM section_bodies WHERE body_hash IN ({placeholders})");
        let mut statement = tx.prepare(&sql).map_err(sqlite_error)?;
        let rows = statement
            .query_map(params_from_iter(chunk.iter().map(|hash| hash.as_slice())), |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .map_err(sqlite_error)?;
        for row in rows {
            known.insert(row.map_err(|error| NarouError::Platform(error.to_string()))?);
        }
    }

    let mut insert = tx
        .prepare_cached(
            "INSERT INTO section_bodies (body_hash, size, encoding, crc32, data)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(body_hash) DO NOTHING",
        )
        .map_err(sqlite_error)?;
    let mut pending: HashSet<&[u8]> = HashSet::new();
    for (index, body) in bodies.iter().enumerate() {
        let hash = &hashes[index];
        if known.contains(hash) || !pending.insert(hash.as_slice()) {
            continue;
        }
        let bytes = body.as_bytes();
        let (stored, encoding) = compress_object_payload(bytes);
        insert
            .execute(params![
                hash,
                bytes.len() as i64,
                encoding.as_str(),
                object_crc32(bytes) as i64,
                stored
            ])
            .map_err(sqlite_error)?;
    }
    Ok(hashes)
}

/// セクション本文の写し: `idx` → (`subtitle`, `body_yaml`)。working set
/// (`novel_sections`) と version snapshot (`novel_version_sections`) で共通。
pub type SectionMap = BTreeMap<String, (Option<String>, String)>;

/// Store (or replace) the mirrored section set of one novel.
pub fn store_sections(conn: &mut Connection, novel_id: i64, sections: &SectionMap) -> Result<()> {
    let tx = conn.transaction().map_err(sqlite_error)?;
    store_sections_in_tx(&tx, novel_id, sections)?;
    tx.commit().map_err(sqlite_error)
}

/// [`store_sections`] inside an existing transaction. Restore/merge record the
/// new version head in the same transaction as the working-set swap.
pub(crate) fn store_sections_in_tx(
    tx: &Connection,
    novel_id: i64,
    sections: &SectionMap,
) -> Result<()> {
    // sections: idx -> (subtitle, body_yaml)
    tx.execute("DELETE FROM novel_sections WHERE novel_id = ?", [novel_id])
        .map_err(sqlite_error)?;
    let bodies: Vec<&str> = sections
        .values()
        .map(|(_, body)| body.as_str())
        .collect();
    let hashes = store_bodies(tx, &bodies)?;
    let mut insert = tx
        .prepare_cached(
            "INSERT INTO novel_sections (novel_id, idx, subtitle, body_hash) VALUES (?, ?, ?, ?)",
        )
        .map_err(sqlite_error)?;
    for ((idx, (subtitle, _)), hash) in sections.iter().zip(hashes.iter()) {
        insert
            .execute(params![novel_id, idx, subtitle, hash])
            .map_err(sqlite_error)?;
    }
    Ok(())
}

/// Load the mirrored section set; `Ok(None)` when nothing is mirrored yet
/// (caller falls back to the ObjectStore / lazy migration).
pub fn load_sections(conn: &Connection, novel_id: i64) -> Result<Option<SectionMap>> {
    load_section_map(conn, "novel_sections", "novel_id", novel_id)
}

/// Body-joined read shared by `novel_sections` and
/// `novel_version_sections`: one query fetches each section together with the
/// stored body instead of one `SELECT` per section. Rows whose body row is
/// missing are skipped, exactly like the previous per-row lookup.
///
/// `table`/`owner_column` are compile-time literals from this module; they are
/// never built from input.
pub(crate) fn load_section_map(
    conn: &Connection,
    table: &str,
    owner_column: &str,
    owner_id: i64,
) -> Result<Option<SectionMap>> {
    let sql = format!(
        "SELECT s.idx, s.subtitle, b.data, b.encoding, b.crc32
         FROM {table} s JOIN section_bodies b ON b.body_hash = s.body_hash
         WHERE s.{owner_column} = ? ORDER BY s.idx"
    );
    let mut statement = conn.prepare(&sql).map_err(sqlite_error)?;
    let rows = statement
        .query_map([owner_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(sqlite_error)?;
    let mut map = BTreeMap::new();
    for row in rows {
        let (idx, subtitle, stored, encoding, crc32) =
            row.map_err(|error| NarouError::Platform(error.to_string()))?;
        let body = decode_section_body(&stored, &encoding, crc32)?;
        map.insert(idx, (subtitle, body));
    }
    if map.is_empty() {
        Ok(None)
    } else {
        Ok(Some(map))
    }
}

/// Decompress one stored section body and verify its CRC-32.
fn decode_section_body(stored: &[u8], encoding: &str, crc32: i64) -> Result<String> {
    let encoding = ObjectEncoding::parse(encoding)?;
    let data = decompress_object_payload(stored, encoding)?;
    verify_object_crc32(&data, crc32 as u32)?;
    String::from_utf8(data)
        .map_err(|error| NarouError::Platform(format!("section body is not UTF-8: {error}")))
}

/// Store a generated output blob (`kind` examples: `converted_text`).
/// Compressed when brotli shrinks it; `encoding` records the codec.
pub fn store_output(conn: &Connection, novel_id: i64, kind: &str, payload: &[u8]) -> Result<()> {
    let (stored, encoding) = compress_object_payload(payload);
    conn.execute(
        "INSERT INTO novel_outputs (novel_id, kind, payload, updated_at, encoding)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(novel_id, kind) DO UPDATE SET
             payload = excluded.payload, updated_at = excluded.updated_at,
             encoding = excluded.encoding",
        params![
            novel_id,
            kind,
            stored,
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            encoding.as_str()
        ],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

/// Remove a generated output blob (`kind` examples: `converted_text`).
///
/// `convert.keep-txt=false` (変換済みテキストを保存しない構成) で、以前の
/// 実行が残した行を掃除するために使う。
pub fn delete_output(conn: &Connection, novel_id: i64, kind: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM novel_outputs WHERE novel_id = ? AND kind = ?",
        params![novel_id, kind],
    )
    .map_err(sqlite_error)?;
    Ok(())
}


pub fn load_output(conn: &Connection, novel_id: i64, kind: &str) -> Result<Option<Vec<u8>>> {
    let mut statement = conn
        .prepare("SELECT payload, encoding FROM novel_outputs WHERE novel_id = ? AND kind = ?")
        .map_err(sqlite_error)?;
    let mut rows = statement
        .query(params![novel_id, kind])
        .map_err(sqlite_error)?;
    match rows.next().map_err(sqlite_error)? {
        Some(row) => {
            let payload: Vec<u8> = row
                .get(0)
                .map_err(|error| NarouError::Platform(error.to_string()))?;
            let encoding: String = row
                .get(1)
                .map_err(|error| NarouError::Platform(error.to_string()))?;
            let encoding = ObjectEncoding::parse(&encoding)?;
            Ok(Some(decompress_object_payload(&payload, encoding)?))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fresh in-memory schema plus the minimal novel row the section tables'
    /// foreign key needs. A `parking_lot` mutex gives the tests a guard
    /// without the poisoning unwrap (the production handle keeps its
    /// documented `Arc<std::sync::Mutex<Connection>>` type).
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

    /// A section set larger than the hash-lookup chunk still round-trips in
    /// order, and identical bodies are stored once.
    #[test]
    fn sections_round_trip_in_order_with_deduplicated_bodies() {
        let conn = test_conn();
        let mut guard = conn.lock();
        let mut sections = SectionMap::new();
        for index in 1..=450 {
            // Only three distinct bodies, so dedup must collapse them.
            sections.insert(
                index.to_string(),
                (
                    Some(format!("第{index}話")),
                    format!("body: {}\n", index % 3),
                ),
            );
        }
        store_sections(&mut guard, 7, &sections).unwrap();

        let bodies: i64 = guard
            .query_row("SELECT COUNT(*) FROM section_bodies", [], |row| row.get(0))
            .unwrap();
        assert_eq!(bodies, 3, "identical bodies are stored once");

        let loaded = load_sections(&guard, 7).unwrap().unwrap();
        assert_eq!(loaded, sections);
    }

    /// A section whose body row vanished is skipped so the caller can fall
    /// back to the ObjectStore; the remaining sections still load, and a novel
    /// with no mirrored rows reports `None`.
    #[test]
    fn load_sections_skips_missing_body_rows() {
        let conn = test_conn();
        let mut guard = conn.lock();
        let mut sections = SectionMap::new();
        sections.insert("1".to_string(), (Some("一".into()), "body: one\n".into()));
        sections.insert("2".to_string(), (Some("二".into()), "body: two\n".into()));
        store_sections(&mut guard, 7, &sections).unwrap();

        let hash = Sha256::digest(b"body: one\n").to_vec();
        guard
            .execute("DELETE FROM section_bodies WHERE body_hash = ?", [hash])
            .unwrap();

        let loaded = load_sections(&guard, 7).unwrap().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["2"].1, "body: two\n");

        guard
            .execute("DELETE FROM novel_sections WHERE novel_id = 7", [])
            .unwrap();
        assert!(load_sections(&guard, 7).unwrap().is_none());
    }

    /// A body row whose stored CRC no longer matches fails loudly on read; the
    /// read path must not hand back corrupted text.
    #[test]
    fn load_sections_reports_crc_mismatch() {
        let conn = test_conn();
        let mut guard = conn.lock();
        let mut sections = SectionMap::new();
        sections.insert("1".to_string(), (None, "body: one\n".into()));
        store_sections(&mut guard, 7, &sections).unwrap();

        guard
            .execute("UPDATE section_bodies SET crc32 = crc32 + 1", [])
            .unwrap();
        let error = load_sections(&guard, 7).unwrap_err();
        assert!(error.to_string().contains("CRC"), "{error}");
    }
}

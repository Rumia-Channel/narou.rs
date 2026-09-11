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

use std::collections::BTreeMap;

use chrono::Utc;
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};

use crate::error::{NarouError, Result};
use crate::platform::{
    compress_object_payload, decompress_object_payload, object_crc32, verify_object_crc32,
    ObjectEncoding,
};

use super::sqlite_error;

/// Insert `body` into `section_bodies` if absent; returns its SHA-256 hash.
/// Runs inside the caller's transaction.
pub fn store_body(tx: &Connection, body: &str) -> Result<Vec<u8>> {
    let hash = Sha256::digest(body.as_bytes()).to_vec();
    let exists: bool = tx
        .query_row(
            "SELECT 1 FROM section_bodies WHERE body_hash = ?",
            [&hash],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !exists {
        let (stored, encoding) = compress_object_payload(body.as_bytes());
        tx.execute(
            "INSERT INTO section_bodies (body_hash, size, encoding, crc32, data)
             VALUES (?, ?, ?, ?, ?)",
            params![
                hash,
                body.len() as i64,
                encoding.as_str(),
                object_crc32(body.as_bytes()) as i64,
                stored
            ],
        )
        .map_err(sqlite_error)?;
    }
    Ok(hash)
}

/// Load a body by hash; verifies the stored CRC-32 after decompression.
pub fn load_body(conn: &Connection, hash: &[u8]) -> Result<Option<String>> {
    let mut statement = conn
        .prepare("SELECT data, encoding, crc32 FROM section_bodies WHERE body_hash = ?")
        .map_err(sqlite_error)?;
    let mut rows = statement.query([hash]).map_err(sqlite_error)?;
    let Some(row) = rows.next().map_err(sqlite_error)? else {
        return Ok(None);
    };
    let stored: Vec<u8> = row.get(0).map_err(sqlite_error)?;
    let encoding: String = row.get(1).map_err(sqlite_error)?;
    let crc32: i64 = row.get(2).map_err(sqlite_error)?;
    let encoding = ObjectEncoding::parse(&encoding)?;
    let data = decompress_object_payload(&stored, encoding)?;
    verify_object_crc32(&data, crc32 as u32)?;
    String::from_utf8(data)
        .map(Some)
        .map_err(|error| NarouError::Platform(format!("section body is not UTF-8: {error}")))
}

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
        let hash = store_body(&tx, body)?;
        tx.execute(
            "INSERT INTO novel_sections (novel_id, idx, subtitle, body_hash) VALUES (?, ?, ?, ?)",
            params![novel_id, idx, subtitle, hash],
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
        .prepare("SELECT idx, subtitle, body_hash FROM novel_sections WHERE novel_id = ? ORDER BY idx")
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map([novel_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<Vec<u8>>>(2)?,
            ))
        })
        .map_err(sqlite_error)?;
    let mut map = BTreeMap::new();
    for row in rows {
        let (idx, subtitle, hash) =
            row.map_err(|error| NarouError::Platform(error.to_string()))?;
        let Some(hash) = hash else { continue };
        let Some(body) = load_body(conn, &hash)? else {
            continue;
        };
        map.insert(idx, (subtitle, body));
    }
    if map.is_empty() {
        Ok(None)
    } else {
        Ok(Some(map))
    }
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

pub fn load_output(conn: &Connection, novel_id: i64, kind: &str) -> Result<Option<Vec<u8>>> {
    let mut statement = conn
        .prepare("SELECT payload, encoding FROM novel_outputs WHERE novel_id = ? AND kind = ?")
        .map_err(sqlite_error)?;
    let mut rows = statement
        .query(params![novel_id, kind])
        .map_err(sqlite_error)?;
    match rows.next().map_err(sqlite_error)? {
        Some(row) => {
            let payload: Vec<u8> =
                row.get(0).map_err(|error| NarouError::Platform(error.to_string()))?;
            let encoding: String = row
                .get(1)
                .map_err(|error| NarouError::Platform(error.to_string()))?;
            let encoding = ObjectEncoding::parse(&encoding)?;
            Ok(Some(decompress_object_payload(&payload, encoding)?))
        }
        None => Ok(None),
    }
}

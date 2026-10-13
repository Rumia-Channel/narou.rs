use std::borrow::Cow;

use narou_rs::error::{NarouError, Result};
use narou_rs::platform::{ObjectEncoding, decode_object_payload_limited, verify_object_crc32};

// One SQL snapshot includes both the header and every chunk. Return the inline
// BLOB only once, even if a legacy object retained unused chunk rows.
pub(crate) const READ_SQL: &str =
    "SELECT o.size, o.updated_at, o.content_type, o.encoding, o.crc32,
       CASE WHEN ROW_NUMBER() OVER (ORDER BY c.seq) = 1 THEN o.data ELSE NULL END,
       c.seq, c.data
     FROM objects o LEFT JOIN object_chunks c ON c.object_key = o.object_key
     WHERE o.object_key = ? ORDER BY c.seq";

// D1 batch is one transaction. Resetting this private guard before its CAS
// needs neither a schema migration nor a random per-attempt token.
pub(crate) const RESET_SQL: &str =
    "DELETE FROM app_state WHERE scope = 'inv' AND key = ?1";

pub(crate) const AUTHORIZE_SQL: &str =
    "INSERT INTO app_state(scope, key, value_yaml, value_json)
     SELECT 'inv', ?1, '1', '{}'
     WHERE CASE WHEN ?3 THEN EXISTS(
       SELECT 1 FROM objects WHERE object_key = ?2
         AND size IS ?4 AND updated_at IS ?5 AND content_type IS ?6
         AND encoding IS ?7 AND crc32 IS ?8 AND data IS ?9)
       ELSE NOT EXISTS(SELECT 1 FROM objects WHERE object_key = ?2) END";

pub(crate) const CHECK_CHUNK_COUNT_SQL: &str =
    "DELETE FROM app_state WHERE scope = 'inv' AND key = ?1
       AND (SELECT COUNT(*) FROM object_chunks WHERE object_key = ?2) <> ?3";

pub(crate) const CHECK_CHUNK_SQL: &str =
    "DELETE FROM app_state WHERE scope = 'inv' AND key = ?1
       AND NOT EXISTS(SELECT 1 FROM object_chunks
         WHERE object_key = ?2 AND seq = ?3 AND data IS ?4)";

pub(crate) const FINISH_SQL: &str =
    "DELETE FROM app_state WHERE scope = 'inv' AND key = ?1 RETURNING 1 AS committed";

pub(crate) const WRITE_INLINE_SQL: &str =
    "INSERT INTO objects(object_key, size, updated_at, content_type, encoding, crc32, data)
     SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7
     WHERE EXISTS(SELECT 1 FROM app_state WHERE scope = 'inv' AND key = ?8)
     ON CONFLICT(object_key) DO UPDATE SET size = excluded.size, updated_at = excluded.updated_at,
       content_type = excluded.content_type, encoding = excluded.encoding,
       crc32 = excluded.crc32, data = excluded.data";

pub(crate) const WRITE_CHUNKED_SQL: &str =
    "INSERT INTO objects(object_key, size, updated_at, content_type, encoding, crc32, data)
     SELECT ?1, ?2, ?3, ?4, ?5, ?6, NULL
     WHERE EXISTS(SELECT 1 FROM app_state WHERE scope = 'inv' AND key = ?7)
     ON CONFLICT(object_key) DO UPDATE SET size = excluded.size, updated_at = excluded.updated_at,
       content_type = excluded.content_type, encoding = excluded.encoding,
       crc32 = excluded.crc32, data = NULL";

pub(crate) const DELETE_CHUNKS_SQL: &str =
    "DELETE FROM object_chunks WHERE object_key = ?1
       AND EXISTS(SELECT 1 FROM app_state WHERE scope = 'inv' AND key = ?2)";

pub(crate) const WRITE_CHUNK_SQL: &str =
    "INSERT INTO object_chunks(object_key, seq, data)
     SELECT ?1, ?2, ?3
     WHERE EXISTS(SELECT 1 FROM app_state WHERE scope = 'inv' AND key = ?4)";

pub(crate) struct Header {
    pub(crate) size: i64,
    pub(crate) updated_at: String,
    pub(crate) content_type: Option<String>,
    pub(crate) encoding: String,
    pub(crate) crc32: i64,
    pub(crate) data: Option<Vec<u8>>,
}

pub(crate) struct Chunk {
    pub(crate) seq: i64,
    pub(crate) data: Vec<u8>,
}

#[derive(Default)]
pub(crate) struct Snapshot {
    pub(crate) header: Option<Header>,
    pub(crate) chunks: Vec<Chunk>,
}

impl Snapshot {
    pub(crate) fn payload(&self, limit: usize) -> Result<Option<Cow<'_, [u8]>>> {
        let Some(header) = &self.header else { return Ok(None) };
        if header.size < 0 || header.size as u64 > limit as u64 {
            return Err(NarouError::Platform("object exceeds the small-read cap".into()));
        }
        let encoding = ObjectEncoding::parse(&header.encoding)?;
        let stored = if let Some(data) = &header.data {
            Cow::Borrowed(data.as_slice())
        } else {
            let length = self.chunks.iter().try_fold(0usize, |length, chunk| {
                length.checked_add(chunk.data.len()).filter(|length| *length <= limit)
                    .ok_or_else(|| NarouError::Platform("stored object exceeds the read cap".into()))
            })?;
            let mut data = Vec::with_capacity(length);
            for chunk in &self.chunks { data.extend_from_slice(&chunk.data); }
            Cow::Owned(data)
        };
        let decoded = if encoding == ObjectEncoding::None {
            if stored.len() > limit {
                return Err(NarouError::Platform("stored object exceeds the read cap".into()));
            }
            stored
        } else {
            Cow::Owned(decode_object_payload_limited(&stored, encoding, limit)?.into_owned())
        };
        verify_object_crc32(&decoded, header.crc32 as u32)?;
        if decoded.len() as i64 != header.size {
            return Err(NarouError::Platform("object payload size mismatch".into()));
        }
        Ok(Some(decoded))
    }

    pub(crate) fn into_payload(self, limit: usize) -> Result<Option<Vec<u8>>> {
        match self.payload(limit)? {
            None => return Ok(None),
            Some(Cow::Owned(data)) => return Ok(Some(data)),
            Some(Cow::Borrowed(_)) => {}
        }
        Ok(Some(self.header.expect("borrowed payload has a header").data
            .expect("borrowed payload is inline")))
    }
}

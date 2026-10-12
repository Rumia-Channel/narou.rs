-- Preserve objects, including rows without a filesystem mirror.
-- `migrations::apply_one` converts the original base64 TEXT schema to
-- BLOB payloads and adds CRC-32 in this transaction. Databases already
-- using BLOB+crc32 require no object-table changes.

-- Section-body deduplication (P4c): novel_sections and
-- novel_version_sections previously stored a full body_yaml copy per row,
-- duplicating the same text across every version snapshot. Bodies move to
-- the content-addressed `section_bodies` table (one row per unique body,
-- brotli-compressed); both referencing tables keep only the hash.
--
-- The data migration (reading body_yaml rows, hashing, compressing,
-- inserting into section_bodies, setting body_hash) runs in Rust inside
-- `migrations::apply` between 0009 and 0010 — it needs hashing/compression,
-- not pure SQL. Migration 0010 then drops the legacy body_yaml columns.

CREATE TABLE IF NOT EXISTS section_bodies (
    body_hash   BLOB PRIMARY KEY,          -- SHA-256 of the uncompressed YAML
    size        INTEGER NOT NULL,          -- uncompressed byte length
    encoding    TEXT NOT NULL DEFAULT 'none',
    crc32       INTEGER NOT NULL DEFAULT 0,
    data        BLOB NOT NULL
) WITHOUT ROWID;

ALTER TABLE novel_sections ADD COLUMN body_hash BLOB;
ALTER TABLE novel_version_sections ADD COLUMN body_hash BLOB;

-- novel_outputs payloads (converted_text etc.) gain a codec column; the
-- payload BLOB stores compressed bytes when encoding != 'none'.
ALTER TABLE novel_outputs ADD COLUMN encoding TEXT NOT NULL DEFAULT 'none';

-- unified_diff stores compressed bytes when encoding != 'none'.
ALTER TABLE novel_version_diffs ADD COLUMN encoding TEXT NOT NULL DEFAULT 'none';

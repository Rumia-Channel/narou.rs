-- Rebuild the objects tables under the BLOB+crc32 schema. The first 0008
-- revision stored base64 TEXT without encoding/crc32; rather than convert
-- in place, drop and let `configure`'s empty-table check re-import every
-- file from the filesystem mirror (which always holds the same bytes).
-- Worker D1 databases never saw the base64 revision in production.
DROP TABLE IF EXISTS object_chunks;
DROP TABLE IF EXISTS objects;

CREATE TABLE objects (
    object_key  TEXT PRIMARY KEY,
    size        INTEGER NOT NULL,
    updated_at  TEXT NOT NULL,
    content_type TEXT,
    encoding    TEXT NOT NULL DEFAULT 'none',
    crc32       INTEGER NOT NULL DEFAULT 0,
    data        BLOB
) WITHOUT ROWID;

CREATE TABLE object_chunks (
    object_key  TEXT NOT NULL REFERENCES objects(object_key) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,
    data        BLOB NOT NULL,
    PRIMARY KEY (object_key, seq)
) WITHOUT ROWID;

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

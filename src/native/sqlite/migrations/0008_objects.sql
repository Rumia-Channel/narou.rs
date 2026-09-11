-- Generic object storage: the 小説データ/ tree (sections, raw HTML, toc,
-- setting.ini, replace.txt, illustrations) and generated assets (EPUB etc.)
-- as logical-keyed rows so the Worker runtime needs no object-storage bucket.
--
-- Payloads are stored base64-encoded in TEXT columns so the same schema and
-- row format work identically on local SQLite (rusqlite) and Cloudflare D1
-- (workers-rs serde path), where BLOB↔Vec<u8> conversion is unreliable.
-- Payloads up to 512 KiB live inline in `objects.data`; larger payloads set
-- `objects.data = NULL` and store all bytes in `object_chunks` (seq 0..N,
-- 512 KiB each). 512 KiB keeps every statement and row comfortably under
-- D1's limits even after base64 inflation (~683 KiB worst case).

CREATE TABLE IF NOT EXISTS objects (
    object_key  TEXT PRIMARY KEY,
    size        INTEGER NOT NULL,
    updated_at  TEXT NOT NULL,
    content_type TEXT,
    data        TEXT
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS object_chunks (
    object_key  TEXT NOT NULL REFERENCES objects(object_key) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,
    data        TEXT NOT NULL,
    PRIMARY KEY (object_key, seq)
) WITHOUT ROWID;

-- Prefix scans use a key range
-- (object_key >= prefix AND object_key < prefix || '0'), so the primary
-- key index suffices; no extra index needed.

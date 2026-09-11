-- Generic object storage: the 小説データ/ tree (sections, raw HTML, toc,
-- setting.ini, replace.txt, illustrations) and generated assets (EPUB etc.)
-- as logical-keyed rows so the Worker runtime needs no object-storage bucket.
--
-- Payloads are stored as BLOBs (raw bytes on rusqlite, Uint8Array on D1 via
-- raw_js_value) — no base64 inflation. `encoding` records the payload codec:
-- 'none' or 'brotli' (applied to the whole payload before chunking, so chunk
-- boundaries never split a brotli stream). `crc32` is the CRC-32 of the
-- *uncompressed* payload for corruption detection; `size` is the
-- uncompressed length.
--
-- Payloads whose stored (compressed) size fits in 512 KiB live inline in
-- `objects.data`; larger payloads set `objects.data = NULL` and store all
-- bytes in `object_chunks` (seq 0..N, 512 KiB each). 512 KiB keeps every
-- statement and row comfortably under D1's limits.

CREATE TABLE IF NOT EXISTS objects (
    object_key  TEXT PRIMARY KEY,
    size        INTEGER NOT NULL,
    updated_at  TEXT NOT NULL,
    content_type TEXT,
    encoding    TEXT NOT NULL DEFAULT 'none',
    crc32       INTEGER NOT NULL DEFAULT 0,
    data        BLOB
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS object_chunks (
    object_key  TEXT NOT NULL REFERENCES objects(object_key) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,
    data        BLOB NOT NULL,
    PRIMARY KEY (object_key, seq)
) WITHOUT ROWID;

-- Prefix scans use a key range
-- (object_key >= prefix AND object_key < prefix || '0'), so the primary
-- key index suffices; no extra index needed.

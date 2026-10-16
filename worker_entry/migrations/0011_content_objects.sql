-- Episode bodies/history move independently of illustration storage. S3 bytes
-- are immutable; publishing or deleting the canonical reference is a D1 commit.
CREATE TABLE content_objects (
    object_key TEXT PRIMARY KEY,
    blob_key TEXT NOT NULL UNIQUE,
    size INTEGER NOT NULL CHECK (size >= 0 AND size <= 16777216),
    stored_size INTEGER NOT NULL CHECK (stored_size >= 0 AND stored_size <= 16777216),
    encoding TEXT NOT NULL CHECK (encoding IN ('none', 'brotli')),
    crc32 INTEGER NOT NULL,
    sha256 TEXT NOT NULL CHECK (length(sha256) = 64),
    updated_at TEXT NOT NULL,
    content_type TEXT,
    verified INTEGER NOT NULL DEFAULT 0 CHECK (verified IN (0, 1))
) WITHOUT ROWID;

-- An upload is reserved here before PUT. Publication removes the reservation
-- in the same transaction that queues the superseded revision. Crashes cannot
-- lose required cleanup; the delay protects an upload still in progress.
CREATE TABLE content_garbage (
    blob_key TEXT PRIMARY KEY,
    not_before TEXT NOT NULL
) WITHOUT ROWID;

-- Transaction-local authorizations: inserted/deleted within one D1 batch.
-- Old runtimes that still hold a D1ObjectStore cannot delete migrated content.
CREATE TABLE content_deletions (object_key TEXT PRIMARY KEY) WITHOUT ROWID;
CREATE TABLE content_migration_lock (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    token TEXT NOT NULL,
    expires_at TEXT NOT NULL
) STRICT;

-- Match the suffix, not an arbitrary occurrence of 本文 in a novel title.
CREATE VIEW legacy_content_objects AS
SELECT * FROM objects WHERE substr(object_key, 1, 7) = 'novels/'
AND substr(object_key, -5) = '.yaml'
AND (
    (json_array_length('[' || replace(json_quote(object_key), '/', '","') || ']') >= 5
     AND json_extract('[' || replace(json_quote(object_key), '/', '","') || ']', '$[#-2]') = '本文')
    OR
    (json_array_length('[' || replace(json_quote(object_key), '/', '","') || ']') >= 7
     AND json_extract('[' || replace(json_quote(object_key), '/', '","') || ']', '$[#-4]') = '本文'
     AND json_extract('[' || replace(json_quote(object_key), '/', '","') || ']', '$[#-3]') IN ('cache', '.cache'))
);

CREATE TRIGGER content_legacy_insert_guard BEFORE INSERT ON objects
WHEN COALESCE((SELECT value_json FROM app_state WHERE scope = 'inv' AND key = 'content_backend'), '"d1"') NOT IN ('"d1"', 'd1')
AND substr(NEW.object_key, 1, 7) = 'novels/' AND substr(NEW.object_key, -5) = '.yaml'
AND (SELECT (json_array_length(p) >= 5 AND json_extract(p, '$[#-2]') = '本文')
         OR (json_array_length(p) >= 7 AND json_extract(p, '$[#-4]') = '本文' AND json_extract(p, '$[#-3]') IN ('cache', '.cache'))
     FROM (SELECT '[' || replace(json_quote(NEW.object_key), '/', '","') || ']' AS p))
BEGIN SELECT RAISE(ABORT, 'legacy body write rejected after content migration started'); END;

CREATE TRIGGER content_legacy_update_guard BEFORE UPDATE ON objects
WHEN COALESCE((SELECT value_json FROM app_state WHERE scope = 'inv' AND key = 'content_backend'), '"d1"') NOT IN ('"d1"', 'd1')
AND substr(NEW.object_key, 1, 7) = 'novels/' AND substr(NEW.object_key, -5) = '.yaml'
AND (SELECT (json_array_length(p) >= 5 AND json_extract(p, '$[#-2]') = '本文')
         OR (json_array_length(p) >= 7 AND json_extract(p, '$[#-4]') = '本文' AND json_extract(p, '$[#-3]') IN ('cache', '.cache'))
     FROM (SELECT '[' || replace(json_quote(NEW.object_key), '/', '","') || ']' AS p))
BEGIN SELECT RAISE(ABORT, 'legacy body write rejected after content migration started'); END;

CREATE TRIGGER content_legacy_delete_guard BEFORE DELETE ON objects
WHEN COALESCE((SELECT value_json FROM app_state WHERE scope = 'inv' AND key = 'content_backend'), '"d1"') NOT IN ('"d1"', 'd1')
AND EXISTS (SELECT 1 FROM legacy_content_objects WHERE object_key = OLD.object_key)
AND NOT EXISTS (SELECT 1 FROM content_deletions WHERE object_key = OLD.object_key)
BEGIN SELECT RAISE(ABORT, 'legacy body deletion requires content store transaction'); END;

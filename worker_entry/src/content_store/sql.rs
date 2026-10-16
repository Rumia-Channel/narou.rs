//! Statements kept explicit so race/rollback conditions can be exercised in SQLite.

pub const INITIALIZE_S3_DEFAULT: &str = r#"
INSERT INTO app_state(scope,key,value_json,value_yaml)
SELECT 'inv','content_backend',json_quote(next_backend),next_backend
FROM (SELECT CASE WHEN EXISTS(SELECT 1 FROM legacy_content_objects)
                  THEN 'migrating' ELSE 's3' END AS next_backend)
WHERE 1
ON CONFLICT(scope,key) DO UPDATE SET value_json=excluded.value_json,value_yaml=excluded.value_yaml
WHERE ?1=1 AND app_state.value_json IN ('"d1"','d1')
RETURNING value_json
"#;

pub const QUEUE_PREVIOUS: &str = r#"
INSERT INTO content_garbage(blob_key, not_before)
SELECT blob_key, ?8 FROM content_objects
WHERE object_key = ?1 AND blob_key != ?2
AND (?11 = '*' OR blob_key = ?11)
AND COALESCE((SELECT value_json FROM app_state WHERE scope='inv' AND key='content_backend'), '"d1"') IN ('"migrating"','migrating','"s3"','s3')
AND EXISTS(SELECT 1 FROM content_garbage WHERE blob_key=?2 AND julianday(not_before)>julianday('now'))
AND (?12 = '' OR EXISTS(SELECT 1 FROM legacy_content_objects WHERE object_key = ?1 AND updated_at = ?12))
ON CONFLICT(blob_key) DO UPDATE SET not_before = excluded.not_before
"#;

pub const PUBLISH: &str = r#"
INSERT INTO content_objects(object_key,blob_key,size,stored_size,encoding,crc32,sha256,updated_at,content_type,verified)
SELECT ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10
WHERE COALESCE((SELECT value_json FROM app_state WHERE scope='inv' AND key='content_backend'), '"d1"') IN ('"migrating"','migrating','"s3"','s3')
AND EXISTS(SELECT 1 FROM content_garbage WHERE blob_key=?2 AND julianday(not_before)>julianday('now'))
AND (?11 = '*' OR COALESCE((SELECT blob_key FROM content_objects WHERE object_key = ?1),'') = ?11)
AND (?12 = '' OR EXISTS(SELECT 1 FROM legacy_content_objects WHERE object_key = ?1 AND updated_at = ?12))
ON CONFLICT(object_key) DO UPDATE SET blob_key=excluded.blob_key,size=excluded.size,
stored_size=excluded.stored_size,encoding=excluded.encoding,crc32=excluded.crc32,
sha256=excluded.sha256,updated_at=excluded.updated_at,content_type=excluded.content_type,verified=excluded.verified
"#;

pub const CUTOVER: &str = r#"
UPDATE app_state SET value_json='"s3"',value_yaml='s3'
WHERE scope='inv' AND key='content_backend' AND value_json IN ('"migrating"','migrating')
AND NOT EXISTS(SELECT 1 FROM legacy_content_objects l LEFT JOIN content_objects c USING(object_key)
               WHERE c.object_key IS NULL OR c.verified != 1)
AND NOT EXISTS(SELECT 1 FROM content_objects WHERE verified != 1)
RETURNING key AS object_key
"#;

pub const QUEUE_DELETED: &str = r#"
INSERT INTO content_garbage(blob_key,not_before)
SELECT blob_key,?3 FROM content_objects WHERE object_key=?1 AND (?2='*' OR blob_key=?2)
ON CONFLICT(blob_key) DO UPDATE SET not_before=excluded.not_before
"#;

pub const LIST: &str = r#"
WITH combined AS (
 SELECT object_key,size,updated_at,content_type FROM objects o
 WHERE NOT EXISTS(SELECT 1 FROM content_objects c WHERE c.object_key=o.object_key)
 AND (?4 != 's3' OR NOT EXISTS(SELECT 1 FROM legacy_content_objects l WHERE l.object_key=o.object_key))
 UNION ALL
 SELECT object_key,size,updated_at,content_type FROM content_objects
)
SELECT object_key,size,updated_at,content_type FROM combined
WHERE object_key >= ?1 AND object_key < ?2 AND object_key > ?3
AND (?1='' OR object_key=?1 OR substr(object_key,1,length(?1)+1)=?1||'/')
ORDER BY object_key LIMIT ?5
"#;

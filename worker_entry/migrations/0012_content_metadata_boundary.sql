-- A subdirectory-layout novel named 本文 has metadata at
-- novels/<site>/本文/本文/toc.yaml. Root YAML names are not episode filenames
-- (episodes carry an index), even when their parent directory is named 本文.
-- Replace existing definitions so databases that applied 0011 are fixed too.
DROP TRIGGER content_legacy_insert_guard;
DROP TRIGGER content_legacy_update_guard;
DROP TRIGGER content_legacy_delete_guard;
DROP VIEW legacy_content_objects;

CREATE VIEW legacy_content_objects AS
SELECT * FROM objects WHERE substr(object_key, 1, 7) = 'novels/'
AND substr(object_key, -5) = '.yaml'
AND (SELECT json_extract(p, '$[#-1]') NOT IN ('toc.yaml', 'converter.yaml', '.illustration_cache.yaml')
     AND ((json_array_length(p) >= 5 AND json_extract(p, '$[#-2]') = '本文')
          OR (json_array_length(p) >= 7 AND json_extract(p, '$[#-4]') = '本文' AND json_extract(p, '$[#-3]') IN ('cache', '.cache')))
     FROM (SELECT '[' || replace(json_quote(object_key), '/', '","') || ']' AS p));

CREATE TRIGGER content_legacy_insert_guard BEFORE INSERT ON objects
WHEN COALESCE((SELECT value_json FROM app_state WHERE scope = 'inv' AND key = 'content_backend'), '"d1"') NOT IN ('"d1"', 'd1')
AND substr(NEW.object_key, 1, 7) = 'novels/' AND substr(NEW.object_key, -5) = '.yaml'
AND (SELECT json_extract(p, '$[#-1]') NOT IN ('toc.yaml', 'converter.yaml', '.illustration_cache.yaml')
     AND ((json_array_length(p) >= 5 AND json_extract(p, '$[#-2]') = '本文')
          OR (json_array_length(p) >= 7 AND json_extract(p, '$[#-4]') = '本文' AND json_extract(p, '$[#-3]') IN ('cache', '.cache')))
     FROM (SELECT '[' || replace(json_quote(NEW.object_key), '/', '","') || ']' AS p))
BEGIN SELECT RAISE(ABORT, 'legacy body write rejected after content migration started'); END;

CREATE TRIGGER content_legacy_update_guard BEFORE UPDATE ON objects
WHEN COALESCE((SELECT value_json FROM app_state WHERE scope = 'inv' AND key = 'content_backend'), '"d1"') NOT IN ('"d1"', 'd1')
AND substr(NEW.object_key, 1, 7) = 'novels/' AND substr(NEW.object_key, -5) = '.yaml'
AND (SELECT json_extract(p, '$[#-1]') NOT IN ('toc.yaml', 'converter.yaml', '.illustration_cache.yaml')
     AND ((json_array_length(p) >= 5 AND json_extract(p, '$[#-2]') = '本文')
          OR (json_array_length(p) >= 7 AND json_extract(p, '$[#-4]') = '本文' AND json_extract(p, '$[#-3]') IN ('cache', '.cache')))
     FROM (SELECT '[' || replace(json_quote(NEW.object_key), '/', '","') || ']' AS p))
BEGIN SELECT RAISE(ABORT, 'legacy body write rejected after content migration started'); END;

CREATE TRIGGER content_legacy_delete_guard BEFORE DELETE ON objects
WHEN COALESCE((SELECT value_json FROM app_state WHERE scope = 'inv' AND key = 'content_backend'), '"d1"') NOT IN ('"d1"', 'd1')
AND EXISTS (SELECT 1 FROM legacy_content_objects WHERE object_key = OLD.object_key)
AND NOT EXISTS (SELECT 1 FROM content_deletions WHERE object_key = OLD.object_key)
BEGIN SELECT RAISE(ABORT, 'legacy body deletion requires content store transaction'); END;

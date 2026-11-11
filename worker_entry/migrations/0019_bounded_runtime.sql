-- Hash hints are addressed by novel, never loaded as a whole-library payload.
CREATE TABLE novel_section_hashes (
    novel_id TEXT PRIMARY KEY,
    digests_json TEXT NOT NULL CHECK(json_valid(digests_json) AND json_type(digests_json) = 'object')
) STRICT;

-- json_each is evaluated only on valid object JSON. Preserve the legacy row
-- when it is YAML-only or malformed: these optional hints may be rebuilt, but
-- a migration must not discard unconvertible persisted values.
INSERT INTO novel_section_hashes(novel_id, digests_json)
SELECT novel.key, novel.value
FROM app_state AS state,
     json_each(CASE WHEN json_valid(state.value_json) THEN
                    CASE WHEN json_type(state.value_json) = 'object' THEN state.value_json ELSE '{}' END
                    ELSE '{}' END) AS novel
WHERE state.scope = 'inv' AND state.key = 'section_hash_cache'
  AND novel.type = 'object';

DELETE FROM app_state
WHERE scope = 'inv' AND key = 'section_hash_cache'
  AND CASE WHEN json_valid(value_json) THEN
      json_type(value_json) = 'object'
      AND NOT EXISTS (SELECT 1 FROM json_each(value_json) WHERE type != 'object')
      ELSE 0 END;

-- Independent of provider/business retries; only lost deliveries/executions
-- spend this counter. Durable checkpoint progress resets it.
ALTER TABLE worker_jobs ADD COLUMN recovery_attempts INTEGER NOT NULL DEFAULT 0;

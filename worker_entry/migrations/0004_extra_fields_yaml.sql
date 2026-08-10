-- Phase 8: persist novel extra fields and settings values as YAML text.
--
-- JSON is a subset of YAML, so the existing JSON payloads backfill directly
-- and stay readable through the YAML readers. The readers keep the legacy
-- JSON column as a fallback (and for rows written before this migration).
--
-- The legacy columns are intentionally kept: the native files are YAML and
-- the Worker keeps byte-compatible JSON text around only for readers that
-- have not been migrated yet.

ALTER TABLE novels ADD COLUMN extra_fields_yaml TEXT NOT NULL DEFAULT '{}';
ALTER TABLE app_state ADD COLUMN value_yaml TEXT NOT NULL DEFAULT '{}';

UPDATE novels SET extra_fields_yaml = extra_fields_json;
UPDATE app_state SET value_yaml = value_json;

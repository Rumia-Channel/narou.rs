-- Drop the legacy body_yaml columns after the 0009→0010 Rust data
-- migration has populated body_hash for every row. Runs only after
-- `migrate_section_bodies` completed inside `migrations::apply`.

ALTER TABLE novel_sections DROP COLUMN body_yaml;
ALTER TABLE novel_version_sections DROP COLUMN body_yaml;

-- Native version history (P4b): immutable snapshots of section sets.
CREATE TABLE IF NOT EXISTS novel_versions (
    id            INTEGER PRIMARY KEY,
    novel_id      INTEGER NOT NULL REFERENCES novels(id) ON DELETE CASCADE,
    parent_id     INTEGER REFERENCES novel_versions(id),
    origin        TEXT NOT NULL CHECK (origin IN ('update','manual','rollback','import')),
    note          TEXT,
    created_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS novel_versions_novel_idx ON novel_versions(novel_id, id DESC);

CREATE TABLE IF NOT EXISTS novel_version_sections (
    version_id INTEGER NOT NULL REFERENCES novel_versions(id) ON DELETE CASCADE,
    idx        TEXT NOT NULL,
    subtitle   TEXT,
    body_yaml  TEXT NOT NULL,
    PRIMARY KEY (version_id, idx)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS novel_version_diffs (
    version_id      INTEGER PRIMARY KEY REFERENCES novel_versions(id) ON DELETE CASCADE,
    prev_version_id INTEGER,
    unified_diff    TEXT NOT NULL
) WITHOUT ROWID;

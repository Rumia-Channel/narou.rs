-- Native content mirrors (P4a): converted outputs and per-novel sections.
CREATE TABLE IF NOT EXISTS novel_outputs (
    novel_id   INTEGER NOT NULL REFERENCES novels(id) ON DELETE CASCADE,
    kind       TEXT NOT NULL,
    payload    BLOB NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (novel_id, kind)
) STRICT;

CREATE TABLE IF NOT EXISTS novel_sections (
    novel_id INTEGER NOT NULL REFERENCES novels(id) ON DELETE CASCADE,
    idx      TEXT NOT NULL,
    subtitle TEXT,
    body_yaml TEXT NOT NULL,
    PRIMARY KEY (novel_id, idx)
) WITHOUT ROWID;

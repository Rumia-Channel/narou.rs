-- Native SQLite schema (ported from Worker native) for Worker application services.
-- Timestamps are canonical UTC RFC3339 strings with nanosecond precision;
-- storing them as TEXT avoids JavaScript Number precision loss.
CREATE TABLE IF NOT EXISTS novels (
    id INTEGER PRIMARY KEY,
    author TEXT NOT NULL,
    author_fold TEXT NOT NULL,
    title TEXT NOT NULL,
    title_fold TEXT NOT NULL,
    file_title TEXT NOT NULL,
    toc_url TEXT NOT NULL UNIQUE,
    toc_url_fold TEXT NOT NULL,
    sitename TEXT NOT NULL,
    sitename_fold TEXT NOT NULL,
    novel_type INTEGER NOT NULL DEFAULT 0,
    end INTEGER NOT NULL DEFAULT 0,
    last_update TEXT NOT NULL,
    new_arrivals_date TEXT,
    use_subdirectory INTEGER NOT NULL DEFAULT 0,
    general_firstup TEXT,
    novelupdated_at TEXT,
    general_lastup TEXT,
    last_mail_date TEXT,
    tags_json TEXT NOT NULL DEFAULT '[]',
    tags_fold TEXT NOT NULL DEFAULT '',
    tags_sort TEXT NOT NULL DEFAULT '',
    ncode TEXT,
    ncode_fold TEXT,
    domain TEXT,
    domain_fold TEXT,
    general_all_no INTEGER,
    length INTEGER,
    suspend INTEGER NOT NULL DEFAULT 0,
    is_narou INTEGER NOT NULL DEFAULT 0,
    last_check_date TEXT,
    convert_failure INTEGER NOT NULL DEFAULT 0,
    extra_fields_json TEXT NOT NULL DEFAULT '{}',
    extra_fields_bytes INTEGER NOT NULL DEFAULT 2
) STRICT;

CREATE INDEX IF NOT EXISTS novels_toc_url_fold_idx ON novels(toc_url_fold);
CREATE INDEX IF NOT EXISTS novels_ncode_fold_idx ON novels(ncode_fold);
CREATE INDEX IF NOT EXISTS novels_title_fold_idx ON novels(title_fold);
CREATE INDEX IF NOT EXISTS novels_author_fold_idx ON novels(author_fold);
CREATE INDEX IF NOT EXISTS novels_domain_fold_idx ON novels(domain_fold);
CREATE INDEX IF NOT EXISTS novels_last_update_idx ON novels(last_update, id);
CREATE INDEX IF NOT EXISTS novels_general_lastup_idx ON novels(general_lastup, id);
CREATE INDEX IF NOT EXISTS novels_last_check_date_idx ON novels(last_check_date, id);
CREATE INDEX IF NOT EXISTS novels_new_arrivals_date_idx ON novels(new_arrivals_date, id);
CREATE INDEX IF NOT EXISTS novels_status_sort_idx ON novels(suspend, end, id);
CREATE INDEX IF NOT EXISTS novels_tags_sort_idx ON novels(tags_sort, id);

CREATE TABLE IF NOT EXISTS novel_tags (
    novel_id INTEGER NOT NULL REFERENCES novels(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    tag TEXT NOT NULL,
    tag_fold TEXT NOT NULL,
    PRIMARY KEY (novel_id, position),
    UNIQUE (novel_id, tag)
) STRICT;
CREATE INDEX IF NOT EXISTS novel_tags_tag_idx ON novel_tags(tag_fold, novel_id);

CREATE TABLE IF NOT EXISTS frozen_novels (
    novel_id INTEGER PRIMARY KEY REFERENCES novels(id) ON DELETE CASCADE
) STRICT;

CREATE TABLE IF NOT EXISTS app_state (
    scope TEXT NOT NULL,
    key TEXT NOT NULL,
    value_json TEXT NOT NULL,
    PRIMARY KEY (scope, key)
) STRICT;

CREATE TABLE IF NOT EXISTS novel_id_sequence (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    next_id INTEGER NOT NULL
) STRICT;
INSERT OR IGNORE INTO novel_id_sequence (id, next_id)
SELECT 1, COALESCE(MAX(id), 0) + 1 FROM novels;

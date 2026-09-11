-- Drop the UNIQUE constraint on novels.toc_url.
--
-- narou.rb permits duplicate toc_url rows (re-download after removal, manual
-- database.yaml edits, same novel reachable through two site URLs). The
-- legacy YAML importer hit `UNIQUE constraint failed: novels.toc_url` on a
-- real library containing such duplicates, so the constraint is removed and
-- replaced by a plain index. `find_by_toc_url` only needs first-match
-- semantics, which a non-unique index serves identically.
--
-- SQLite cannot drop a column constraint in place; the table is rebuilt.
-- PRAGMA foreign_keys is off inside this migration transaction so the
-- dependent tables (novel_tags, frozen_novels, novel_outputs,
-- novel_sections, novel_versions) keep their rows untouched.

PRAGMA foreign_keys = OFF;

CREATE TABLE novels_new (
    id INTEGER PRIMARY KEY,
    author TEXT NOT NULL,
    author_fold TEXT NOT NULL,
    title TEXT NOT NULL,
    title_fold TEXT NOT NULL,
    file_title TEXT NOT NULL,
    toc_url TEXT NOT NULL,
    toc_url_fold TEXT NOT NULL,
    sitename TEXT NOT NULL,
    sitename_fold TEXT NOT NULL,
    novel_type INTEGER NOT NULL DEFAULT 0,
    "end" INTEGER NOT NULL DEFAULT 0,
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
    status_sort TEXT NOT NULL DEFAULT '',
    convert_failure INTEGER NOT NULL DEFAULT 0,
    extra_fields_json TEXT NOT NULL DEFAULT '{}',
    extra_fields_yaml TEXT NOT NULL DEFAULT '{}',
    extra_fields_bytes INTEGER NOT NULL DEFAULT 2
) STRICT;

INSERT INTO novels_new SELECT * FROM novels;
DROP TABLE novels;
ALTER TABLE novels_new RENAME TO novels;

CREATE INDEX IF NOT EXISTS novels_toc_url_idx ON novels(toc_url);
CREATE INDEX IF NOT EXISTS novels_toc_url_fold_idx ON novels(toc_url_fold);
CREATE INDEX IF NOT EXISTS novels_ncode_fold_idx ON novels(ncode_fold);
CREATE INDEX IF NOT EXISTS novels_title_fold_idx ON novels(title_fold);
CREATE INDEX IF NOT EXISTS novels_author_fold_idx ON novels(author_fold);
CREATE INDEX IF NOT EXISTS novels_domain_fold_idx ON novels(domain_fold);
CREATE INDEX IF NOT EXISTS novels_last_update_idx ON novels(last_update, id);
CREATE INDEX IF NOT EXISTS novels_general_lastup_idx ON novels(general_lastup, id);
CREATE INDEX IF NOT EXISTS novels_last_check_date_idx ON novels(last_check_date, id);
CREATE INDEX IF NOT EXISTS novels_new_arrivals_date_idx ON novels(new_arrivals_date, id);
CREATE INDEX IF NOT EXISTS novels_status_sort_idx ON novels(suspend, "end", id);
CREATE INDEX IF NOT EXISTS novels_tags_sort_idx ON novels(tags_sort, id);

PRAGMA foreign_keys = ON;

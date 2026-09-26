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
--
-- Foreign-key handling differs per backend:
-- - D1 always enforces foreign keys, and `PRAGMA foreign_keys` is a no-op
--   inside a transaction anyway (`PRAGMA defer_foreign_keys` only defers
--   constraint *checks* — ON DELETE CASCADE still fires immediately). So
--   the children that exist on both backends (novel_tags, frozen_novels)
--   are copied to staging tables and emptied before the DROP, then restored
--   afterwards.
-- - The native runner disables `PRAGMA foreign_keys` *outside* the
--   transaction around this file, which additionally protects the
--   native-only children (novel_outputs, novel_sections, novel_versions,
--   novel_version_sections, novel_version_diffs) that do not exist in the
--   D1 schema at this version and so cannot be named here.

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

-- Column lists are explicit: ALTER TABLE ADD COLUMN appends, so the old
-- table's physical tail order (…, convert_failure, extra_fields_json,
-- extra_fields_bytes, status_sort, extra_fields_yaml) differs from
-- novels_new's declared order. A positional `SELECT *` would land those
-- five columns into the wrong targets (and fail STRICT type checks).
INSERT INTO novels_new (
    id,
    author,
    author_fold,
    title,
    title_fold,
    file_title,
    toc_url,
    toc_url_fold,
    sitename,
    sitename_fold,
    novel_type,
    "end",
    last_update,
    new_arrivals_date,
    use_subdirectory,
    general_firstup,
    novelupdated_at,
    general_lastup,
    last_mail_date,
    tags_json,
    tags_fold,
    tags_sort,
    ncode,
    ncode_fold,
    domain,
    domain_fold,
    general_all_no,
    length,
    suspend,
    is_narou,
    last_check_date,
    status_sort,
    convert_failure,
    extra_fields_json,
    extra_fields_yaml,
    extra_fields_bytes
)
SELECT
    id,
    author,
    author_fold,
    title,
    title_fold,
    file_title,
    toc_url,
    toc_url_fold,
    sitename,
    sitename_fold,
    novel_type,
    "end",
    last_update,
    new_arrivals_date,
    use_subdirectory,
    general_firstup,
    novelupdated_at,
    general_lastup,
    last_mail_date,
    tags_json,
    tags_fold,
    tags_sort,
    ncode,
    ncode_fold,
    domain,
    domain_fold,
    general_all_no,
    length,
    suspend,
    is_narou,
    last_check_date,
    status_sort,
    convert_failure,
    extra_fields_json,
    extra_fields_yaml,
    extra_fields_bytes
FROM novels;

-- Evacuate the children present on both backends so D1's always-on
-- ON DELETE CASCADE does not erase them when novels is dropped.
CREATE TABLE novel_tags_bak AS SELECT * FROM novel_tags;
CREATE TABLE frozen_novels_bak AS SELECT * FROM frozen_novels;
DELETE FROM novel_tags;
DELETE FROM frozen_novels;

DROP TABLE novels;
ALTER TABLE novels_new RENAME TO novels;

INSERT INTO novel_tags SELECT * FROM novel_tags_bak;
INSERT INTO frozen_novels SELECT * FROM frozen_novels_bak;
DROP TABLE novel_tags_bak;
DROP TABLE frozen_novels_bak;

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
-- Same key columns 0003 established; recreating the dropped index must not
-- revert to the pre-0003 (suspend, "end", id) definition.
CREATE INDEX IF NOT EXISTS novels_status_sort_idx ON novels(status_sort, id);
CREATE INDEX IF NOT EXISTS novels_tags_sort_idx ON novels(tags_sort, id);

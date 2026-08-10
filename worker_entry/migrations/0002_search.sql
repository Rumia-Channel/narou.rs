-- Search remains substring-compatible with native behavior while avoiding
-- per-row lower()/LIKE expressions in normal queries.
CREATE INDEX IF NOT EXISTS novels_title_author_fold_idx
    ON novels(title_fold, author_fold, id);
CREATE INDEX IF NOT EXISTS novels_sitenames_fold_idx
    ON novels(sitename_fold, id);

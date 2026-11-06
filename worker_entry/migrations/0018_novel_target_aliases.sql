-- Admission used to rebuild all URL suffixes on every wakeup. The alias
-- index preserves the same exact URL / ncode / slash-delimited suffix set,
-- including collisions (the lowest novel_id wins), in the novel transaction.
CREATE TABLE novel_target_aliases (
    target TEXT NOT NULL,
    novel_id INTEGER NOT NULL,
    PRIMARY KEY (target, novel_id)
) WITHOUT ROWID, STRICT;
CREATE INDEX novel_target_aliases_novel_idx ON novel_target_aliases(novel_id);

INSERT OR IGNORE INTO novel_target_aliases(target, novel_id)
WITH RECURSIVE suffixes(novel_id, target) AS (
    SELECT id, substr(rtrim(toc_url_fold, '/'), instr(rtrim(toc_url_fold, '/'), '/') + 1)
    FROM novels WHERE instr(rtrim(toc_url_fold, '/'), '/') > 0
    UNION ALL
    SELECT novel_id, substr(target, instr(target, '/') + 1)
    FROM suffixes WHERE instr(target, '/') > 0
)
SELECT toc_url_fold, id FROM novels
UNION ALL SELECT ncode_fold, id FROM novels WHERE ncode_fold IS NOT NULL
UNION ALL SELECT target, novel_id FROM suffixes;

CREATE TRIGGER novel_target_aliases_insert AFTER INSERT ON novels
BEGIN
    INSERT OR IGNORE INTO novel_target_aliases(target, novel_id)
    WITH RECURSIVE suffixes(target) AS (
        SELECT substr(rtrim(NEW.toc_url_fold, '/'), instr(rtrim(NEW.toc_url_fold, '/'), '/') + 1)
        WHERE instr(rtrim(NEW.toc_url_fold, '/'), '/') > 0
        UNION ALL
        SELECT substr(target, instr(target, '/') + 1) FROM suffixes WHERE instr(target, '/') > 0
    )
    SELECT NEW.toc_url_fold, NEW.id
    UNION ALL SELECT NEW.ncode_fold, NEW.id WHERE NEW.ncode_fold IS NOT NULL
    UNION ALL SELECT target, NEW.id FROM suffixes;
END;

CREATE TRIGGER novel_target_aliases_update AFTER UPDATE OF id, toc_url_fold, ncode_fold ON novels
WHEN OLD.id IS NOT NEW.id OR OLD.toc_url_fold IS NOT NEW.toc_url_fold OR OLD.ncode_fold IS NOT NEW.ncode_fold
BEGIN
    DELETE FROM novel_target_aliases WHERE novel_id = OLD.id;
    INSERT OR IGNORE INTO novel_target_aliases(target, novel_id)
    WITH RECURSIVE suffixes(target) AS (
        SELECT substr(rtrim(NEW.toc_url_fold, '/'), instr(rtrim(NEW.toc_url_fold, '/'), '/') + 1)
        WHERE instr(rtrim(NEW.toc_url_fold, '/'), '/') > 0
        UNION ALL
        SELECT substr(target, instr(target, '/') + 1) FROM suffixes WHERE instr(target, '/') > 0
    )
    SELECT NEW.toc_url_fold, NEW.id
    UNION ALL SELECT NEW.ncode_fold, NEW.id WHERE NEW.ncode_fold IS NOT NULL
    UNION ALL SELECT target, NEW.id FROM suffixes;
END;

CREATE TRIGGER novel_target_aliases_delete AFTER DELETE ON novels
BEGIN
    DELETE FROM novel_target_aliases WHERE novel_id = OLD.id;
END;

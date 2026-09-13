-- Materialize the status label used by list sorting.
-- The canonical row remains novels; tag/freeze mutations refresh this derived key.
ALTER TABLE novels ADD COLUMN status_sort TEXT NOT NULL DEFAULT '';

UPDATE novels
SET status_sort =
    (CASE WHEN end <> 0 OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = novels.id AND t.tag = 'end') THEN '完結' ELSE '' END ||
     CASE WHEN (end <> 0 OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = novels.id AND t.tag = 'end'))
                AND EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = novels.id AND t.tag = '404')
          THEN ', ' ELSE '' END ||
     CASE WHEN EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = novels.id AND t.tag = '404') THEN '削除' ELSE '' END ||
     CASE WHEN (end <> 0 OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = novels.id AND t.tag = 'end')
                    OR EXISTS (SELECT 1 FROM novel_tags t WHERE t.novel_id = novels.id AND t.tag = '404'))
                AND suspend <> 0
          THEN ', ' ELSE '' END ||
     CASE WHEN suspend <> 0 THEN '中断' ELSE '' END);

DROP INDEX IF EXISTS novels_status_sort_idx;
CREATE INDEX IF NOT EXISTS novels_status_sort_idx ON novels(status_sort, id);

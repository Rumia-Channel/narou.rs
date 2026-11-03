-- Exact tag filtering and grouping must not probe every novel's tag rows.
-- The existing tag_fold index serves a different, case-folded contract.
CREATE INDEX IF NOT EXISTS novel_tags_exact_tag_idx ON novel_tags(tag, novel_id);
CREATE INDEX IF NOT EXISTS novels_attribute_tag_idx ON novels(attribute_tag, id);

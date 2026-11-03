-- Exact tag filtering and grouping must not probe every novel's tag rows.
-- The existing tag_fold index serves a different, case-folded contract.
CREATE INDEX IF NOT EXISTS novel_tags_exact_tag_idx ON novel_tags(tag, novel_id);
CREATE INDEX IF NOT EXISTS novels_attribute_tag_idx ON novels(attribute_tag, id);

-- Ownership probes must skip terminal history with no physical execution.
-- A status-only index still visits every permanent row to inspect its token.
CREATE INDEX IF NOT EXISTS worker_jobs_execution_owner_idx ON worker_jobs(status, execution_token, lease_until);

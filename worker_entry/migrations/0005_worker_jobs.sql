-- Worker job ledger (Phase 8).
--
-- One row per discrete queued job. The queue message carries only the
-- envelope (version, job_id, job) and is acked only after a durable terminal
-- state has been recorded here, so a crash between execution and ack never
-- loses work (the redelivered message re-claims the same row idempotently).
--
-- Timestamps are canonical UTC RFC3339 strings (same convention as novels).

CREATE TABLE IF NOT EXISTS worker_jobs (
    job_id TEXT PRIMARY KEY,
    -- Canonical kind string (JobKind::as_str), e.g. 'download' / 'update'.
    kind TEXT NOT NULL,
    -- Canonical target string (JobTarget::as_str): numeric id, ncode, or '*'.
    target TEXT NOT NULL,
    -- Effective options as a JSON array of strings (serde_json).
    options TEXT NOT NULL DEFAULT '[]',
    -- Ledger status: pending / running / succeeded / partial / retryable /
    -- blocked / permanent. Only active states participate in dedupe.
    status TEXT NOT NULL DEFAULT 'pending',
    -- kind:target:effective-options canonical key for active dedupe.
    dedupe_key TEXT NOT NULL,
    -- Retryable attempt counter (bounded retries; never blanket retry).
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
) STRICT;

-- At most one *active* job per kind+target+effective options. Enqueueing the
-- same plan while an active row exists returns the existing job id; reaching
-- a terminal state drops the row out of this index, which is what "clears"
-- the dedupe key so a later identical request enqueues fresh.
CREATE UNIQUE INDEX IF NOT EXISTS worker_jobs_active_dedupe
    ON worker_jobs(dedupe_key)
    WHERE status IN ('pending', 'running', 'retryable');

-- Ledger lookup by id and dedupe lookup by key.
CREATE INDEX IF NOT EXISTS worker_jobs_status_idx ON worker_jobs(status, created_at);

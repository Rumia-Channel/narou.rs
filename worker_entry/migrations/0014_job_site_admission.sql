-- Admission waiting is durable and separate from provider/business retry counts.
-- Legacy owners without resolved identity conservatively block new claims until
-- their old invocation settles or its lease is recovered.
ALTER TABLE worker_jobs ADD COLUMN site_key TEXT;
ALTER TABLE worker_jobs ADD COLUMN normalized_target TEXT;
ALTER TABLE worker_jobs ADD COLUMN admission_waiting INTEGER NOT NULL DEFAULT 0
    CHECK (admission_waiting IN (0, 1));
CREATE INDEX worker_jobs_site_owner_idx ON worker_jobs(site_key, kind, status)
    WHERE status = 'running' OR (status = 'permanent' AND execution_token IS NOT NULL);
CREATE INDEX worker_jobs_admission_wait_idx ON worker_jobs(site_key, created_at, job_id)
    WHERE admission_waiting = 1;

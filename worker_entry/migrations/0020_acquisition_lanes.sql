-- Separate delivery reservations from physical execution leases. A queue message
-- may execute only while its lane/job/token reservation still matches.
ALTER TABLE worker_jobs ADD COLUMN wait_reason TEXT
    CHECK (wait_reason IS NULL OR wait_reason IN ('site', 'lane', 'dispatch', 'capacity'));

CREATE TABLE worker_acquisition_lanes (
    lane INTEGER PRIMARY KEY CHECK (lane BETWEEN 0 AND 3),
    job_id TEXT UNIQUE,
    site_key TEXT,
    dispatch_token TEXT UNIQUE,
    lease_until TEXT,
    CHECK ((job_id IS NULL AND site_key IS NULL AND dispatch_token IS NULL AND lease_until IS NULL)
        OR (job_id IS NOT NULL AND site_key IS NOT NULL AND dispatch_token IS NOT NULL AND lease_until IS NOT NULL))
) STRICT;
INSERT INTO worker_acquisition_lanes(lane) VALUES (0), (1), (2), (3);
CREATE UNIQUE INDEX worker_acquisition_site_idx ON worker_acquisition_lanes(site_key)
    WHERE site_key IS NOT NULL;
CREATE INDEX worker_jobs_acquisition_ready_idx ON worker_jobs(created_at, job_id)
    WHERE kind IN ('download', 'update', 'author_check') AND status IN ('pending', 'retryable');

-- Existing v1/v2 queue deliveries become ingress notifications. Keep their
-- plans, checkpoints, retry holds and identities; the dispatcher fills only
-- missing identities before assigning a dedicated execution lane.
UPDATE worker_jobs SET admission_waiting = 1, wait_reason = 'lane'
WHERE kind IN ('download', 'update', 'author_check') AND status IN ('pending', 'retryable');

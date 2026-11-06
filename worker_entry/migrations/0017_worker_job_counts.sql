-- UI polling must not rescan the entire job history. Keep exact counts in
-- the same transaction as every ledger mutation, including old Worker code.
-- reason: 0 = ordinary, 1 = user-cancelled, 2 = removed/cleared from queue.
-- Only the three persisted operator reasons affect counts; arbitrary error
-- messages must not create an unbounded number of aggregate rows.
CREATE TABLE worker_job_counts (
    status TEXT NOT NULL,
    lane INTEGER NOT NULL CHECK (lane IN (0, 1)),
    reason INTEGER NOT NULL CHECK (reason IN (0, 1, 2)),
    n INTEGER NOT NULL CHECK (n >= 0),
    PRIMARY KEY (status, lane, reason)
) WITHOUT ROWID, STRICT;

CREATE VIEW worker_job_count_keys AS
SELECT job_id, status,
       CASE WHEN kind IN ('convert', 'send', 'backup', 'mail') THEN 1 ELSE 0 END AS lane,
       CASE WHEN status = 'permanent' THEN CASE last_error
           WHEN 'cancelled by user via Web UI' THEN 1
           WHEN 'queue cleared by user via Web UI' THEN 2
           WHEN 'removed from queue by user via Web UI' THEN 2
           ELSE 0 END ELSE 0 END AS reason
FROM worker_jobs
WHERE status IN ('pending', 'retryable', 'running', 'succeeded', 'partial', 'blocked', 'permanent');

-- One-time backfill, before the triggers are installed.
INSERT INTO worker_job_counts(status, lane, reason, n)
SELECT status, lane, reason, COUNT(*) FROM worker_job_count_keys GROUP BY status, lane, reason;

CREATE TRIGGER worker_job_counts_insert AFTER INSERT ON worker_jobs
BEGIN
    INSERT INTO worker_job_counts(status, lane, reason, n)
    SELECT status, lane, reason, 1 FROM worker_job_count_keys WHERE job_id = NEW.job_id
    ON CONFLICT(status, lane, reason) DO UPDATE SET n = n + 1;
END;

CREATE TRIGGER worker_job_counts_delete AFTER DELETE ON worker_jobs
BEGIN
    UPDATE worker_job_counts SET n = n - 1
    WHERE status = OLD.status
      AND lane = CASE WHEN OLD.kind IN ('convert', 'send', 'backup', 'mail') THEN 1 ELSE 0 END
      AND reason = CASE WHEN OLD.status = 'permanent' THEN CASE OLD.last_error
          WHEN 'cancelled by user via Web UI' THEN 1
          WHEN 'queue cleared by user via Web UI' THEN 2
          WHEN 'removed from queue by user via Web UI' THEN 2
          ELSE 0 END ELSE 0 END;
END;

-- Check the aggregate key, not payload/lease/attempt changes. Heartbeats and
-- checkpoints must not add any counter reads or writes.
CREATE TRIGGER worker_job_counts_update AFTER UPDATE OF status, kind, last_error ON worker_jobs
WHEN (OLD.status,
      CASE WHEN OLD.kind IN ('convert', 'send', 'backup', 'mail') THEN 1 ELSE 0 END,
      CASE WHEN OLD.status = 'permanent' THEN CASE OLD.last_error
          WHEN 'cancelled by user via Web UI' THEN 1
          WHEN 'queue cleared by user via Web UI' THEN 2
          WHEN 'removed from queue by user via Web UI' THEN 2
          ELSE 0 END ELSE 0 END)
 IS NOT (NEW.status,
      CASE WHEN NEW.kind IN ('convert', 'send', 'backup', 'mail') THEN 1 ELSE 0 END,
      CASE WHEN NEW.status = 'permanent' THEN CASE NEW.last_error
          WHEN 'cancelled by user via Web UI' THEN 1
          WHEN 'queue cleared by user via Web UI' THEN 2
          WHEN 'removed from queue by user via Web UI' THEN 2
          ELSE 0 END ELSE 0 END)
BEGIN
    UPDATE worker_job_counts SET n = n - 1
    WHERE status = OLD.status
      AND lane = CASE WHEN OLD.kind IN ('convert', 'send', 'backup', 'mail') THEN 1 ELSE 0 END
      AND reason = CASE WHEN OLD.status = 'permanent' THEN CASE OLD.last_error
          WHEN 'cancelled by user via Web UI' THEN 1
          WHEN 'queue cleared by user via Web UI' THEN 2
          WHEN 'removed from queue by user via Web UI' THEN 2
          ELSE 0 END ELSE 0 END;
    INSERT INTO worker_job_counts(status, lane, reason, n)
    SELECT status, lane, reason, 1 FROM worker_job_count_keys WHERE job_id = NEW.job_id
    ON CONFLICT(status, lane, reason) DO UPDATE SET n = n + 1;
END;

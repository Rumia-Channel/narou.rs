-- The prefix-first index still serves per-novel admission and renewal.
CREATE INDEX novel_reader_leases_global_expiry ON novel_reader_leases(expires_at);

-- Terminal jobs normally clear the checkpoint. Only the retained checkpoints
-- are candidates; the recovery query still checks reservation and lease state.
CREATE INDEX worker_jobs_terminal_checkpoint ON worker_jobs(job_id)
WHERE status IN ('succeeded','partial','blocked','permanent') AND checkpoint_json IS NOT NULL;

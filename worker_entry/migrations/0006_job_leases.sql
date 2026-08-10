-- Phase 8 execution ownership and resumable checkpoint fields.
-- Additive migration for deployments that already applied 0005_worker_jobs.
ALTER TABLE worker_jobs ADD COLUMN lease_until TEXT;
ALTER TABLE worker_jobs ADD COLUMN execution_token TEXT;
ALTER TABLE worker_jobs ADD COLUMN checkpoint_json TEXT;

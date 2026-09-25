-- Novels whose fetch needed the stored login cookie (see the native
-- `0011_requires_login.sql`; D1 keeps the same schema shape).
ALTER TABLE novels ADD COLUMN requires_login INTEGER NOT NULL DEFAULT 0;

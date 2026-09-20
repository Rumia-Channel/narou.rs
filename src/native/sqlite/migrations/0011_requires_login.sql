-- Novels whose fetch needed the stored login cookie.
--
-- Recorded so later runs authenticate from the start for those novels alone:
-- everything else stays anonymous until a fetch actually fails and the cookie
-- rescues it.
ALTER TABLE novels ADD COLUMN requires_login INTEGER NOT NULL DEFAULT 0;

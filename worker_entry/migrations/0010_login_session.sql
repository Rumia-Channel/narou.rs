-- Which stored login credential a novel needs (see the native
-- `0012_login_session.sql`; D1 keeps the same schema shape).
ALTER TABLE novels ADD COLUMN login_session TEXT;

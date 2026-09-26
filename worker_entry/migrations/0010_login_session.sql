-- Which stored login credential a novel needs.
--
-- `requires_login` says a fetch needed the stored cookie; this records the id
-- of the credential that made it work, so a site holding several logins does
-- not have to walk the whole list again on every run. NULL means "not known
-- yet": the list is walked and the credential that succeeds is recorded here.
ALTER TABLE novels ADD COLUMN login_session TEXT;

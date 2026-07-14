-- Temp sessions: deleted automatically (full delete-session cleanup)
-- when the last user_tabs row pointing at them is closed. Additive with
-- a DEFAULT, so existing rows need no backfill — every pre-existing
-- session is a regular one.
ALTER TABLE sessions ADD COLUMN is_temp BOOLEAN NOT NULL DEFAULT 0;

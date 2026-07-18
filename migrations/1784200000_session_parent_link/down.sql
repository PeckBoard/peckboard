DROP INDEX IF EXISTS idx_sessions_parent_session_id;
ALTER TABLE sessions DROP COLUMN parent_session_id;
ALTER TABLE sessions DROP COLUMN subagent_completed_at;

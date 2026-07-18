-- Subagent sessions: spawned by another session via the `spawn_subagent`
-- MCP tool (expert_kind = 'subagent'). `parent_session_id` links child to
-- parent; `subagent_completed_at` is stamped by the completion listener
-- once the child's result has been reported back to the parent (NULL =
-- still running, counts toward the parent's concurrent-subagent cap).
ALTER TABLE sessions ADD COLUMN parent_session_id TEXT;
ALTER TABLE sessions ADD COLUMN subagent_completed_at TEXT;
CREATE INDEX IF NOT EXISTS idx_sessions_parent_session_id
    ON sessions(parent_session_id) WHERE parent_session_id IS NOT NULL;

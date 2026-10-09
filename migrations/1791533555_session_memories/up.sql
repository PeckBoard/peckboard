-- Per-session durable memory pool. An agent records facts, decisions and
-- user preferences here through the `memory_*` MCP tools; the pool is
-- rendered into the session's system prompt on every spawn, so it survives
-- `clear_session` (which only truncates events/todos) and context
-- compaction. Rows go away only with the session itself (FK cascade).
CREATE TABLE IF NOT EXISTS session_memories (
    id          TEXT PRIMARY KEY NOT NULL,
    session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    content     TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_session_memories_session
    ON session_memories (session_id, created_at);

-- Messages from other sessions held for a chat session while the user is
-- actively talking to it (focus mode, `service::chat_focus`). One row per
-- held message; a row is deleted the moment it is delivered, so the table
-- only ever holds the live queue and it survives a restart. Brand-new table
-- only — no existing table is touched.
CREATE TABLE IF NOT EXISTS chat_focus_queue (
    id                 TEXT PRIMARY KEY NOT NULL,
    session_id         TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    source_session_id  TEXT REFERENCES sessions(id) ON DELETE SET NULL,
    source_name        TEXT NOT NULL,
    text               TEXT NOT NULL,
    created_at         TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_chat_focus_queue_session
    ON chat_focus_queue (session_id, created_at);

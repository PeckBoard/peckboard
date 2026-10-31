-- The voice Assistant's temporary memory: short facts it records at topic
-- boundaries (what was done, learned, or is still pending), kept across the
-- automatic context clear and rendered into its prompt as "Recent Memory".
-- Every row expires (`expires_at`, retention set in the Assistant settings)
-- and is swept; rows go away with the voice session (FK cascade). Text is
-- redacted server-side before it is written (`service::assistant_memory`).
-- Brand-new table only — no existing table is touched.
CREATE TABLE IF NOT EXISTS assistant_memories (
    id                TEXT PRIMARY KEY NOT NULL,
    voice_session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    topic_id          TEXT,
    topic_title       TEXT NOT NULL,
    kind              TEXT NOT NULL CHECK (kind IN ('did', 'learned', 'pending')),
    summary           TEXT NOT NULL,
    ref_kind          TEXT CHECK (ref_kind IS NULL OR ref_kind IN ('session', 'project')),
    ref_id            TEXT,
    source            TEXT NOT NULL CHECK (source IN ('assistant', 'fallback')),
    created_at        TEXT NOT NULL,
    expires_at        TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_assistant_memories_session_expiry
    ON assistant_memories (voice_session_id, expires_at);

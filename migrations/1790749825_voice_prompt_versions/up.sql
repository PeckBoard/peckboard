-- Editable voice-assistant system prompt (`service::voice_prompt`). Every
-- change appends a row; the active prompt is the newest row. An empty table
-- (or a newest row with source 'default') means the built-in
-- VOICE_SYSTEM_PROMPT is active, so code updates to the default keep
-- reaching users who never customized or who reset. `content` on a
-- 'default' row is a snapshot of the built-in at reset time, for history
-- diffs only. Brand-new table only — no existing table is touched.
CREATE TABLE IF NOT EXISTS voice_prompt_versions (
    id          TEXT PRIMARY KEY NOT NULL,
    content     TEXT NOT NULL,
    source      TEXT NOT NULL CHECK (source IN ('default', 'user', 'assistant')),
    note        TEXT,
    created_at  TEXT NOT NULL,
    created_by  TEXT
);

CREATE INDEX IF NOT EXISTS idx_voice_prompt_versions_created
    ON voice_prompt_versions (created_at);

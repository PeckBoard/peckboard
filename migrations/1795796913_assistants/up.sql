-- Named voice assistants. Each row is one assistant the user can talk to:
-- its spoken name, its job (instructions added to the shared voice rules),
-- its voice, and the voice session (`sessions.expert_kind = 'voice'`) that
-- holds its conversation. `session_id` is NULL until the assistant is first
-- opened; topics, memory and the relay queue hang off that session id, as
-- they did when there was a single Assistant.
--
-- Exactly one row is the default (who answers when no name is said); the
-- partial unique index enforces it. Names are unique without regard to
-- case, because they are matched against speech.
--
-- Brand-new table only: no existing table is touched and no row is moved.
-- The Assistant an install already has is adopted as the default row by
-- `Db::default_assistant` the first time assistants are read.
CREATE TABLE IF NOT EXISTS assistants (
    id          TEXT PRIMARY KEY NOT NULL,
    name        TEXT NOT NULL,
    job         TEXT NOT NULL DEFAULT '',
    voice       TEXT,
    color       TEXT,
    icon        TEXT,
    template    TEXT,
    session_id  TEXT,
    is_default  BOOLEAN NOT NULL DEFAULT 0,
    position    INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_assistants_name
    ON assistants (name COLLATE NOCASE);

CREATE UNIQUE INDEX IF NOT EXISTS idx_assistants_one_default
    ON assistants (is_default) WHERE is_default = 1;

CREATE UNIQUE INDEX IF NOT EXISTS idx_assistants_session
    ON assistants (session_id) WHERE session_id IS NOT NULL;

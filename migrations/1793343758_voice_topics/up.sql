-- The voice assistant's topic ring (the full-page Assistant view). The
-- assistant model curates every row itself through its `assistant_topics`
-- tool: conversation threads, plus the sessions / projects it chose to put
-- on the ring (`ref_id`; the live status is computed at read time, never
-- stored). Scoped to one voice session. At most one topic per session is
-- current (the partial unique index; the crud also clears the old one in
-- the same transaction).
-- Brand-new table only — no existing table is touched.
CREATE TABLE IF NOT EXISTS voice_topics (
    id          TEXT PRIMARY KEY NOT NULL,
    session_id  TEXT NOT NULL,
    kind        TEXT NOT NULL CHECK (kind IN ('conversation', 'session', 'project')),
    title       TEXT NOT NULL,
    summary     TEXT,
    ref_id      TEXT,
    status      TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'closed')),
    is_current  BOOLEAN NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    CHECK (kind = 'conversation' OR ref_id IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS idx_voice_topics_session_status
    ON voice_topics (session_id, status);

CREATE UNIQUE INDEX IF NOT EXISTS idx_voice_topics_one_current
    ON voice_topics (session_id) WHERE is_current = 1;

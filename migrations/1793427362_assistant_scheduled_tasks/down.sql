-- Reverse rebuild: narrow `voice_relay_queue` back. Held `scheduled` relays
-- can't survive the old constraint, so they're dropped.
DELETE FROM voice_relay_queue WHERE kind = 'scheduled' OR source_session_id IS NULL;

CREATE TABLE IF NOT EXISTS voice_relay_queue_old (
    id                 TEXT PRIMARY KEY NOT NULL,
    voice_session_id   TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    source_session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    kind               TEXT NOT NULL CHECK (kind IN ('question', 'update')),
    question_event_id  TEXT,
    text               TEXT NOT NULL,
    summary            TEXT NOT NULL,
    created_at         TEXT NOT NULL
);
INSERT INTO voice_relay_queue_old (id, voice_session_id, source_session_id, kind, question_event_id, text, summary, created_at)
    SELECT id, voice_session_id, source_session_id, kind, question_event_id, text, summary, created_at FROM voice_relay_queue;
DROP TABLE voice_relay_queue;
ALTER TABLE voice_relay_queue_old RENAME TO voice_relay_queue;

CREATE INDEX IF NOT EXISTS idx_voice_relay_queue_voice
    ON voice_relay_queue (voice_session_id, created_at);

-- `repeating_tasks.target` is left in place: SQLite cannot drop a column
-- carrying a CHECK without a full table rebuild, and a stray nullable TEXT
-- column is harmless (the old binary never reads it).

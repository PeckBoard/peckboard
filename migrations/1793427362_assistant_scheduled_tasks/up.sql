-- Repeating tasks that run in the Assistant (the global voice session)
-- instead of spawning a new session.
--
-- `repeating_tasks.target`: NULL = a fresh session per run (every existing
-- row keeps today's behaviour); 'assistant' = the run is queued into the
-- voice relay gate as a `scheduled` relay and the Assistant performs it.
-- Defensive ADD COLUMN: matched by ensure_schema() in repair.rs.
ALTER TABLE repeating_tasks ADD COLUMN target TEXT
    CHECK (target IS NULL OR target = 'assistant');

-- `voice_relay_queue` gains a source-less `scheduled` kind (a due
-- Assistant-target task, waiting for a quiet gap). SQLite cannot alter a
-- CHECK or a NOT NULL in place, so the table is rebuilt; every held row is
-- copied across unchanged.
CREATE TABLE IF NOT EXISTS voice_relay_queue_new (
    id                 TEXT PRIMARY KEY NOT NULL,
    voice_session_id   TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    source_session_id  TEXT REFERENCES sessions(id) ON DELETE CASCADE,
    kind               TEXT NOT NULL CHECK (kind IN ('question', 'update', 'scheduled')),
    question_event_id  TEXT,
    text               TEXT NOT NULL,
    summary            TEXT NOT NULL,
    created_at         TEXT NOT NULL,
    CHECK (kind = 'scheduled' OR source_session_id IS NOT NULL)
);
INSERT INTO voice_relay_queue_new (id, voice_session_id, source_session_id, kind, question_event_id, text, summary, created_at)
    SELECT id, voice_session_id, source_session_id, kind, question_event_id, text, summary, created_at FROM voice_relay_queue;
DROP TABLE voice_relay_queue;
ALTER TABLE voice_relay_queue_new RENAME TO voice_relay_queue;

CREATE INDEX IF NOT EXISTS idx_voice_relay_queue_voice
    ON voice_relay_queue (voice_session_id, created_at);

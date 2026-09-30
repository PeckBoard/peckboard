-- Relays (other sessions' questions / turn-end updates) held for the voice
-- assistant session until the user is quiet and the topic fits — see
-- `service::voice_gate`. One row per held relay; a row is deleted the
-- moment it is delivered, so the table only ever holds the live queue and
-- it survives a restart. Brand-new table only — no existing table is
-- touched.
CREATE TABLE IF NOT EXISTS voice_relay_queue (
    id                 TEXT PRIMARY KEY NOT NULL,
    voice_session_id   TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    source_session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    kind               TEXT NOT NULL CHECK (kind IN ('question', 'update')),
    question_event_id  TEXT,
    text               TEXT NOT NULL,
    summary            TEXT NOT NULL,
    created_at         TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_voice_relay_queue_voice
    ON voice_relay_queue (voice_session_id, created_at);

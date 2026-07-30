-- Collapse the FIFO back to the old single-slot shape, keeping the
-- oldest queued message per session (the one the drain would have
-- delivered next).
CREATE TABLE queued_messages_slot (
    session_id TEXT PRIMARY KEY,
    text       TEXT NOT NULL,
    queued_at  TEXT NOT NULL,
    model      TEXT,
    effort     TEXT
);
INSERT INTO queued_messages_slot (session_id, text, queued_at, model, effort)
    SELECT session_id, text, queued_at, model, effort FROM queued_messages
    WHERE id IN (SELECT MIN(id) FROM queued_messages GROUP BY session_id);
DROP TABLE queued_messages;
ALTER TABLE queued_messages_slot RENAME TO queued_messages;

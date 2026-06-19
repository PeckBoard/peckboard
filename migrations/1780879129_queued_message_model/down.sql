-- Down migrations must rebuild the table since SQLite cannot DROP COLUMN
-- before 3.35 in all build configurations. Provided for completeness.
CREATE TABLE queued_messages_new (
    session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    text TEXT NOT NULL,
    queued_at TEXT NOT NULL
);
INSERT INTO queued_messages_new (session_id, text, queued_at)
    SELECT session_id, text, queued_at FROM queued_messages;
DROP TABLE queued_messages;
ALTER TABLE queued_messages_new RENAME TO queued_messages;

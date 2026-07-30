-- Queue rework: `queued_messages` was a single slot per session
-- (PRIMARY KEY session_id + replace_into), so a second message queued
-- while the agent was busy silently overwrote the first. Rebuild it as
-- a FIFO: one row per message, drained oldest-first on agent completion.
--
-- New columns:
--   id                  — autoincrement delivery order + per-message handle
--                         for the "send now" (force) and remove endpoints.
--   attachment_ids      — JSON array of attachment ids so a queued send
--                         keeps its images; bytes are re-resolved from the
--                         attachments dir at drain time.
--   user_event_appended — 1 when the enqueuer already wrote the `user`
--                         event (the /message route does); the drain only
--                         appends one when this is 0, which fixes queued
--                         messages showing up twice in the transcript.
CREATE TABLE queued_messages_fifo (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id          TEXT    NOT NULL,
    text                TEXT    NOT NULL,
    queued_at           TEXT    NOT NULL,
    model               TEXT,
    effort              TEXT,
    attachment_ids      TEXT,
    user_event_appended INTEGER NOT NULL DEFAULT 0
);
INSERT INTO queued_messages_fifo (session_id, text, queued_at, model, effort, user_event_appended)
    SELECT session_id, text, queued_at, model, effort, 1 FROM queued_messages;
DROP TABLE queued_messages;
ALTER TABLE queued_messages_fifo RENAME TO queued_messages;
CREATE INDEX idx_queued_messages_session ON queued_messages(session_id);

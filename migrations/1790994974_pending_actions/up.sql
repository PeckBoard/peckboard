-- Server-issued one-time confirmations (`service::voice_actions`). When the
-- voice assistant (and, later, an IM / phone channel) calls a gated tool the
-- server does NOT run it: it stores the exact call here and asks the owning
-- user to confirm on a server-rendered surface (the web voice panel today).
-- Only a human press on an authenticated route runs the STORED call, once.
-- Rows are never deleted: they double as the audit log of every gated call
-- (created / confirmed / cancelled / expired, by whom, and the result).
-- Brand-new table only — no existing table is touched.
CREATE TABLE IF NOT EXISTS pending_actions (
    id           TEXT PRIMARY KEY NOT NULL,
    user_id      TEXT NOT NULL,
    session_id   TEXT NOT NULL,
    channel      TEXT NOT NULL,
    tool         TEXT NOT NULL,
    args_json    TEXT NOT NULL,
    summary      TEXT NOT NULL,
    status       TEXT NOT NULL CHECK (status IN ('pending', 'confirmed', 'cancelled', 'expired')),
    created_at   TEXT NOT NULL,
    expires_at   TEXT NOT NULL,
    resolved_at  TEXT,
    resolved_by  TEXT,
    result_json  TEXT
);

CREATE INDEX IF NOT EXISTS idx_pending_actions_user_status
    ON pending_actions (user_id, status);

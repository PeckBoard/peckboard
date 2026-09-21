-- Audit log of bridged remote-agent actions: one row per remote_agent_*
-- MCP call routed to a device. `args_summary` is a short redacted
-- summary, never the full payload or any output; `status` is
-- ok / offline / timeout / disconnected / error.
CREATE TABLE IF NOT EXISTS device_activity (
    id              TEXT    PRIMARY KEY NOT NULL,
    device_id       TEXT    NOT NULL,
    session_id      TEXT,
    capability      TEXT    NOT NULL,
    args_summary    TEXT    NOT NULL,
    status          TEXT    NOT NULL,
    created_at      TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_device_activity_device
    ON device_activity(device_id, created_at);

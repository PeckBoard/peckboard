CREATE TABLE IF NOT EXISTS devices (
    id           TEXT PRIMARY KEY NOT NULL,
    user_id      TEXT NOT NULL,
    name         TEXT NOT NULL,
    platform     TEXT NOT NULL,
    secret_hash  TEXT NOT NULL,
    status       TEXT NOT NULL DEFAULT 'active',
    last_seen_at TEXT,
    created_at   TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_devices_user_id ON devices(user_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_devices_secret_hash ON devices(secret_hash);

CREATE TABLE IF NOT EXISTS mfa_methods (
    id             TEXT PRIMARY KEY NOT NULL,
    user_id        TEXT NOT NULL REFERENCES users(id),
    kind           TEXT NOT NULL,
    label          TEXT,
    secret_ct      TEXT,
    secret_nonce   TEXT,
    last_timestep  INTEGER,
    created_at     TEXT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_mfa_methods_user_kind
    ON mfa_methods(user_id, kind);

CREATE TABLE IF NOT EXISTS mfa_recovery_codes (
    id         TEXT PRIMARY KEY NOT NULL,
    user_id    TEXT NOT NULL REFERENCES users(id),
    code_hash  TEXT NOT NULL,
    used_at    TEXT
);

CREATE INDEX IF NOT EXISTS idx_mfa_recovery_codes_user
    ON mfa_recovery_codes(user_id);

CREATE TABLE IF NOT EXISTS mfa_challenges (
    id           TEXT PRIMARY KEY NOT NULL,
    user_id      TEXT NOT NULL REFERENCES users(id),
    token_hash   TEXT NOT NULL UNIQUE,
    created_at   INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL,
    consumed_at  INTEGER,
    failures     INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_mfa_challenges_user
    ON mfa_challenges(user_id);

CREATE TABLE IF NOT EXISTS mfa_pending (
    id            TEXT PRIMARY KEY NOT NULL,
    user_id       TEXT NOT NULL UNIQUE REFERENCES users(id),
    kind          TEXT NOT NULL,
    secret_ct     TEXT NOT NULL,
    secret_nonce  TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    expires_at    INTEGER NOT NULL
);

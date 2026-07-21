-- User-defined environment variables injected into agent sessions. A var is
-- either plaintext (`value` set) or encrypted with the user's login password
-- (`ciphertext`/`nonce`/`kdf_salt` set, `encrypted = 1`).
--
-- Encryption: per-var Argon2id (16-byte `kdf_salt`) derives a 256-bit key from
-- the password; AES-256-GCM with a fresh 12-byte `nonce` produces `ciphertext`
-- (base64, tag included). `encrypted_by` records which user's password unlocks
-- the var; NULL for plaintext. `name` is UNIQUE so upsert-by-name replaces in
-- place. Self-contained: nothing else references this table.
CREATE TABLE IF NOT EXISTS env_vars (
    id           TEXT PRIMARY KEY NOT NULL,
    name         TEXT NOT NULL UNIQUE,
    value        TEXT,               -- plaintext when not encrypted
    ciphertext   TEXT,               -- base64(AES-256-GCM ct incl. tag) when encrypted
    nonce        TEXT,               -- hex, 12 random bytes per encryption
    kdf_salt     TEXT,               -- hex, 16 random bytes per var
    encrypted    BOOLEAN NOT NULL DEFAULT 0,
    encrypted_by TEXT,               -- users.id whose password unlocks it; NULL for plain
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);

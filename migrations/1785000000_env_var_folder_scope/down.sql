-- Restore the unscoped table. Folder-scoped rows cannot exist in the old
-- UNIQUE(name) shape, so only global rows survive the downgrade.
CREATE TABLE env_vars_old (
    id           TEXT PRIMARY KEY NOT NULL,
    name         TEXT NOT NULL UNIQUE,
    value        TEXT,
    ciphertext   TEXT,
    nonce        TEXT,
    kdf_salt     TEXT,
    encrypted    BOOLEAN NOT NULL DEFAULT 0,
    encrypted_by TEXT,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);
INSERT INTO env_vars_old
    (id, name, value, ciphertext, nonce, kdf_salt, encrypted, encrypted_by, created_at, updated_at)
    SELECT id, name, value, ciphertext, nonce, kdf_salt, encrypted, encrypted_by, created_at, updated_at
    FROM env_vars WHERE folder_id IS NULL;
DROP TABLE env_vars;
ALTER TABLE env_vars_old RENAME TO env_vars;

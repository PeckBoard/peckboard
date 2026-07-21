-- Folder scoping for env vars: `folder_id` NULL = global, else the var only
-- applies to sessions in that folder (a folder var shadows a global one with
-- the same name). SQLite cannot drop the old UNIQUE(name) constraint, so the
-- table is rebuilt; uniqueness moves to partial indexes because a plain
-- UNIQUE(name, folder_id) treats NULLs as distinct and would allow duplicate
-- global names. Existing rows stay global (folder_id NULL).
CREATE TABLE env_vars_new (
    id           TEXT PRIMARY KEY NOT NULL,
    name         TEXT NOT NULL,
    value        TEXT,               -- plaintext when not encrypted
    ciphertext   TEXT,               -- base64(AES-256-GCM ct incl. tag) when encrypted
    nonce        TEXT,               -- hex, 12 random bytes per encryption
    kdf_salt     TEXT,               -- hex, 16 random bytes per var
    encrypted    BOOLEAN NOT NULL DEFAULT 0,
    encrypted_by TEXT,               -- users.id whose password unlocks it; NULL for plain
    folder_id    TEXT,               -- folders.id; NULL = global
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);
INSERT INTO env_vars_new
    (id, name, value, ciphertext, nonce, kdf_salt, encrypted, encrypted_by, folder_id, created_at, updated_at)
    SELECT id, name, value, ciphertext, nonce, kdf_salt, encrypted, encrypted_by, NULL, created_at, updated_at
    FROM env_vars;
DROP TABLE env_vars;
ALTER TABLE env_vars_new RENAME TO env_vars;
CREATE UNIQUE INDEX idx_env_vars_global_name ON env_vars(name) WHERE folder_id IS NULL;
CREATE UNIQUE INDEX idx_env_vars_folder_name ON env_vars(folder_id, name) WHERE folder_id IS NOT NULL;

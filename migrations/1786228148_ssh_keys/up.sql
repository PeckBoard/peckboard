CREATE TABLE IF NOT EXISTS ssh_keys (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    key_type TEXT NOT NULL,
    public_key TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    private_key_ciphertext TEXT NOT NULL,
    private_key_nonce TEXT NOT NULL,
    passphrase_ciphertext TEXT,
    passphrase_nonce TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    created_by TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_ssh_keys_name ON ssh_keys(name);

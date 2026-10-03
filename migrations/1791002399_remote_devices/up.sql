-- Devices paired for remote access via the relay (relay.peckboard.com).
-- `secret_ciphertext` is the 32-byte pairing secret sealed with
-- AES-256-GCM under the server-held `remote_access_key` file (AAD = id);
-- it is never returned by any API after creation. Revoking a device
-- deletes its row, and with it the secret.
CREATE TABLE IF NOT EXISTS remote_devices (
    id                TEXT PRIMARY KEY NOT NULL,
    user_id           TEXT NOT NULL,
    name              TEXT NOT NULL,
    secret_ciphertext BLOB NOT NULL,
    secret_nonce      BLOB NOT NULL,
    created_at        TEXT NOT NULL,
    last_connected_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_remote_devices_user_id ON remote_devices(user_id);

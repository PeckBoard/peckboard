-- Pairing v2 state per remote-access device. No row = legacy (S-only)
-- pairing. One row per `remote_devices` row:
--   pending : v2 link issued, unused            (link_expires_at set)
--   staged  : device key + rendezvous secret R issued; S still accepted
--             (re-delivery to the same key / legacy service) until the
--             device first connects with R
--   active  : device seen on R; remote_devices.secret_* tombstoned
-- Sealed blobs are AES-256-GCM under the server-held `remote_access_key`
-- with distinct AADs, so a ciphertext can't be moved between columns.
CREATE TABLE IF NOT EXISTS remote_device_enrollments (
    device_id                TEXT PRIMARY KEY NOT NULL
                             REFERENCES remote_devices(id) ON DELETE CASCADE,
    state                    TEXT NOT NULL CHECK (state IN ('pending','staged','active')),
    link_expires_at          TEXT,              -- NULL for legacy upgrades
    device_pubkey            BLOB CHECK (device_pubkey IS NULL OR length(device_pubkey) = 32),
    rendezvous_ciphertext    BLOB,              -- R, AAD = id || '/rendezvous/2'
    rendezvous_nonce         BLOB,
    link_secret_ciphertext   BLOB,              -- S kept only for the refuse loop after activation
    link_secret_nonce        BLOB,              --   AAD = id || '/link-refuse'
    link_refuse_until        TEXT,
    enrolled_at              TEXT,
    enrolled_from            TEXT,              -- "203.0.113.7:4000"
    device_name_hint         TEXT,
    activated_at             TEXT,
    reuse_attempts           INTEGER NOT NULL DEFAULT 0,
    last_reuse_at            TEXT,
    last_reuse_from          TEXT,
    created_at               TEXT NOT NULL,
    CHECK (state <> 'pending' OR link_expires_at IS NOT NULL),
    CHECK (state = 'pending' OR (device_pubkey IS NOT NULL
           AND rendezvous_ciphertext IS NOT NULL AND rendezvous_nonce IS NOT NULL))
);

CREATE INDEX IF NOT EXISTS idx_remote_device_enrollments_state
    ON remote_device_enrollments(state);

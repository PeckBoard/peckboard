-- The remote-access device an auth session was created through (login /
-- MFA verify / password change arriving over that device's relay tunnel),
-- so revoking the device also revokes the sessions it minted. Nullable, no
-- FK (revoke deletes the sessions itself): every session created any other
-- way, and every pre-existing row, stays NULL.
ALTER TABLE auth_sessions ADD COLUMN remote_device_id TEXT;

CREATE INDEX IF NOT EXISTS idx_auth_sessions_remote_device
    ON auth_sessions (remote_device_id);

DROP INDEX IF EXISTS idx_auth_sessions_remote_device;
ALTER TABLE auth_sessions DROP COLUMN remote_device_id;

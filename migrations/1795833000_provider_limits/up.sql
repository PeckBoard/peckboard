-- Provider accounts (or, for Claude, account + model) that are out of quota,
-- and when the provider said the limit resets. Written by
-- `provider::rate_limits` and reloaded at boot, so a restart keeps holding
-- dispatch instead of spawning one doomed turn per waiting card.
-- Brand-new table only: no existing table is touched.
CREATE TABLE IF NOT EXISTS provider_limits (
    -- `provider@account` or `provider@account/model`.
    key           TEXT PRIMARY KEY NOT NULL,
    -- The model id the limit was hit on, as stored on the session.
    model         TEXT NOT NULL,
    -- RFC 3339, UTC.
    limited_until TEXT NOT NULL,
    -- The provider's own message.
    reason        TEXT NOT NULL,
    noted_at      TEXT NOT NULL
);

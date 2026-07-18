-- Multiple Moonshot AI / Kimi Code accounts the spawned `kimi` CLI can run
-- as. Mirrors `grok_accounts` (see 1782713952_grok_accounts) so the account
-- UX is identical across providers. The active account for a session is
-- encoded into the session's model id as `kimi:<model>@<account_id>`; a bare
-- `kimi:<model>` keeps using the host's ambient `~/.kimi-code` credentials —
-- the implicit "Default" account.
--
-- `kind` selects how the account authenticates the spawned `kimi`:
--   'device'  -> a browser device-code login (`kimi login`) whose OAuth
--                tokens land inside `config_dir/config.toml`. The
--                `credential` column is just a non-secret marker ('device');
--                the real token is the per-account KIMI_CODE_HOME on disk.
--   'api_key' -> a Moonshot AI API key. PeckBoard writes a config.toml
--                (providers + model aliases) into `config_dir` and the key
--                is additionally injected as KIMI_API_KEY at spawn time.
--
-- `config_dir` is a per-account directory used as KIMI_CODE_HOME so accounts
-- don't clobber each other's local CLI state. The route that creates the row
-- fills it in. NULL means "inherit the host KIMI_CODE_HOME".
--
-- Budgets mirror grok_accounts exactly for UI parity; Kimi's prompt-mode
-- stream-json output does not currently expose token usage, so these are
-- reserved and simply never trip until usage is wired up.
CREATE TABLE IF NOT EXISTS kimi_accounts (
    id                  TEXT    PRIMARY KEY NOT NULL,
    name                TEXT    NOT NULL,
    kind                TEXT    NOT NULL,
    credential          TEXT    NOT NULL,
    config_dir          TEXT,
    budget_window_hours INTEGER,
    budget_limit_usd    REAL,
    budget_limit_tokens INTEGER,
    warn_threshold      REAL    NOT NULL DEFAULT 0.75,
    critical_threshold  REAL    NOT NULL DEFAULT 0.90,
    created_at          BIGINT  NOT NULL,
    updated_at          BIGINT  NOT NULL
);

-- Multiple Grok / xAI accounts the spawned `grok` CLI can run as. Mirrors
-- `claude_accounts` (see 1782694873_claude_accounts) so the account UX is
-- identical across providers. The active account for a session is encoded
-- into the session's model id as `grok:<model>@<account_id>`; a bare
-- `grok:<model>` keeps using the host's ambient `~/.grok` credentials — the
-- implicit "Default" account.
--
-- `kind` selects how the account authenticates the spawned `grok`:
--   'device'  -> a browser device-code login (`grok login --device-auth`)
--                whose credentials live in `config_dir/auth.json`. The
--                `credential` column is just a non-secret marker ('device');
--                the real token is the per-account GROK_HOME on disk.
--   'api_key' -> an xAI API key, injected as XAI_API_KEY at spawn time.
--
-- `config_dir` is a per-account directory used as GROK_HOME so accounts
-- don't clobber each other's local CLI state (and is where a device login
-- writes its auth.json). The route that creates the row fills it in. NULL
-- means "inherit the host GROK_HOME".
--
-- Budgets mirror claude_accounts exactly for UI parity; Grok's headless
-- streaming-json output does not currently expose token usage, so these
-- are reserved and simply never trip until usage is wired up.
CREATE TABLE IF NOT EXISTS grok_accounts (
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

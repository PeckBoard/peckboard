-- Multiple Claude/Anthropic accounts the spawned `claude` CLI can run
-- as. Each row is one credential the user has added; the active account
-- for a session is encoded into the session's model id as
-- `claude:<model>@<account_id>` (a bare `claude:<model>` keeps using the
-- host's ambient credentials — the implicit "Default" account — for
-- backward compatibility with every session/card stored before this).
--
-- `kind` selects which env var the credential is injected through at
-- spawn time:
--   'api_key'     -> ANTHROPIC_API_KEY        (sk-ant-... keys)
--   'oauth_token' -> CLAUDE_CODE_OAUTH_TOKEN  (subscription / setup-token)
--
-- `config_dir` is a per-account directory used as CLAUDE_CONFIG_DIR so
-- accounts don't clobber each other's local CLI state; the route that
-- creates the row fills it in (it knows the data dir). NULL means
-- "inherit the host config dir".
--
-- Budgets are user-set soft caps evaluated over a rolling window so the
-- UI can warn before a real Anthropic limit bites. All three budget
-- columns are nullable: NULL window or NULL limits == "no budget, never
-- warn". The thresholds are the fractions of the budget at which the UI
-- escalates to the warning / critical levels.
CREATE TABLE IF NOT EXISTS claude_accounts (
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

-- Attribute each recorded usage turn to the account that produced it so
-- per-account budgets and rollups are exact even after a session later
-- switches accounts. Nullable: historical rows and Default-account turns
-- carry NULL. No inline FK — the delete path nulls matching rows itself
-- so this ADD COLUMN stays within SQLite's idempotent-friendly subset
-- and the repair.rs healer can re-add an identical plain column.
ALTER TABLE usage_events ADD COLUMN account_id TEXT;

CREATE INDEX IF NOT EXISTS idx_usage_events_account
    ON usage_events (account_id, ts);

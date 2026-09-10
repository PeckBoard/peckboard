-- Multiple OpenAI Codex CLI accounts the spawned `codex` CLI can run as.
-- Mirrors `kimi_accounts` / `grok_accounts` so the account UX is identical
-- across providers. The active account for a session is encoded into the
-- session's model id as `codex:<model>@<account_id>`; a bare `codex:<model>`
-- keeps using the host's ambient `~/.codex` credentials — the implicit
-- "Default" account.
--
-- `kind` is always `'device'`: ChatGPT sign-in via `codex login --device-auth`.
-- The `credential` column is a non-secret marker ('device'); the real OAuth
-- tokens land in `config_dir/auth.json` (the per-account CODEX_HOME). API-key
-- login is not offered — ChatGPT sign-in is the only in-app path.
--
-- `config_dir` is a per-account directory used as CODEX_HOME so accounts
-- don't clobber each other's local CLI state. The route that creates the row
-- fills it in. NULL means "inherit the host CODEX_HOME".
--
-- Budgets mirror grok/kimi for UI parity.
CREATE TABLE IF NOT EXISTS codex_accounts (
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

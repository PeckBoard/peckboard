-- Durable plans authored by a session (worker or chat). A plan is the
-- markdown design a thinking model proposes for a card or a chat task; it
-- must survive model switches, agent termination, and clear_session (which
-- only truncates a session's events/todos), so it lives in its own table
-- rather than in the event log.
--
-- `session_id` is the creator session (so a chat's plan is reachable from
-- the session menu). `card_id`/`project_id` are set when the creator is a
-- worker on a card (so the card menu can find it). `status` tracks the
-- lifecycle; `version` bumps on each revision.
CREATE TABLE IF NOT EXISTS plans (
    id          TEXT PRIMARY KEY NOT NULL,
    session_id  TEXT NOT NULL,
    card_id     TEXT,
    project_id  TEXT,
    title       TEXT NOT NULL,
    markdown    TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'proposed',
    version     INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_plans_session ON plans (session_id);
CREATE INDEX IF NOT EXISTS idx_plans_card ON plans (card_id);

-- Per-line human review comments on a proposed plan. They persist (survive
-- reload) until folded into a revision, at which point `resolved` flips.
-- `anchor` is the 1-based source-markdown line the comment is attached to.
CREATE TABLE IF NOT EXISTS plan_comments (
    id          TEXT PRIMARY KEY NOT NULL,
    plan_id     TEXT NOT NULL,
    anchor      INTEGER NOT NULL,
    body        TEXT NOT NULL,
    resolved    BOOLEAN NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_plan_comments_plan ON plan_comments (plan_id);

-- One-shot flag: when set, the session's next turn gets the saved plan
-- injected ahead of the user message so the (thinking) model reviews the
-- completed work against the plan. Cleared after a single injection.
ALTER TABLE sessions ADD COLUMN pending_plan_review BOOLEAN NOT NULL DEFAULT 0;

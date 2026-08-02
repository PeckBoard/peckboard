-- User-defined workflows that layer on top of the hardcoded built-ins in
-- src/workflow.rs. Built-ins stay read-only and in-code; these tables hold
-- everything a user creates through Settings -> Workflows or the API.
--
-- Steps live in their own table (not a JSON blob in `custom_workflows`) so
-- they get real typing, a NOT NULL step name, and a uniqueness constraint —
-- see peckboard/AGENTS.md's migrations section on why JSON blobs are a
-- last resort, not a default.
CREATE TABLE IF NOT EXISTS custom_workflows (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    description  TEXT NOT NULL,
    priority     INTEGER NOT NULL,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS custom_workflow_steps (
    workflow_id  TEXT NOT NULL REFERENCES custom_workflows(id) ON DELETE CASCADE,
    position     INTEGER NOT NULL,
    step         TEXT NOT NULL,
    instructions TEXT NOT NULL,
    PRIMARY KEY (workflow_id, position)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_custom_workflow_steps_step
    ON custom_workflow_steps (workflow_id, step);

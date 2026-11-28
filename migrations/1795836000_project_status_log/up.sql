-- Who paused or resumed a project, and when. A project's status can be
-- flipped by a person, by any agent session through the MCP tools, or by
-- the server; `projects.status` only says what it is now.
-- Brand-new table only: no existing table is touched. Rows go with their
-- project (ON DELETE CASCADE; foreign keys are enforced).
CREATE TABLE IF NOT EXISTS project_status_log (
    id          TEXT PRIMARY KEY NOT NULL,
    project_id  TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    action      TEXT NOT NULL CHECK (action IN ('paused', 'resumed')),
    actor_kind  TEXT NOT NULL CHECK (actor_kind IN ('user', 'session', 'system')),
    -- The user id or session id; NULL for the system.
    actor_id    TEXT,
    -- The username or session name at the time.
    actor_name  TEXT NOT NULL,
    -- RFC 3339, UTC.
    at          TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_project_status_log_project
    ON project_status_log (project_id, at);

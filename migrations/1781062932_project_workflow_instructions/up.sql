-- Per-project, per-workflow, per-step additional instructions appended to
-- the built-in step prompt when a worker runs. Lets the user say "for the
-- in_progress step of fast-develop-software, also commit to master and
-- push" without rewriting the platform default. Empty/absent rows mean
-- "no additional instructions for that step".
CREATE TABLE IF NOT EXISTS project_workflow_instructions (
    project_id   TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    workflow_id  TEXT NOT NULL,
    step         TEXT NOT NULL,
    instructions TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL,
    PRIMARY KEY (project_id, workflow_id, step)
);

CREATE INDEX IF NOT EXISTS idx_pwi_project ON project_workflow_instructions (project_id);

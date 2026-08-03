-- Reverse rebuild: narrow the CHECK back. Rows carrying one of the two new
-- statuses can't survive the old constraint, so they're dropped.
DELETE FROM repeating_task_runs WHERE status IN ('corrupt_schedule', 'consumed_once');

CREATE TABLE IF NOT EXISTS repeating_task_runs_old (
    id          TEXT    PRIMARY KEY NOT NULL,
    task_id     TEXT    NOT NULL REFERENCES repeating_tasks(id),
    session_id  TEXT,
    started_at  TEXT    NOT NULL,
    status      TEXT    NOT NULL CHECK (status IN ('spawned', 'already_running', 'throttled', 'failed')),
    trigger     TEXT    NOT NULL CHECK (trigger IN ('scheduler', 'manual')),
    detail      TEXT
);
INSERT INTO repeating_task_runs_old (id, task_id, session_id, started_at, status, trigger, detail)
    SELECT id, task_id, session_id, started_at, status, trigger, detail FROM repeating_task_runs;
DROP TABLE repeating_task_runs;
ALTER TABLE repeating_task_runs_old RENAME TO repeating_task_runs;

CREATE INDEX IF NOT EXISTS idx_repeating_task_runs_task
    ON repeating_task_runs (task_id, started_at);

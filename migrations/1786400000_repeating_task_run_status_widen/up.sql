-- Widen the `repeating_task_runs.status` CHECK to admit the two refusal
-- outcomes the dispatcher already knows about but could never persist:
--
--   `corrupt_schedule`  the stored schedule can't be parsed, so the row is
--                       disabled instead of dispatched (src/repeating.rs).
--   `consumed_once`     a `once` schedule already fired; force-run refuses
--                       to re-fire it.
--
-- Both were rejected by the old CHECK, and `record_run` only logs a warning
-- on insert failure -- so the run-history view showed nothing for exactly
-- the cases an operator most needs ("why did my task stop?").
--
-- SQLite cannot alter a CHECK constraint in place, so the table is rebuilt.
CREATE TABLE IF NOT EXISTS repeating_task_runs_new (
    id          TEXT    PRIMARY KEY NOT NULL,
    task_id     TEXT    NOT NULL REFERENCES repeating_tasks(id),
    session_id  TEXT,
    started_at  TEXT    NOT NULL,
    status      TEXT    NOT NULL CHECK (status IN ('spawned', 'already_running', 'throttled', 'failed', 'corrupt_schedule', 'consumed_once')),
    trigger     TEXT    NOT NULL CHECK (trigger IN ('scheduler', 'manual')),
    detail      TEXT
);
INSERT INTO repeating_task_runs_new (id, task_id, session_id, started_at, status, trigger, detail)
    SELECT id, task_id, session_id, started_at, status, trigger, detail FROM repeating_task_runs;
DROP TABLE repeating_task_runs;
ALTER TABLE repeating_task_runs_new RENAME TO repeating_task_runs;

CREATE INDEX IF NOT EXISTS idx_repeating_task_runs_task
    ON repeating_task_runs (task_id, started_at);

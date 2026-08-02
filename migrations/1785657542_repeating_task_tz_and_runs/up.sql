-- Timezone-aware scheduling + per-run history for repeating tasks.
--
-- `timezone` is an optional IANA zone name (e.g. "America/New_York").
-- NULL preserves the existing behaviour of every pre-existing row:
-- schedules are computed in UTC. Populated rows compute daily/weekly/
-- monthly/once wall-clock times in that zone (chrono-tz), so "09:00"
-- survives DST instead of drifting an hour twice a year.
--
-- `repeating_task_runs` records the *dispatch* outcome of every
-- scheduler tick or manual "Run now" for a task -- not the eventual
-- agent result, which is already visible via the spawned session's
-- events. This gives the run-history view something to show even for
-- throttled/already-running/failed-dispatch ticks that never produced
-- a session at all.
ALTER TABLE repeating_tasks ADD COLUMN timezone TEXT;

CREATE TABLE IF NOT EXISTS repeating_task_runs (
    id          TEXT    PRIMARY KEY NOT NULL,
    task_id     TEXT    NOT NULL REFERENCES repeating_tasks(id),
    session_id  TEXT,
    started_at  TEXT    NOT NULL,
    status      TEXT    NOT NULL CHECK (status IN ('spawned', 'already_running', 'throttled', 'failed')),
    trigger     TEXT    NOT NULL CHECK (trigger IN ('scheduler', 'manual')),
    detail      TEXT
);

CREATE INDEX IF NOT EXISTS idx_repeating_task_runs_task
    ON repeating_task_runs (task_id, started_at);

-- Repeating tasks: a user-defined prompt + schedule that spawns a fresh
-- session on each tick. The "only one running session per task" invariant
-- lives in RepeatingTaskManager, but the schema gives us the link we need
-- to query "what sessions belong to task T" via `sessions.repeating_task_id`.
--
-- `schedule_kind` discriminates the JSON shape in `schedule_value`:
--   * "interval" -> {"minutes": N}            -- fire every N minutes
--   * "daily"    -> {"hour": H, "minute": M}  -- daily at HH:MM UTC
--   * "weekly"   -> {"weekday": 0..6, "hour": H, "minute": M}
--
-- `next_run_at` is denormalised so the scheduler tick is a single indexed
-- range scan instead of "load all tasks, recompute, filter". It's
-- recomputed on create/update and after every run.
CREATE TABLE IF NOT EXISTS repeating_tasks (
    id              TEXT PRIMARY KEY NOT NULL,
    name            TEXT NOT NULL,
    description     TEXT NOT NULL DEFAULT '',
    folder_id       TEXT NOT NULL REFERENCES folders(id),
    prompt          TEXT NOT NULL,
    schedule_kind   TEXT NOT NULL,
    schedule_value  TEXT NOT NULL,
    model           TEXT,
    effort          TEXT,
    enabled         BOOLEAN NOT NULL DEFAULT 1,
    next_run_at     TEXT,
    last_run_at     TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_repeating_tasks_folder
    ON repeating_tasks (folder_id);
CREATE INDEX IF NOT EXISTS idx_repeating_tasks_next_run
    ON repeating_tasks (next_run_at) WHERE enabled = 1;

-- Link sessions back to the task that spawned them so we can list runs
-- per task. Defensive ADD COLUMN: matched by ensure_schema() in repair.rs
-- in case the migration mis-applies on an existing DB.
ALTER TABLE sessions ADD COLUMN repeating_task_id TEXT REFERENCES repeating_tasks(id);
CREATE INDEX IF NOT EXISTS idx_sessions_repeating_task
    ON sessions (repeating_task_id);

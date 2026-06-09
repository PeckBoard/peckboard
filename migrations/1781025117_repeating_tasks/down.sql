-- Drop the index first (DROP TABLE drops associated indexes, but the
-- sessions index is on the surviving table and must come off explicitly).
DROP INDEX IF EXISTS idx_sessions_repeating_task;
DROP INDEX IF EXISTS idx_repeating_tasks_next_run;
DROP INDEX IF EXISTS idx_repeating_tasks_folder;

-- Recreate `sessions` without `repeating_task_id`. SQLite cannot DROP
-- COLUMN in old build configurations.
CREATE TABLE sessions_new (
    id              TEXT    PRIMARY KEY NOT NULL,
    name            TEXT    NOT NULL,
    folder_id       TEXT    NOT NULL REFERENCES folders(id),
    model           TEXT,
    effort          TEXT,
    is_worker       BOOLEAN NOT NULL DEFAULT 0,
    project_id      TEXT    REFERENCES projects(id),
    card_id         TEXT    REFERENCES cards(id),
    conversation_id TEXT,
    created_at      TEXT    NOT NULL,
    last_activity   TEXT    NOT NULL
);
INSERT INTO sessions_new
    SELECT id, name, folder_id, model, effort, is_worker, project_id,
           card_id, conversation_id, created_at, last_activity
    FROM sessions;
DROP TABLE sessions;
ALTER TABLE sessions_new RENAME TO sessions;

DROP TABLE IF EXISTS repeating_tasks;

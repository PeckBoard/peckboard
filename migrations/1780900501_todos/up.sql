-- Promote todos from event-derived snapshots to a dedicated, durable table.
-- A `TodoWrite` (or any provider's equivalent) is replace-all: each
-- snapshot wholly supersedes the previous list. So a session's current
-- todo state is exactly the rows here for that `session_id`.
--
-- `position` preserves the order the agent emitted, so the UI shows
-- items in the same sequence as the latest snapshot. `status` carries
-- the canonical lifecycle token (`pending` | `in_progress` | `done`),
-- matching `crate::todo::TodoStatus`.
--
-- We KEEP emitting `todo` events too — live WS updates and the
-- frontend `useProjectTodos` / `latestTodoSnapshot` paths depend on
-- them. This table is the load-time read source of truth.
CREATE TABLE IF NOT EXISTS todos (
    session_id   TEXT    NOT NULL,
    position     INTEGER NOT NULL,
    content      TEXT    NOT NULL,
    status       TEXT    NOT NULL,
    active_form  TEXT,
    updated_at   TEXT    NOT NULL,
    PRIMARY KEY (session_id, position)
);

CREATE INDEX IF NOT EXISTS idx_todos_session
    ON todos (session_id);

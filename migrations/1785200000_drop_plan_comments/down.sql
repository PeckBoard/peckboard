-- Restore the shape (never the rows — the up migration drops them).
CREATE TABLE IF NOT EXISTS plan_comments (
    id          TEXT PRIMARY KEY NOT NULL,
    plan_id     TEXT NOT NULL,
    anchor      INTEGER NOT NULL,
    body        TEXT NOT NULL,
    resolved    BOOLEAN NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_plan_comments_plan ON plan_comments (plan_id);

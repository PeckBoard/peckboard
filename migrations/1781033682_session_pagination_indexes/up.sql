-- Composite indexes for keyset-paginated session lists.
--
-- The session list endpoint (`GET /api/sessions`) used to load every row,
-- order it in memory, and ship the whole list to the browser. For a user
-- who never closes tabs the row count grows without bound; once it hits a
-- few thousand the request becomes the page-load bottleneck.
--
-- Keyset pagination on `(last_activity DESC, id DESC)` fixes this — but
-- only if the planner can use an index. SQLite indexes on ascending
-- columns serve both ASC and DESC scans, so plain `(last_activity, id)`
-- is enough. The folder-scoped variant adds `folder_id` as the leading
-- column so the planner can still seek directly to a folder's rows.
--
-- IF NOT EXISTS is defensive: a healed DB may already have these from
-- `src/db/repair.rs::ensure_sessions_pagination_indexes`.
CREATE INDEX IF NOT EXISTS idx_sessions_last_activity
    ON sessions (last_activity, id);

CREATE INDEX IF NOT EXISTS idx_sessions_folder_last_activity
    ON sessions (folder_id, last_activity, id);

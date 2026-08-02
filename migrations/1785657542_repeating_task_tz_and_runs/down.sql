-- `timezone` is left in place: SQLite cannot drop a column without a
-- full table rebuild, and a stray nullable TEXT column is harmless.
DROP INDEX IF EXISTS idx_repeating_task_runs_task;
DROP TABLE IF EXISTS repeating_task_runs;

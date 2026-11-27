-- `repeating_tasks.assistant_id` is left in place: a stray nullable TEXT
-- column is harmless (the old binary never reads it), and dropping it would
-- lose which assistant each task belongs to.
SELECT 1;

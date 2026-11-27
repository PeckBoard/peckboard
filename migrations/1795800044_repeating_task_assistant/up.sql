-- Which assistant performs an Assistant-target repeating task
-- (`repeating_tasks.target = 'assistant'`). NULL = the default assistant,
-- which is what every existing task means. No FK: when an assistant is
-- deleted its tasks fall back to the default one instead of vanishing.
--
-- `ALTER TABLE … ADD COLUMN` can't be made idempotent in SQLite;
-- `src/db/repair.rs::ensure_schema` adds the column on startup if missing.
ALTER TABLE repeating_tasks ADD COLUMN assistant_id TEXT;

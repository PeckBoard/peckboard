-- Per-project spend budgets with auto-pause.
--
-- `budget_usd_cents` is the spend cap for the period in US cents (integer
-- avoids floating-point equality issues). `budget_period` is the window:
-- 'daily' | 'weekly' | 'monthly' (UTC windows). Both are nullable — NULL
-- means no budget is configured.
--
-- Additive and nullable, so existing rows need no backfill (NULL = no budget).
ALTER TABLE projects ADD COLUMN budget_usd_cents INTEGER;
ALTER TABLE projects ADD COLUMN budget_period TEXT;

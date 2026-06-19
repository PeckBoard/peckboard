-- New-table migration: nothing else in the schema references
-- pm_decisions, so dropping it is a clean local rollback.
DROP INDEX IF EXISTS idx_pm_decisions_project_status;
DROP TABLE IF EXISTS pm_decisions;

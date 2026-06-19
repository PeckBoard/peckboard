-- SQLite gained ALTER TABLE DROP COLUMN in 3.35.0 (2021-03). The
-- `default_workflow` column was never dropped, so undoing the migration
-- only requires removing the `workflow` column.
ALTER TABLE projects DROP COLUMN workflow;

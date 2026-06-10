-- A project's workflow is now a required setting on the projects table.
-- The previous `default_workflow` column was nullable and framed as a
-- per-card default; this migration introduces a `workflow` column that
-- carries the actual project workflow, with a NOT NULL constraint.
--
-- The old `default_workflow` column is intentionally left in place per
-- the project's migration policy ("Never DROP in a forward migration").
-- It is no longer read by application code.

-- Add the new column. Existing rows get a temporary 'task' assignment
-- because SQLite ALTER TABLE ADD COLUMN cannot add NOT NULL without a
-- constant default; the UPDATE below replaces 'task' with each project's
-- previously-stored value whenever it was set.
ALTER TABLE projects ADD COLUMN workflow TEXT NOT NULL DEFAULT 'task';

-- Backfill from the legacy nullable column. Rows whose `default_workflow`
-- was NULL or empty keep the 'task' assignment from the ADD COLUMN step.
UPDATE projects
   SET workflow = default_workflow
 WHERE default_workflow IS NOT NULL
   AND default_workflow != '';

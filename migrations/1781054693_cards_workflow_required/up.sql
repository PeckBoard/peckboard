-- A card's workflow is now baked in at create time: when a user creates a
-- card without naming a workflow, the project's required workflow is copied
-- into `cards.workflow` rather than left NULL to be resolved at read time.
-- That decouples a card's step order from the project's current workflow
-- setting, which used to drift if the project's workflow was changed later.
--
-- Old `cards.workflow` was nullable. SQLite cannot add a NOT NULL
-- constraint to an existing column, so we rename the legacy column
-- (preserving all data) and add a new NOT NULL column under the same
-- name, then backfill from the legacy column when set, falling back to
-- the owning project's workflow, then to the platform default ('task').
--
-- The legacy column is intentionally left in place ("Never DROP in a
-- forward migration"). No application code reads it after this point.

ALTER TABLE cards RENAME COLUMN workflow TO workflow_legacy;
ALTER TABLE cards ADD COLUMN workflow TEXT NOT NULL DEFAULT 'task';

-- Backfill: prefer the card's own legacy value, then the owning project's
-- workflow, then 'task'. SQLite supports correlated subqueries in UPDATE,
-- which keeps this expressible as a single statement.
UPDATE cards
   SET workflow = COALESCE(
       NULLIF(workflow_legacy, ''),
       (SELECT workflow FROM projects WHERE projects.id = cards.project_id),
       'task'
   );

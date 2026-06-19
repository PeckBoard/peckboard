-- Drop the new NOT NULL column and rename the legacy one back. SQLite
-- gained ALTER TABLE DROP COLUMN in 3.35.0 (2021-03).
ALTER TABLE cards DROP COLUMN workflow;
ALTER TABLE cards RENAME COLUMN workflow_legacy TO workflow;

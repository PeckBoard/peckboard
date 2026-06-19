-- SQLite supports dropping tables; the rest of the schema doesn't reference
-- project_workflow_instructions, so removing it is a clean rollback. Per
-- project convention forward migrations never DROP, but DROP is fine here
-- because down.sql only runs when a developer locally reverts.
DROP TABLE IF EXISTS project_workflow_instructions;

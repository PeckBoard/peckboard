-- SQLite supports DROP COLUMN as of 3.35 (2021). Safe for local rollback.
ALTER TABLE sessions DROP COLUMN is_permanent;
ALTER TABLE sessions DROP COLUMN scope_path;
ALTER TABLE sessions DROP COLUMN knowledge_area;
ALTER TABLE sessions DROP COLUMN knowledge_summary;
ALTER TABLE sessions DROP COLUMN expert_kind;
ALTER TABLE sessions DROP COLUMN is_expert;

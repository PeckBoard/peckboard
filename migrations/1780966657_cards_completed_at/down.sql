-- SQLite supports DROP COLUMN as of 3.35 (2021). Safe for local rollback.
ALTER TABLE cards DROP COLUMN completed_at;

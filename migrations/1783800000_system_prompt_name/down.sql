-- SQLite supports DROP COLUMN as of 3.35 (2021). Safe for local rollback.
ALTER TABLE sessions DROP COLUMN system_prompt_name;
ALTER TABLE cards DROP COLUMN system_prompt_name;

-- SQLite can drop a column on modern versions; older ones require a
-- table rebuild. The column is nullable and unused when empty, so leaving
-- it in place on downgrade is harmless; we drop it for a clean down path.
ALTER TABLE sessions DROP COLUMN system_prompt;

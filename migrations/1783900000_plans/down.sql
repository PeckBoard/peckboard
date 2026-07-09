-- SQLite supports DROP COLUMN as of 3.35 (2021). Safe for local rollback.
ALTER TABLE sessions DROP COLUMN pending_plan_review;
DROP TABLE IF EXISTS plan_comments;
DROP TABLE IF EXISTS plans;

-- SQLite supports DROP COLUMN as of 3.35 (2021). Safe for local rollback.
ALTER TABLE sessions DROP COLUMN pending_doc_review;
DROP TABLE IF EXISTS doc_review_comments;
DROP TABLE IF EXISTS doc_review_versions;
DROP TABLE IF EXISTS doc_reviews;

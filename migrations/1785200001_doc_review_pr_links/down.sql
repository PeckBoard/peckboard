DROP INDEX IF EXISTS idx_doc_review_comments_external;
ALTER TABLE doc_review_comments DROP COLUMN external_id;
ALTER TABLE doc_review_comments DROP COLUMN external_kind;
DROP TABLE IF EXISTS doc_review_pr_links;

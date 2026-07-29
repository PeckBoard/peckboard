-- A review of a markdown file can be tied to the GitHub pull request that
-- changes that file. GitHub's line comments on the file arrive as
-- annotations; resolving one replies on its thread.
--
-- One link per review: a review is one document, and a document under two
-- pull requests at once has no single set of line numbers to agree with.
CREATE TABLE IF NOT EXISTS doc_review_pr_links (
    review_id      TEXT    PRIMARY KEY NOT NULL REFERENCES doc_reviews(id) ON DELETE CASCADE,
    owner          TEXT    NOT NULL,
    repo           TEXT    NOT NULL,
    number         INTEGER NOT NULL,
    -- Repo-relative path of the reviewed file. GitHub reports a review
    -- comment's `path` the same way, and that is what they are matched on.
    file_path      TEXT    NOT NULL,
    last_synced_at TEXT,
    created_at     TEXT    NOT NULL
);

-- Where an imported annotation came from, so a re-sync recognises what it
-- already has instead of duplicating it, and so a resolution knows which
-- thread to answer. `external_kind` is 'github_pr'; `external_id` is the
-- GitHub review-comment id.
ALTER TABLE doc_review_comments ADD COLUMN external_kind TEXT;
ALTER TABLE doc_review_comments ADD COLUMN external_id TEXT;

-- NULLs compare distinct in SQLite, so every hand-written annotation still
-- inserts freely; only imported ids are held unique per review.
CREATE UNIQUE INDEX IF NOT EXISTS idx_doc_review_comments_external
    ON doc_review_comments (review_id, external_id);

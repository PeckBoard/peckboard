-- Document Review: an AI-assisted review pass over any markdown document
-- (a folder file, a report, or a saved plan). The user annotates the
-- rendered doc; a dedicated review session revises it. Every revision is a
-- new version row, never a silent overwrite, so the doc's history survives
-- model switches, agent termination, and clear_session (which only
-- truncates a session's events/todos).
--
-- `source_ref` addresses the document inside its `source_kind`:
--   file   -> "<folder_id>:<relative/path.md>"
--   report -> "<YYYY-MM-DD>/<file.md>"
--   plan   -> "<plan_id>"
-- `session_id` is the review AI session, created lazily on the first pass
-- (nullable until then, and nulled again if that session is deleted).
--
-- `source_kind` carries a CHECK because it selects the read/write adapter —
-- a typo'd value yields a review no adapter can resolve. `status` does not:
-- it is a lifecycle field, and SQLite can only change a CHECK by recreating
-- the table (see 1781202566_user_tabs_more_kinds).
CREATE TABLE IF NOT EXISTS doc_reviews (
    id              TEXT    PRIMARY KEY NOT NULL,
    title           TEXT    NOT NULL,
    source_kind     TEXT    NOT NULL CHECK (source_kind IN ('file', 'report', 'plan')),
    source_ref      TEXT    NOT NULL,
    folder_id       TEXT    REFERENCES folders(id),
    project_id      TEXT,
    session_id      TEXT,
    -- annotating | running | needs_input | approved
    status          TEXT    NOT NULL DEFAULT 'annotating',
    current_version INTEGER NOT NULL DEFAULT 1,
    created_at      TEXT    NOT NULL,
    updated_at      TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_doc_reviews_status ON doc_reviews (status);
CREATE INDEX IF NOT EXISTS idx_doc_reviews_session ON doc_reviews (session_id);
CREATE INDEX IF NOT EXISTS idx_doc_reviews_folder ON doc_reviews (folder_id);

-- One immutable snapshot of the document per pass. Version 1 is the
-- markdown as first loaded from the source; each assistant revision (and
-- each user revert) appends the next version. `note` describes what
-- changed. The composite primary key gives the UNIQUE(review_id, version)
-- the feature contract requires.
CREATE TABLE IF NOT EXISTS doc_review_versions (
    review_id  TEXT    NOT NULL REFERENCES doc_reviews(id) ON DELETE CASCADE,
    version    INTEGER NOT NULL,
    markdown   TEXT    NOT NULL,
    note       TEXT    NOT NULL,
    created_by TEXT    NOT NULL CHECK (created_by IN ('user', 'assistant')),
    created_at TEXT    NOT NULL,
    PRIMARY KEY (review_id, version)
);

CREATE INDEX IF NOT EXISTS idx_doc_review_versions_review ON doc_review_versions (review_id);

-- A user annotation anchored to a line range of a given version.
-- `kind` drives how the review session is asked to act on it, so it carries
-- a CHECK. `status` tracks the annotation through a pass:
-- pending -> sent -> fixed | declined | answered.
CREATE TABLE IF NOT EXISTS doc_review_comments (
    id              TEXT    PRIMARY KEY NOT NULL,
    review_id       TEXT    NOT NULL REFERENCES doc_reviews(id) ON DELETE CASCADE,
    version         INTEGER NOT NULL,
    start_line      INTEGER NOT NULL,
    end_line        INTEGER NOT NULL,
    quote           TEXT,
    kind            TEXT    NOT NULL CHECK (kind IN ('comment', 'suggest', 'wrong', 'expand', 'shorten')),
    body            TEXT    NOT NULL,
    -- pending | sent | fixed | declined | answered
    status          TEXT    NOT NULL DEFAULT 'pending',
    resolution_note TEXT,
    created_at      TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_doc_review_comments_review ON doc_review_comments (review_id);
CREATE INDEX IF NOT EXISTS idx_doc_review_comments_status ON doc_review_comments (review_id, status);

-- One-shot flag: when set to a review id, the session's next turn gets that
-- review's document + open comments injected ahead of the user message.
-- Cleared after a single injection (see src/handover.rs).
ALTER TABLE sessions ADD COLUMN pending_doc_review TEXT;

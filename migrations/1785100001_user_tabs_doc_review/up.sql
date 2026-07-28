-- Widen the user_tabs.item_type CHECK to allow 'doc_review' tabs, so a
-- Document Review can be opened as a tab alongside sessions, projects,
-- reports, and repeating tasks.
--
-- SQLite has no ALTER TABLE for CHECK constraints, so we recreate the
-- table and copy every row across (same pattern as
-- 1781202566_user_tabs_more_kinds). INSERT … SELECT preserves every tab
-- the user had open; the migration runs in a single transaction, so a
-- partial failure leaves the original table untouched.
--
-- The CHECK stays (not dropped) on purpose: user_tabs is polymorphic with
-- no FK, and a typo'd item_type would silently create a dead row no UI can
-- resolve. Adding a kind costs a migration — that's the point.

CREATE TABLE IF NOT EXISTS user_tabs_new (
    user_id     TEXT    NOT NULL REFERENCES users(id),
    item_type   TEXT    NOT NULL CHECK (item_type IN ('session', 'project', 'report', 'repeating_task', 'doc_review')),
    item_id     TEXT    NOT NULL,
    last_active TEXT    NOT NULL,
    PRIMARY KEY (user_id, item_type, item_id)
);

INSERT INTO user_tabs_new (user_id, item_type, item_id, last_active)
    SELECT user_id, item_type, item_id, last_active FROM user_tabs;

DROP TABLE user_tabs;
ALTER TABLE user_tabs_new RENAME TO user_tabs;

CREATE INDEX IF NOT EXISTS idx_user_tabs_user_active ON user_tabs (user_id, last_active DESC);

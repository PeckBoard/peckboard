-- Relax the user_tabs.item_type CHECK constraint to allow 'report' and
-- 'repeating_task' tabs in addition to 'session' / 'project'.
--
-- SQLite has no ALTER TABLE for CHECK constraints, so we recreate the
-- table and copy every row across. INSERT … SELECT preserves every row
-- the user had open, including any worker-session tabs. The wrapping
-- migration runs in a single transaction, so a partial failure leaves
-- the original table untouched.
--
-- The CHECK is included (not dropped) on purpose: this is a polymorphic
-- table with no FK, and a typo'd item_type at insert time would silently
-- create a dead row no UI can resolve. Keeping the CHECK means adding a
-- new kind requires a new migration — that's a feature, not a bug, for
-- a polymorphic catch-all table.

CREATE TABLE IF NOT EXISTS user_tabs_new (
    user_id     TEXT    NOT NULL REFERENCES users(id),
    item_type   TEXT    NOT NULL CHECK (item_type IN ('session', 'project', 'report', 'repeating_task')),
    item_id     TEXT    NOT NULL,
    last_active TEXT    NOT NULL,
    PRIMARY KEY (user_id, item_type, item_id)
);

INSERT INTO user_tabs_new (user_id, item_type, item_id, last_active)
    SELECT user_id, item_type, item_id, last_active FROM user_tabs;

DROP TABLE user_tabs;
ALTER TABLE user_tabs_new RENAME TO user_tabs;

CREATE INDEX IF NOT EXISTS idx_user_tabs_user_active ON user_tabs (user_id, last_active DESC);

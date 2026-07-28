-- Restore the pre-doc_review CHECK. Doc-review tabs get dropped on the way
-- down — they cannot be represented under the narrower CHECK.

CREATE TABLE IF NOT EXISTS user_tabs_old (
    user_id     TEXT    NOT NULL REFERENCES users(id),
    item_type   TEXT    NOT NULL CHECK (item_type IN ('session', 'project', 'report', 'repeating_task')),
    item_id     TEXT    NOT NULL,
    last_active TEXT    NOT NULL,
    PRIMARY KEY (user_id, item_type, item_id)
);

INSERT INTO user_tabs_old (user_id, item_type, item_id, last_active)
    SELECT user_id, item_type, item_id, last_active FROM user_tabs
    WHERE item_type IN ('session', 'project', 'report', 'repeating_task');

DROP TABLE user_tabs;
ALTER TABLE user_tabs_old RENAME TO user_tabs;

CREATE INDEX IF NOT EXISTS idx_user_tabs_user_active ON user_tabs (user_id, last_active DESC);

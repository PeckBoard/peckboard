-- Per-user list of opened "tabs" — sessions and projects the user has
-- visited recently, ordered by `last_active`. Used by the frontend
-- tab strip so the same set of tabs appears on every device a user
-- signs in from.
CREATE TABLE user_tabs (
    user_id     TEXT    NOT NULL REFERENCES users(id),
    item_type   TEXT    NOT NULL CHECK (item_type IN ('session', 'project')),
    item_id     TEXT    NOT NULL,
    last_active TEXT    NOT NULL,
    PRIMARY KEY (user_id, item_type, item_id)
);

CREATE INDEX idx_user_tabs_user_active ON user_tabs (user_id, last_active DESC);

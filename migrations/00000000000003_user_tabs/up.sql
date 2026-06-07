-- Per-user list of opened "tabs" — sessions and projects the user has
-- visited recently, ordered by `last_active`. Used by the frontend
-- tab strip so the same set of tabs appears on every device a user
-- signs in from.
--
-- IF NOT EXISTS because an earlier version of this migration shipped
-- under id 00000000000002_user_tabs, which collided with the upstream
-- worker_communication migration. Data dirs created in that window
-- already have the table; we just need to not re-create it.
CREATE TABLE IF NOT EXISTS user_tabs (
    user_id     TEXT    NOT NULL REFERENCES users(id),
    item_type   TEXT    NOT NULL CHECK (item_type IN ('session', 'project')),
    item_id     TEXT    NOT NULL,
    last_active TEXT    NOT NULL,
    PRIMARY KEY (user_id, item_type, item_id)
);

CREATE INDEX IF NOT EXISTS idx_user_tabs_user_active ON user_tabs (user_id, last_active DESC);

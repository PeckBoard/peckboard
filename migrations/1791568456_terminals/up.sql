-- Interactive SSH terminals: one row per shell the user opened from a
-- plugin-registered host. The row is the durable identity of the terminal
-- (its tab, its list entry, and the remote tmux session it reattaches to)
-- and carries NO credentials — the host reference (plugin_id + host_id) is
-- resolved through the owning plugin at every connect.
CREATE TABLE IF NOT EXISTS terminals (
    id             TEXT    PRIMARY KEY NOT NULL,
    user_id        TEXT    NOT NULL REFERENCES users(id),
    plugin_id      TEXT    NOT NULL,
    host_id        TEXT    NOT NULL,
    name           TEXT    NOT NULL,
    host_label     TEXT    NOT NULL,
    tmux_session   TEXT    NOT NULL,
    persistent     BOOLEAN NOT NULL DEFAULT 0,
    created_at     TEXT    NOT NULL,
    last_active_at TEXT    NOT NULL,
    closed_at      TEXT
);

CREATE INDEX IF NOT EXISTS idx_terminals_user_open
    ON terminals (user_id, closed_at, last_active_at DESC);

-- Widen the user_tabs.item_type CHECK to allow 'terminal' tabs, so a
-- terminal opens as a tab next to sessions. Same recreate-and-copy pattern
-- as 1785100001_user_tabs_doc_review (SQLite cannot ALTER a CHECK).
CREATE TABLE IF NOT EXISTS user_tabs_new (
    user_id     TEXT    NOT NULL REFERENCES users(id),
    item_type   TEXT    NOT NULL CHECK (item_type IN ('session', 'project', 'report', 'repeating_task', 'doc_review', 'terminal')),
    item_id     TEXT    NOT NULL,
    last_active TEXT    NOT NULL,
    PRIMARY KEY (user_id, item_type, item_id)
);

INSERT INTO user_tabs_new (user_id, item_type, item_id, last_active)
    SELECT user_id, item_type, item_id, last_active FROM user_tabs;

DROP TABLE user_tabs;
ALTER TABLE user_tabs_new RENAME TO user_tabs;

CREATE INDEX IF NOT EXISTS idx_user_tabs_user_active ON user_tabs (user_id, last_active DESC);

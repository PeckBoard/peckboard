-- Saved multi-session split views: a per-user named layout tree whose
-- leaves each show one session. `session_views` is the named view;
-- `session_view_nodes` is its layout tree (split nodes with a direction,
-- leaf nodes pointing at a session). `ratio` is the node's share of its
-- parent split; `position` orders siblings.
--
-- Deleting a view cascades to its nodes; deleting a session only blanks
-- the leaf (session_id -> NULL) so the layout survives. Brand-new tables
-- only — no existing table is touched.

CREATE TABLE IF NOT EXISTS session_views (
    id          TEXT    PRIMARY KEY NOT NULL,
    user_id     TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name        TEXT    NOT NULL,
    created_at  TEXT    NOT NULL,
    updated_at  TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_session_views_user ON session_views (user_id);

CREATE TABLE IF NOT EXISTS session_view_nodes (
    id              TEXT    PRIMARY KEY NOT NULL,
    view_id         TEXT    NOT NULL REFERENCES session_views(id) ON DELETE CASCADE,
    parent_node_id  TEXT    REFERENCES session_view_nodes(id) ON DELETE CASCADE,
    position        INTEGER NOT NULL,
    kind            TEXT    NOT NULL CHECK (kind IN ('split', 'leaf')),
    dir             TEXT    CHECK (dir IN ('row', 'col')),
    ratio           REAL    NOT NULL DEFAULT 1.0,
    session_id      TEXT    REFERENCES sessions(id) ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS idx_session_view_nodes_view ON session_view_nodes (view_id);

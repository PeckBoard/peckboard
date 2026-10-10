-- Dashboard widgets: supersedes `view_widgets` (whose `kind` CHECK can't be
-- widened in place in SQLite). Same columns minus the CHECK — the app
-- validates `kind` against its enum — plus the refs/content the new kinds
-- need: `card_id` (dependencies root), `report_ref` ("<folder>/<file>"),
-- `body` (note markdown), and `host_ref` (opaque ssh-fleet plugin host id,
-- no FK — plugin-owned). Deleting a view cascades; deleting a target only
-- blanks the ref (ON DELETE SET NULL).
--
-- Every existing `view_widgets` row is copied. `view_widgets` and
-- `session_view_nodes` are left in place, unused — never dropped.
-- `view_widgets_converted` keeps its meaning.

CREATE TABLE IF NOT EXISTS dashboard_widgets (
    id           TEXT    NOT NULL,
    view_id      TEXT    NOT NULL REFERENCES session_views(id) ON DELETE CASCADE,
    kind         TEXT    NOT NULL,
    x            INTEGER NOT NULL,
    y            INTEGER NOT NULL,
    w            INTEGER NOT NULL,
    h            INTEGER NOT NULL,
    session_id   TEXT    NULL REFERENCES sessions(id) ON DELETE SET NULL,
    terminal_id  TEXT    NULL REFERENCES terminals(id) ON DELETE SET NULL,
    project_id   TEXT    NULL REFERENCES projects(id) ON DELETE SET NULL,
    card_id      TEXT    NULL REFERENCES cards(id) ON DELETE SET NULL,
    report_ref   TEXT    NULL,
    body         TEXT    NULL,
    host_ref     TEXT    NULL,
    PRIMARY KEY (view_id, id)
);

CREATE INDEX IF NOT EXISTS idx_dashboard_widgets_view ON dashboard_widgets (view_id);

INSERT OR IGNORE INTO dashboard_widgets
    (id, view_id, kind, x, y, w, h, session_id, terminal_id, project_id)
SELECT id, view_id, kind, x, y, w, h, session_id, terminal_id, project_id
FROM view_widgets;

-- Widget dashboards for saved views: each row is one rectangle on a
-- 12-column grid (`x`/`w` in columns, `y`/`h` in rows) showing a session,
-- an SSH terminal, or a project summary. `id` is client-generated and
-- unique per view. At most the ref column matching `kind` is set (enforced
-- by the app; all NULL is an empty placeholder). Deleting a view cascades;
-- deleting the target only blanks the ref (ON DELETE SET NULL).
--
-- `view_widgets_converted` marks views whose widget rows are authoritative
-- (created/saved as widgets, or a legacy `session_view_nodes` tree already
-- converted once on first read), so a view emptied to zero widgets never
-- re-converts its old tree.
--
-- Brand-new tables only. `session_view_nodes` is left untouched.

CREATE TABLE IF NOT EXISTS view_widgets (
    id           TEXT    NOT NULL,
    view_id      TEXT    NOT NULL REFERENCES session_views(id) ON DELETE CASCADE,
    kind         TEXT    NOT NULL CHECK (kind IN ('session', 'terminal', 'project')),
    x            INTEGER NOT NULL,
    y            INTEGER NOT NULL,
    w            INTEGER NOT NULL,
    h            INTEGER NOT NULL,
    session_id   TEXT    NULL REFERENCES sessions(id) ON DELETE SET NULL,
    terminal_id  TEXT    NULL REFERENCES terminals(id) ON DELETE SET NULL,
    project_id   TEXT    NULL REFERENCES projects(id) ON DELETE SET NULL,
    PRIMARY KEY (view_id, id)
);

CREATE INDEX IF NOT EXISTS idx_view_widgets_view ON view_widgets (view_id);

CREATE TABLE IF NOT EXISTS view_widgets_converted (
    view_id       TEXT    PRIMARY KEY NOT NULL REFERENCES session_views(id) ON DELETE CASCADE,
    converted_at  TEXT    NOT NULL
);

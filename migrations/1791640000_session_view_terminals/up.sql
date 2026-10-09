-- Terminal panes in saved views: a leaf node shows either a session
-- (`session_id`) or an SSH terminal (`terminal_id`), never both. Additive
-- only. Terminals are normally soft-closed (`closed_at`), which keeps the
-- id here so the pane can offer "Reopen on <host>"; a hard delete (user
-- removal) blanks the leaf via ON DELETE SET NULL.
ALTER TABLE session_view_nodes ADD COLUMN terminal_id TEXT NULL REFERENCES terminals(id) ON DELETE SET NULL;

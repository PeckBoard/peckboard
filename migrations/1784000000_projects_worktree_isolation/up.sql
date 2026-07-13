-- Opt-in worktree isolation: each card's worker runs in its own git worktree.
-- Defaults to 0 (off) — must be explicitly enabled per project.
ALTER TABLE projects ADD COLUMN worktree_isolation BOOLEAN NOT NULL DEFAULT 0;

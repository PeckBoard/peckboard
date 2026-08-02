-- Durable record of a card's git worktree that could not be merged back
-- into the project folder (dirty tree, merge conflict, or a cleanup step
-- that failed). NULL reason = nothing pending. Survives restart so the UI
-- can offer "Retry merge" instead of waiting for the janitor to notice.
-- Nullable + additive, so existing rows need no backfill.
ALTER TABLE cards ADD COLUMN worktree_unmerged_reason TEXT;
ALTER TABLE cards ADD COLUMN worktree_unmerged_detail TEXT;

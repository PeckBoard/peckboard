-- Plan review moved onto Document Review.
--
-- `plan_comments` was a second, weaker copy of `doc_review_comments`: one
-- flat line anchor, no versions, no diff, no revision history, and a
-- "review complete" that flattened every note into a single chat message.
-- A plan is now reviewed as a `plan`-kind doc review like any other
-- document, so it gets passes, versions, resolutions and an audit trail
-- from the machinery that already exists.
--
-- The rows are dropped, not migrated: they belonged to a flow that is gone,
-- and a plan whose review still matters is one `New Review` away.
DROP INDEX IF EXISTS idx_plan_comments_plan;
DROP TABLE IF EXISTS plan_comments;

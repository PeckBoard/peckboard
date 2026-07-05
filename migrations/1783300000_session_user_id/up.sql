-- Per-session owner: the authenticated PeckBoard user who owns the session.
-- Nullable, no FK (matching sessions' other ALTER-added columns like
-- worker_step / system_prompt): legacy rows and internally-spawned sessions
-- that resolve to no single user stay NULL.
--
-- NULL policy for the follow-up send_message same-user gate: two sessions are
-- "same user" ONLY when both user_ids are non-NULL and equal. NULL is unknown
-- / non-matching -- all-NULL sessions are never mutually messageable.
ALTER TABLE sessions ADD COLUMN user_id TEXT;

-- Backfill: single-operator installs (exactly one user) own every existing
-- session. Multi-user installs stay NULL (ambiguous -- left for the operator).
UPDATE sessions
   SET user_id = (SELECT id FROM users)
 WHERE user_id IS NULL
   AND (SELECT COUNT(*) FROM users) = 1;

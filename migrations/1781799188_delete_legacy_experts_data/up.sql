-- Delete legacy core experts data.
--
-- The experts + PM-expert features moved into the experts WASM plugin. Core no
-- longer creates `is_expert` sessions or `pm_decisions` rows: the plugin owns
-- experts as ordinary sessions tagged in its own per-session metadata, and PM
-- decisions live in the plugin's document store. The data left behind by the
-- old core implementation is therefore stale and orphaned, so we remove it
-- (explicitly authorized — no migration of the old data into the plugin).
--
-- Per the project's no-drop rule we delete the DATA only; the `is_expert` /
-- `expert_kind` columns and the `pm_decisions` table are left in place as an
-- unused vestige. This statement is idempotent: a second run deletes nothing
-- (the rows are already gone), and on a fresh DB it is a no-op.

-- Conversation events belonging to expert sessions (no FK cascade in SQLite).
DELETE FROM events WHERE session_id IN (SELECT id FROM sessions WHERE is_expert = 1);

-- The expert sessions themselves (knowledge / question / PM experts).
DELETE FROM sessions WHERE is_expert = 1;

-- The old PM decision store.
DELETE FROM pm_decisions;

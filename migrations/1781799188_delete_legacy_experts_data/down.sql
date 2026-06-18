-- Irreversible. This migration deletes the legacy core experts data (expert
-- sessions, their events, and PM decisions). That data cannot be reconstructed,
-- so there is nothing to undo. No-op, present only because diesel requires a
-- down.sql for every migration.
SELECT 1;

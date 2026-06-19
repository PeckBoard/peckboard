-- Track when a card transitioned into the `done` step so the Kanban
-- "Done" column can sort by most-recently-finished first.
--
-- Nullable: cards that are not (and never were) in `done` carry NULL.
-- A backfill in `src/db/repair.rs::ensure_schema()` populates this
-- column for existing `done` rows on older DBs (using `updated_at` as
-- a best-effort proxy), so the heal path doesn't depend on this
-- migration having ever run.
ALTER TABLE cards ADD COLUMN completed_at TEXT;

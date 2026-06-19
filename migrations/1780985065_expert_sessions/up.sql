-- Persistent data model for Expert Sessions: long-lived agent sessions
-- that hold codebase knowledge and answer questions from chat/worker
-- sessions. These columns live on `sessions` because an expert IS a
-- session with extra metadata, not a separate entity.
--
-- All columns are additive and either carry a DEFAULT (the two BOOLEAN
-- flags) or are NULL-able (the descriptive TEXT fields), so existing
-- rows backfill automatically. SQLite cannot make `ADD COLUMN`
-- idempotent, so `src/db/repair.rs::ensure_schema()` re-adds each of
-- these on startup if a DB is missing them.
ALTER TABLE sessions ADD COLUMN is_expert BOOLEAN NOT NULL DEFAULT 0;
-- NULL for non-experts; 'knowledge' or 'question' for experts.
ALTER TABLE sessions ADD COLUMN expert_kind TEXT;
-- The expert's own summary of what it knows.
ALTER TABLE sessions ADD COLUMN knowledge_summary TEXT;
-- The topic/area the expert owns.
ALTER TABLE sessions ADD COLUMN knowledge_area TEXT;
-- The folder boundary that defines the expert's knowledge scope.
ALTER TABLE sessions ADD COLUMN scope_path TEXT;
-- True for the long-lived question-experts that rehydrate under a
-- stable id from their report files.
ALTER TABLE sessions ADD COLUMN is_permanent BOOLEAN NOT NULL DEFAULT 0;

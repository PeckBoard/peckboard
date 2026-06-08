-- Add `model` and `effort` to queued_messages so the drain dispatches the
-- run with the same provider/model the user picked when queueing, rather
-- than falling back to "default" (which routes to the Claude provider).
--
-- Both columns are nullable: existing rows (and queue writes from older
-- frontends) will leave them null, and the drain logic falls back to the
-- session/card/project model precedence chain. Idempotent ADD COLUMN is
-- impossible in SQLite — the matching defensive check lives in
-- src/db/repair.rs::ensure_schema().
ALTER TABLE queued_messages ADD COLUMN model TEXT;
ALTER TABLE queued_messages ADD COLUMN effort TEXT;

-- Both columns are nullable and unused when empty, so leaving them on a
-- downgrade is harmless; we drop them for a clean down path.
ALTER TABLE sessions DROP COLUMN pending_handover_doc;
ALTER TABLE sessions DROP COLUMN handover_to_model;

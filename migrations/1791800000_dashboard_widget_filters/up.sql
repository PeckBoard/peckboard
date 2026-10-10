-- Per-widget filter state (JSON object text): free-form UI state the app
-- never queries, validated by the server on write. Nullable — every
-- existing row and every kind without filters stays NULL.
-- `src/db/repair.rs::ensure_schema()` re-adds the column if missing.
ALTER TABLE dashboard_widgets ADD COLUMN filters TEXT NULL;

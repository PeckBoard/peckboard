-- Local-only rollback. DROP COLUMN needs SQLite 3.35+; guarded so an
-- older shell doesn't choke the whole down migration.
DROP INDEX IF EXISTS idx_usage_events_account;
ALTER TABLE usage_events DROP COLUMN account_id;
DROP TABLE IF EXISTS claude_accounts;

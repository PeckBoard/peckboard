-- Local-only rollback. DROP COLUMN needs SQLite 3.35+.
ALTER TABLE claude_accounts DROP COLUMN refresh_token;
ALTER TABLE claude_accounts DROP COLUMN token_expires_at;

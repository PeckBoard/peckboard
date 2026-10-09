DROP INDEX IF EXISTS idx_card_sessions_session;
DROP INDEX IF EXISTS idx_card_sessions_card;
DROP TABLE IF EXISTS card_sessions;
ALTER TABLE cards DROP COLUMN reviewed_at;
ALTER TABLE cards DROP COLUMN review_verdict;
ALTER TABLE cards DROP COLUMN review_summary;
ALTER TABLE sessions DROP COLUMN sealed_reason;
ALTER TABLE sessions DROP COLUMN sealed_at;

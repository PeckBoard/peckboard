-- Short-lived OAuth support for browser-login (`oauth_token`) accounts.
-- Anthropic's token endpoint now issues ~8h access tokens for the
-- `user:inference user:profile` scope set (the profile scope is required
-- by the plan-usage endpoint), instead of the ~1-year inference-only
-- setup token the table was built around. Store the refresh token and
-- the access token's expiry so the app can renew the credential before
-- it lapses.
--
-- Both nullable: `api_key` accounts and legacy long-lived setup tokens
-- carry NULL (no refresh needed / possible).
ALTER TABLE claude_accounts ADD COLUMN refresh_token TEXT;
ALTER TABLE claude_accounts ADD COLUMN token_expires_at BIGINT;

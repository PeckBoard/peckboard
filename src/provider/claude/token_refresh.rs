//! Keeps short-lived `oauth_token` account credentials fresh.
//!
//! Browser logins now mint ~8h access tokens plus a refresh token (see
//! [`super::oauth`]). Every consumer of an account credential — session
//! spawn env injection and the plan-usage poller — goes through
//! [`fresh_credential`], which transparently renews and persists the
//! token when it is about to lapse. Legacy long-lived setup tokens (no
//! refresh token / no expiry on the row) pass through untouched.
//!
//! Known limitation: the token is injected into the spawned CLI's env at
//! process start, so a single claude process that lives past the token's
//! expiry keeps the stale value until its next respawn. Refreshes renew
//! the DB row, not a running process.

use std::time::Duration;

use crate::db::Db;
use crate::db::models::{ClaudeAccount, ClaudeAccountChanges};

use super::oauth;

/// Renew when the stored token expires within this window, so a token
/// handed to a spawning session doesn't lapse moments later.
const REFRESH_MARGIN_MS: i64 = 30 * 60 * 1000;

/// Serializes refreshes so concurrent consumers (plan poller + session
/// spawn) don't race the same refresh token — the endpoint may rotate it,
/// invalidating the loser's copy.
static REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Whether a row's credential needs renewing before use.
fn needs_refresh(account: &ClaudeAccount, now: i64) -> bool {
    account
        .refresh_token
        .as_deref()
        .is_some_and(|r| !r.is_empty())
        && account
            .token_expires_at
            .is_some_and(|at| at - now < REFRESH_MARGIN_MS)
}

/// A valid access token for an `oauth_token` account: the stored one when
/// it is still fresh (or long-lived), else a renewed one that has been
/// persisted back to the account row first.
pub async fn fresh_credential(db: &Db, account: &ClaudeAccount) -> anyhow::Result<String> {
    if !needs_refresh(account, now_ms()) {
        return Ok(account.credential.clone());
    }
    let _guard = REFRESH_LOCK.lock().await;
    // Re-read under the lock — a concurrent caller may have already
    // refreshed (and rotated the refresh token) while we waited.
    let account = db
        .get_claude_account(&account.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("claude account not found: {}", account.id))?;
    if !needs_refresh(&account, now_ms()) {
        return Ok(account.credential.clone());
    }
    let refresh_token = account
        .refresh_token
        .clone()
        .expect("needs_refresh implies refresh_token");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default();
    let minted = oauth::refresh(&client, &refresh_token).await.map_err(|e| {
        anyhow::anyhow!(
            "token refresh for account '{}' failed: {e} — redo the browser login on the account",
            account.name
        )
    })?;

    let changes = ClaudeAccountChanges {
        credential: Some(minted.access_token.clone()),
        // Keep the old refresh token when the endpoint didn't rotate it.
        refresh_token: Some(Some(minted.refresh_token.unwrap_or(refresh_token))),
        token_expires_at: Some(minted.expires_at_ms),
        updated_at: Some(now_ms()),
        ..Default::default()
    };
    db.update_claude_account(&account.id, changes).await?;
    tracing::info!(account = %account.name, "claude: refreshed short-lived oauth token");
    Ok(minted.access_token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(refresh_token: Option<&str>, token_expires_at: Option<i64>) -> ClaudeAccount {
        ClaudeAccount {
            id: "acc_t".into(),
            name: "T".into(),
            kind: "oauth_token".into(),
            credential: "tok".into(),
            config_dir: None,
            budget_window_hours: None,
            budget_limit_usd: None,
            budget_limit_tokens: None,
            warn_threshold: 0.75,
            critical_threshold: 0.90,
            created_at: 0,
            updated_at: 0,
            refresh_token: refresh_token.map(str::to_string),
            token_expires_at,
        }
    }

    #[test]
    fn refresh_is_gated_on_a_refresh_token_and_a_near_expiry() {
        let now = 1_000_000_000;
        // Legacy long-lived setup token: no refresh material — never.
        assert!(!needs_refresh(&account(None, None), now));
        // Expiring soon but nothing to refresh with — never (the guard at
        // exchange time prevents this shape from being stored anyway).
        assert!(!needs_refresh(&account(None, Some(now + 1)), now));
        // Fresh for hours — not yet.
        assert!(!needs_refresh(
            &account(Some("ref"), Some(now + 8 * 3_600_000)),
            now
        ));
        // Inside the margin, or already expired — refresh.
        assert!(needs_refresh(
            &account(Some("ref"), Some(now + REFRESH_MARGIN_MS - 1)),
            now
        ));
        assert!(needs_refresh(&account(Some("ref"), Some(now - 1)), now));
    }
}

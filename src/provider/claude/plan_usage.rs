//! Subscription plan-usage poller — the programmatic equivalent of the
//! `claude /usage` screen.
//!
//! The interactive CLI's `/usage` view is powered by an undocumented
//! endpoint, `GET https://api.anthropic.com/api/oauth/usage`, authenticated
//! with the login's OAuth bearer token. This module calls the same endpoint
//! for the host's default login (`$CLAUDE_CONFIG_DIR|~/.claude` →
//! `.credentials.json`) and for every stored `oauth_token` account, caches
//! the buckets in-process, and refreshes them on a fixed interval. The
//! Settings → Claude Accounts page reads the cache via
//! `GET /api/claude-accounts/plan-usage`.
//!
//! `api_key` accounts are skipped — pay-as-you-go keys have no plan buckets.
//! The endpoint requires the `user:profile` scope. Accounts logged in
//! before the login flow requested that scope get a 403; the cache entry
//! then carries a re-login hint instead of buckets.
//! The endpoint is unofficial and rate-limited upstream, so the cadence is
//! deliberately lazy and a failed refresh keeps the last good snapshot.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::state::AppState;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// Refresh cadence for every login's plan usage.
const POLL_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Cache/response key for the host's own login (the implicit "Default"
/// account, which has no row in `claude_accounts`).
pub const DEFAULT_KEY: &str = "default";

/// One usage bucket as the endpoint reports it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanBucket {
    /// Percent of the bucket's enforced limit consumed, 0–100.
    pub utilization: f64,
    /// When the bucket resets (ISO 8601); absent for untouched buckets.
    pub resets_at: Option<String>,
}

/// The stable buckets `/usage` shows. Experimental buckets the endpoint
/// also returns (promotional pools etc.) are ignored.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlanUsage {
    /// The rolling 5-hour session block.
    pub five_hour: Option<PlanBucket>,
    /// All-models weekly quota.
    pub seven_day: Option<PlanBucket>,
    /// Sonnet-specific weekly sub-quota.
    pub seven_day_sonnet: Option<PlanBucket>,
    /// Opus-specific weekly sub-quota.
    pub seven_day_opus: Option<PlanBucket>,
}

/// Cached fetch state for one login: the last good snapshot plus the error
/// of the most recent attempt when it failed. Both can be set at once —
/// stale-but-present data with a fresh error means "showing old numbers".
#[derive(Debug, Clone, Serialize, Default)]
pub struct PlanUsageEntry {
    pub usage: Option<PlanUsage>,
    /// ms epoch of the last successful fetch.
    pub fetched_at: Option<i64>,
    pub last_error: Option<String>,
}

/// Per-login plan usage keyed by `DEFAULT_KEY` or the account id.
/// Process-ephemeral diagnostics (repopulated on the first tick after
/// boot), so it lives here rather than the DB.
static CACHE: LazyLock<Mutex<HashMap<String, PlanUsageEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Current cache contents, for the HTTP layer.
pub fn snapshot() -> HashMap<String, PlanUsageEntry> {
    CACHE.lock().map(|c| c.clone()).unwrap_or_default()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn record_ok(key: &str, usage: PlanUsage) {
    if let Ok(mut cache) = CACHE.lock() {
        let entry = cache.entry(key.to_string()).or_default();
        entry.usage = Some(usage);
        entry.fetched_at = Some(now_ms());
        entry.last_error = None;
    }
}

fn record_err(key: &str, err: impl std::fmt::Display) {
    if let Ok(mut cache) = CACHE.lock() {
        let entry = cache.entry(key.to_string()).or_default();
        entry.last_error = Some(err.to_string());
    }
}

/// The host default login's OAuth access token, read the way the CLI
/// stores it on Linux/Windows: `$CLAUDE_CONFIG_DIR` (or `~/.claude`) +
/// `.credentials.json`, token at `claudeAiOauth.accessToken` (older
/// installs keep the fields top-level). On macOS the CLI uses the
/// keychain instead, so this returns `None` there.
fn host_oauth_token() -> Option<String> {
    let dir = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude")))?;
    let raw = std::fs::read_to_string(dir.join(".credentials.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("claudeAiOauth")
        .and_then(|o| o.get("accessToken"))
        .or_else(|| v.get("accessToken"))
        .and_then(|t| t.as_str())
        .map(str::to_string)
}

/// Fetch one login's plan usage from the OAuth usage endpoint.
async fn fetch(client: &reqwest::Client, token: &str) -> anyhow::Result<PlanUsage> {
    let resp = client
        .get(USAGE_URL)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header("anthropic-version", "2023-06-01")
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        // Bound the echoed body — enough to see "rate limited" / "scope"
        // style messages without dumping an HTML error page into the cache.
        let brief: String = body.chars().take(200).collect();
        anyhow::bail!("usage fetch failed ({status}): {brief}");
    }
    let parsed: PlanUsage = serde_json::from_str(&body)
        .map_err(|e| anyhow::anyhow!("unexpected usage response: {e}"))?;
    Ok(parsed)
}

/// Refresh every login once: the host default plus each stored
/// `oauth_token` account. Failures are recorded per login and never
/// abort the cycle.
pub async fn refresh_once(db: &Db) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default();

    match host_oauth_token() {
        Some(token) => match fetch(&client, &token).await {
            Ok(usage) => record_ok(DEFAULT_KEY, usage),
            Err(e) => {
                tracing::warn!("plan-usage: default login fetch failed: {e}");
                record_err(DEFAULT_KEY, e);
            }
        },
        None => record_err(
            DEFAULT_KEY,
            "no host Claude login found (~/.claude/.credentials.json)",
        ),
    }

    let accounts = match db.list_claude_accounts().await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("plan-usage: failed to list claude accounts: {e}");
            return;
        }
    };
    for acct in accounts.iter().filter(|a| a.kind == "oauth_token") {
        match fetch(&client, &acct.credential).await {
            Ok(usage) => record_ok(&acct.id, usage),
            Err(e) => {
                tracing::warn!(account = %acct.name, "plan-usage: fetch failed: {e}");
                // The scope 403 is a permanent property of the stored token,
                // not a transient failure — surface the fix instead.
                let msg = e.to_string();
                if msg.contains("user:profile") {
                    record_err(
                        &acct.id,
                        "token lacks the user:profile scope — edit the account and redo \
                         the browser login to enable plan usage",
                    );
                } else {
                    record_err(&acct.id, msg);
                }
            }
        }
    }
}

/// Spawn the poller: refresh immediately at boot, then every
/// [`POLL_INTERVAL`].
pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            // The first tick fires immediately, so the settings page has
            // data shortly after boot rather than 30 minutes in.
            interval.tick().await;
            refresh_once(&state.db).await;
        }
    });
    tracing::info!(
        "Claude plan-usage poller started ({}min interval)",
        POLL_INTERVAL.as_secs() / 60
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_usage_parses_the_documented_response_shape() {
        let body = r#"{
            "five_hour": {"utilization": 12.0, "resets_at": "2026-07-05T19:40:00+00:00"},
            "seven_day": {"utilization": 2.0, "resets_at": "2026-07-12T16:00:01+00:00"},
            "seven_day_sonnet": {"utilization": 0.0, "resets_at": null},
            "seven_day_opus": null,
            "seven_day_oauth_apps": null,
            "tangelo": null,
            "extra_usage": {"is_enabled": false}
        }"#;
        let parsed: PlanUsage = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.five_hour.as_ref().unwrap().utilization, 12.0);
        assert_eq!(parsed.seven_day.as_ref().unwrap().utilization, 2.0);
        let sonnet = parsed.seven_day_sonnet.as_ref().unwrap();
        assert_eq!(sonnet.utilization, 0.0);
        assert!(sonnet.resets_at.is_none());
        assert!(parsed.seven_day_opus.is_none());
    }

    #[test]
    fn cache_keeps_last_good_snapshot_across_a_failure() {
        record_ok(
            "t-acct",
            PlanUsage {
                five_hour: Some(PlanBucket {
                    utilization: 50.0,
                    resets_at: None,
                }),
                ..Default::default()
            },
        );
        record_err("t-acct", "boom");
        let snap = snapshot();
        let entry = snap.get("t-acct").unwrap();
        assert_eq!(
            entry
                .usage
                .as_ref()
                .unwrap()
                .five_hour
                .as_ref()
                .unwrap()
                .utilization,
            50.0
        );
        assert_eq!(entry.last_error.as_deref(), Some("boom"));
        assert!(entry.fetched_at.is_some());
    }
}

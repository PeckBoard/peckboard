//! Which provider accounts are out of quota, and until when.
//!
//! A run that crashes with [`CrashKind::RateLimit`] ("You've hit your usage
//! limit … try again at Oct 18th, 2026 1:59 AM") marks its provider account
//! limited. Until the reset passes, the worker orchestrator does not
//! dispatch to that account: card reviews pick another pool model, and a
//! worker pinned to the account waits instead of spawning a run that dies
//! in a second. Without this, every review hashed onto an exhausted account
//! crashed twice and blocked its card.
//!
//! In memory only: a restart forgets the limits, and the first run after
//! it just crashes once and records the limit again.
//!
//! [`CrashKind::RateLimit`]: crate::provider::stream::CrashKind::RateLimit

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Duration, Local, NaiveDateTime, TimeZone, Utc};

use crate::provider::registry::{ProviderRegistry, split_model_account};

/// Hold when the error names no reset time. Short: a bare 429 or
/// "overloaded" usually clears in minutes, and a still-exhausted quota
/// just crashes once more and records itself again.
const FALLBACK_HOLD_MINUTES: i64 = 15;
/// Never hold an account longer than this, whatever the error claims.
const MAX_HOLD_HOURS: i64 = 24;

#[derive(Default)]
pub struct RateLimits {
    until: Mutex<HashMap<String, DateTime<Utc>>>,
}

/// `provider@account` for a stored model id. A bare id (no provider
/// prefix) belongs to `claude`, as everywhere else; no account means the
/// provider's default login.
fn account_key(model_id: Option<&str>) -> String {
    let id = model_id.unwrap_or("");
    let (provider, rest) = ProviderRegistry::parse_model_id(id, "claude");
    let account = split_model_account(&rest).1.unwrap_or("");
    format!("{provider}@{account}")
}

impl RateLimits {
    /// Record that `model_id`'s account hit a limit, with the reset time
    /// read from `reason`. Returns when the hold ends.
    pub fn note(&self, model_id: Option<&str>, reason: &str, now: DateTime<Utc>) -> DateTime<Utc> {
        let until = reset_at(reason, now);
        let key = account_key(model_id);
        tracing::warn!(account = %key, until = %until, "Provider account rate-limited; holding dispatch");
        self.until.lock().unwrap().insert(key, until);
        until
    }

    /// When `model_id`'s account becomes usable again, or `None` if it is
    /// not limited at `now`.
    pub fn limited_until(
        &self,
        model_id: Option<&str>,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let key = account_key(model_id);
        let mut map = self.until.lock().unwrap();
        match map.get(&key) {
            Some(until) if *until > now => Some(*until),
            Some(_) => {
                map.remove(&key);
                None
            }
            None => None,
        }
    }

    pub fn is_limited(&self, model_id: Option<&str>, now: DateTime<Utc>) -> bool {
        self.limited_until(model_id, now).is_some()
    }
}

/// The reset time a rate-limit error names, clamped to
/// `(now, now + MAX_HOLD_HOURS]`; [`FALLBACK_HOLD_MINUTES`] from `now` when
/// it names none. Understands Codex's "try again at Oct 18th, 2026 1:59 AM"
/// (machine-local time) and relative "resets in 4m" / "try again in 2 hours".
pub fn reset_at(reason: &str, now: DateTime<Utc>) -> DateTime<Utc> {
    let parsed = absolute_reset(reason).or_else(|| relative_reset(reason, now));
    match parsed {
        Some(t) if t > now => t.min(now + Duration::hours(MAX_HOLD_HOURS)),
        _ => now + Duration::minutes(FALLBACK_HOLD_MINUTES),
    }
}

fn absolute_reset(reason: &str) -> Option<DateTime<Utc>> {
    let re = regex::Regex::new(
        r"(?i)try again at ([A-Z][a-z]{2}) (\d{1,2})(?:st|nd|rd|th)?,? (\d{4}),? (\d{1,2}):(\d{2}) ?([AP]M)",
    )
    .ok()?;
    let c = re.captures(reason)?;
    let text = format!(
        "{} {} {} {}:{} {}",
        &c[1],
        &c[2],
        &c[3],
        &c[4],
        &c[5],
        c[6].to_uppercase()
    );
    let naive = NaiveDateTime::parse_from_str(&text, "%b %d %Y %I:%M %p").ok()?;
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|t| t.with_timezone(&Utc))
}

fn relative_reset(reason: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let re = regex::Regex::new(
        r"(?i)(?:resets?|try again) in (\d+)\s*(s|sec|secs|seconds?|m|min|mins|minutes?|h|hr|hrs|hours?)\b",
    )
    .ok()?;
    let c = re.captures(reason)?;
    let n: i64 = c[1].parse().ok()?;
    let unit = c[2].to_ascii_lowercase();
    let d = if unit.starts_with('s') {
        Duration::seconds(n)
    } else if unit.starts_with('h') {
        Duration::hours(n)
    } else {
        Duration::minutes(n)
    };
    Some(now + d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    #[test]
    fn codex_usage_limit_reset_is_parsed_and_scoped_to_the_account() {
        let now = now();
        let target = (Local::now() + Duration::hours(3)).naive_local();
        let reason = format!(
            "You've hit your usage limit. Upgrade to Pro, or try again at {}.",
            target.format("%b %-dth, %Y %-I:%M %p")
        );
        let limits = RateLimits::default();
        let until = limits.note(Some("codex:gpt-5.6-sol@cacc_1"), &reason, now);
        let delta = until - now;
        assert!(
            delta > Duration::minutes(170) && delta <= Duration::minutes(181),
            "reset ~3h out, got {delta}"
        );
        // Same account, any model: limited. Other account / provider: not.
        assert!(limits.is_limited(Some("codex:gpt-5.6@cacc_1"), now));
        assert!(!limits.is_limited(Some("codex:gpt-5.6-sol@cacc_2"), now));
        assert!(!limits.is_limited(Some("claude:claude-sonnet-5-5@cacc_1"), now));
        // The hold lifts once the reset passes.
        assert!(!limits.is_limited(Some("codex:gpt-5.6@cacc_1"), until + Duration::seconds(1)));
    }

    #[test]
    fn relative_unknown_and_absurd_resets_are_bounded() {
        let now = now();
        assert_eq!(
            reset_at("rate limit resets in 4m", now),
            now + Duration::minutes(4)
        );
        assert_eq!(
            reset_at("API Error: 429 too many requests", now),
            now + Duration::minutes(FALLBACK_HOLD_MINUTES)
        );
        assert_eq!(
            reset_at("usage limit, try again in 500 hours", now),
            now + Duration::hours(MAX_HOLD_HOURS)
        );
    }

    #[test]
    fn bare_and_default_ids_share_the_default_claude_login() {
        let now = now();
        let limits = RateLimits::default();
        limits.note(None, "usage limit", now);
        assert!(limits.is_limited(Some("default"), now));
        assert!(limits.is_limited(Some("claude:claude-opus-5-5"), now));
        assert!(!limits.is_limited(Some("claude:claude-opus-5-5@acc_x"), now));
    }
}

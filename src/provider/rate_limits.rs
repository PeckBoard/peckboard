//! Which provider accounts are out of quota, and until when.
//!
//! A turn that ends with [`CrashKind::RateLimit`] ("You've hit your usage
//! limit … try again at Oct 18th, 2026 1:59 AM", or Claude's "You've hit
//! your weekly limit · resets 8am (UTC)") marks its provider account
//! limited. Until the reset passes, nothing is dispatched to that account:
//! card reviews pick another pool model, a worker pinned to the account
//! waits instead of spawning a run that dies in a second, and a chat turn
//! is parked and re-sent at the reset (`provider::limit_recovery`).
//!
//! A hold is scoped to the account. For Claude it is scoped to the model on
//! that account as well: its weekly caps are per model family, so Opus
//! running out must not stop Fable on the same login. The cost is one
//! one-second failed turn per model to learn each hold.
//!
//! Holds are stored in `provider_limits` and reloaded at boot, so a restart
//! neither forgets them nor burns a doomed turn per waiting card.
//!
//! [`CrashKind::RateLimit`]: crate::provider::stream::CrashKind::RateLimit

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use diesel::prelude::*;

use crate::db::Db;
use crate::provider::registry::{ProviderRegistry, split_model_account};
use crate::provider::stream::CrashKind;

/// Hold when the error names no reset time. Short: a bare 429 or
/// "overloaded" usually clears in minutes, and a still-exhausted quota
/// just fails once more and records itself again.
const FALLBACK_HOLD_MINUTES: i64 = 15;
/// Never hold longer than this, whatever the error claims. A weekly cap
/// can name a reset up to a week out.
const MAX_HOLD_DAYS: i64 = 8;

/// One account (or account + model) that is out of quota.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Hold {
    /// The model id the limit was hit on, as stored on the session.
    pub model: String,
    pub until: DateTime<Utc>,
    /// The provider's own words.
    pub reason: String,
}

#[derive(Default)]
pub struct RateLimits {
    holds: Mutex<HashMap<String, Hold>>,
    /// Set once at boot by [`RateLimits::load`]; `None` in unit tests,
    /// where holds live in memory only.
    db: Mutex<Option<Db>>,
}

/// `(provider, account, model name)` of a stored model id. A bare id (no
/// provider prefix) belongs to `claude`, as everywhere else; no account
/// means the provider's default login.
fn parts(model_id: Option<&str>) -> (String, String, String) {
    let id = model_id.unwrap_or("");
    let (provider, rest) = ProviderRegistry::parse_model_id(id, "claude");
    let (name, account) = split_model_account(&rest);
    (
        provider.to_string(),
        account.unwrap_or("").to_string(),
        name.to_string(),
    )
}

/// The whole-account key, which holds every model on the login.
fn account_key(model_id: Option<&str>) -> String {
    let (provider, account, _) = parts(model_id);
    format!("{provider}@{account}")
}

/// The key a new hold is recorded under: account + model for Claude when
/// the id names a model, the whole account otherwise.
fn hold_key(model_id: Option<&str>) -> String {
    let (provider, account, name) = parts(model_id);
    if provider == "claude" && !name.is_empty() && name != "default" {
        format!("{provider}@{account}/{name}")
    } else {
        format!("{provider}@{account}")
    }
}

#[derive(QueryableByName)]
struct HoldRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    key: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    model: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    limited_until: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    reason: String,
}

impl RateLimits {
    /// Boot: remember `db` for later writes and load the holds that are
    /// still in force. Expired rows are deleted.
    pub async fn load(&self, db: &Db) {
        *self.db.lock().unwrap() = Some(db.clone());
        let now = Utc::now();
        let cutoff = now.to_rfc3339();
        let rows = db
            .with_conn(move |conn| {
                diesel::sql_query("DELETE FROM provider_limits WHERE limited_until <= ?1")
                    .bind::<diesel::sql_types::Text, _>(cutoff)
                    .execute(conn)?;
                let rows: Vec<HoldRow> = diesel::sql_query(
                    "SELECT key, model, limited_until, reason FROM provider_limits",
                )
                .load(conn)?;
                Ok(rows)
            })
            .await;
        let rows = match rows {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("Could not load provider limit holds: {e:#}");
                return;
            }
        };
        let mut holds = self.holds.lock().unwrap();
        for row in rows {
            let Ok(until) = DateTime::parse_from_rfc3339(&row.limited_until) else {
                continue;
            };
            let until = until.with_timezone(&Utc);
            if until > now {
                tracing::info!(account = %row.key, until = %until, "Provider limit hold still in force");
                holds.insert(
                    row.key,
                    Hold {
                        model: row.model,
                        until,
                        reason: row.reason,
                    },
                );
            }
        }
    }

    /// Write (`Some`) or delete (`None`) one stored hold, off the caller's
    /// thread. A lost write only costs one failed turn after a restart.
    fn persist(&self, key: String, hold: Option<Hold>) {
        let Some(db) = self.db.lock().unwrap().clone() else {
            return;
        };
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        rt.spawn(async move {
            let res = db
                .with_conn(move |conn| {
                    match hold {
                        Some(h) => diesel::sql_query(
                            "INSERT OR REPLACE INTO provider_limits \
                             (key, model, limited_until, reason, noted_at) \
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                        )
                        .bind::<diesel::sql_types::Text, _>(key)
                        .bind::<diesel::sql_types::Text, _>(h.model)
                        .bind::<diesel::sql_types::Text, _>(h.until.to_rfc3339())
                        .bind::<diesel::sql_types::Text, _>(h.reason)
                        .bind::<diesel::sql_types::Text, _>(Utc::now().to_rfc3339())
                        .execute(conn)?,
                        None => diesel::sql_query("DELETE FROM provider_limits WHERE key = ?1")
                            .bind::<diesel::sql_types::Text, _>(key)
                            .execute(conn)?,
                    };
                    Ok(())
                })
                .await;
            if let Err(e) = res {
                tracing::warn!("Could not store a provider limit hold: {e:#}");
            }
        });
    }

    /// Record that `model_id` hit a limit, with the reset time read from
    /// `reason`. Returns when the hold ends.
    pub fn note(&self, model_id: Option<&str>, reason: &str, now: DateTime<Utc>) -> DateTime<Utc> {
        let until = reset_at(reason, now);
        let key = hold_key(model_id);
        tracing::warn!(account = %key, until = %until, "Provider account rate-limited; holding dispatch");
        let hold = Hold {
            model: model_id.unwrap_or("").to_string(),
            until,
            reason: reason.trim().chars().take(300).collect(),
        };
        self.holds.lock().unwrap().insert(key.clone(), hold.clone());
        self.persist(key, Some(hold));
        until
    }

    /// The hold that stops `model_id` at `now`, if any: one on the model
    /// itself or one on its whole account.
    pub fn hold_for(&self, model_id: Option<&str>, now: DateTime<Utc>) -> Option<Hold> {
        let mut expired = Vec::new();
        let mut found = None;
        {
            let mut holds = self.holds.lock().unwrap();
            for key in [hold_key(model_id), account_key(model_id)] {
                match holds.get(&key) {
                    Some(hold) if hold.until > now => {
                        found.get_or_insert_with(|| hold.clone());
                    }
                    Some(_) => {
                        holds.remove(&key);
                        expired.push(key);
                    }
                    None => {}
                }
            }
        }
        for key in expired {
            self.persist(key, None);
        }
        found
    }

    /// When `model_id` becomes usable again, or `None` if it is not
    /// limited at `now`.
    pub fn limited_until(
        &self,
        model_id: Option<&str>,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        self.hold_for(model_id, now).map(|h| h.until)
    }

    pub fn is_limited(&self, model_id: Option<&str>, now: DateTime<Utc>) -> bool {
        self.limited_until(model_id, now).is_some()
    }

    /// Every hold in force at `now`, soonest reset first.
    pub fn active(&self, now: DateTime<Utc>) -> Vec<Hold> {
        let mut holds: Vec<Hold> = self
            .holds
            .lock()
            .unwrap()
            .values()
            .filter(|h| h.until > now)
            .cloned()
            .collect();
        holds.sort_by(|a, b| a.until.cmp(&b.until).then(a.model.cmp(&b.model)));
        holds
    }

    /// Drop the holds that stop `model_id` (the user's "Retry now"), or
    /// every hold when `model_id` is `None`. Returns how many went.
    pub fn clear(&self, model_id: Option<&str>) -> usize {
        let removed: Vec<String> = {
            let mut holds = self.holds.lock().unwrap();
            let keys: Vec<String> = match model_id {
                Some(_) => [hold_key(model_id), account_key(model_id)]
                    .into_iter()
                    .filter(|k| holds.contains_key(k))
                    .collect(),
                None => holds.keys().cloned().collect(),
            };
            for key in &keys {
                holds.remove(key);
            }
            keys
        };
        let n = removed.len();
        for key in removed {
            self.persist(key, None);
        }
        n
    }
}

/// A card the orchestrator is holding back because its model is out of
/// quota. What the board shows as "waiting for the limit to reset".
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CardWait {
    pub card_id: String,
    pub model: String,
    pub until: DateTime<Utc>,
    pub reason: String,
}

struct WaitEntry {
    project_id: String,
    wait: CardWait,
    seen: DateTime<Utc>,
}

/// (database, card id) -> the card's current wait. Process-wide, like the
/// orchestrator's budget flag: the project JSON builder has no `AppState`
/// to reach the registry through. Keyed by [`Db::instance_key`] as well, so
/// two servers in one process (tests) never see each other's cards.
/// Refreshed by every orchestrator tick that defers the card, so an entry
/// not seen for [`WAIT_STALE_SECS`] is a card that has since moved on
/// (done, re-modelled, deleted) and is ignored.
static WAITS: std::sync::LazyLock<Mutex<HashMap<(usize, String), WaitEntry>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// The orchestrator ticks every 5 s; six missed ticks means gone.
const WAIT_STALE_SECS: i64 = 30;

/// Record that `card_id` is being held back by `hold`. Returns true when
/// this is news (the card was not already known to be waiting until then),
/// so the caller can log and broadcast once instead of every tick.
pub fn note_card_wait(
    db: &Db,
    card_id: &str,
    project_id: &str,
    hold: &Hold,
    now: DateTime<Utc>,
) -> bool {
    let key = (db.instance_key(), card_id.to_string());
    let mut waits = WAITS.lock().unwrap_or_else(|e| e.into_inner());
    let fresh = waits.get(&key).is_none_or(|e| {
        e.wait.until != hold.until || (now - e.seen).num_seconds() > WAIT_STALE_SECS
    });
    waits.insert(
        key,
        WaitEntry {
            project_id: project_id.to_string(),
            wait: CardWait {
                card_id: card_id.to_string(),
                model: hold.model.clone(),
                until: hold.until,
                reason: hold.reason.clone(),
            },
            seen: now,
        },
    );
    fresh
}

/// The card is being dispatched (or no longer exists): it waits no more.
/// Returns true when it had been waiting.
pub fn clear_card_wait(db: &Db, card_id: &str) -> bool {
    WAITS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(db.instance_key(), card_id.to_string()))
        .is_some()
}

/// The cards of `project_id` waiting on a limit at `now`, soonest reset
/// first.
pub fn card_waits(db: &Db, project_id: &str, now: DateTime<Utc>) -> Vec<CardWait> {
    let db_key = db.instance_key();
    let mut out: Vec<CardWait> = WAITS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|((k, _), e)| {
            *k == db_key
                && e.project_id == project_id
                && e.wait.until > now
                && (now - e.seen).num_seconds() <= WAIT_STALE_SECS
        })
        .map(|(_, e)| e.wait.clone())
        .collect();
    out.sort_by(|a, b| a.until.cmp(&b.until).then(a.card_id.cmp(&b.card_id)));
    out
}

/// Whether `card_id` is waiting on a limit at `now`, and until when.
pub fn card_wait(db: &Db, card_id: &str, now: DateTime<Utc>) -> Option<CardWait> {
    WAITS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(db.instance_key(), card_id.to_string()))
        .filter(|e| e.wait.until > now && (now - e.seen).num_seconds() <= WAIT_STALE_SECS)
        .map(|e| e.wait.clone())
}

/// Whether `text` is a provider's usage-limit notice rather than a reply:
/// one short line that opens with the limit wording ("You've hit your
/// weekly limit · resets 8am (UTC)", "5-hour limit reached ∙ resets 3pm",
/// "You're out of extra usage"). Anchored and short on purpose — an answer
/// that merely quotes such a line, or discusses limits, must not match.
pub fn is_limit_stop(text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() || text.len() > 240 || text.contains('\n') {
        return false;
    }
    if CrashKind::classify(text) != CrashKind::RateLimit {
        return false;
    }
    static OPENING: std::sync::LazyLock<Option<regex::Regex>> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)^(you['’]ve hit your\b.{0,40}\blimit|you have hit your\b.{0,40}\blimit|.{0,30}\blimit reached|you['’]re out of extra usage|out of extra usage)",
        )
        .ok()
    });
    OPENING.as_ref().is_some_and(|re| re.is_match(text))
}

/// The clock a reset time is written in. Claude appends the zone it used
/// ("resets 8am (UTC)", "resets 3pm (Europe/Berlin)"); with none, the
/// machine's local time, which is what a CLI prints by default.
enum Zone {
    Local,
    Utc,
    Named(chrono_tz::Tz),
}

impl Zone {
    fn of(reason: &str) -> Zone {
        let named = regex::Regex::new(r"\(([A-Za-z_]+(?:/[A-Za-z0-9_+\-]+)*)\)")
            .ok()
            .and_then(|re| re.captures(reason).map(|c| c[1].to_string()));
        match named.as_deref() {
            Some("UTC") | Some("GMT") | Some("Z") => Zone::Utc,
            Some(name) => name
                .parse::<chrono_tz::Tz>()
                .map_or(Zone::Local, Zone::Named),
            None => Zone::Local,
        }
    }

    fn today(&self, now: DateTime<Utc>) -> NaiveDate {
        match self {
            Zone::Local => now.with_timezone(&Local).date_naive(),
            Zone::Utc => now.date_naive(),
            Zone::Named(tz) => now.with_timezone(tz).date_naive(),
        }
    }

    fn resolve(&self, naive: &NaiveDateTime) -> Option<DateTime<Utc>> {
        match self {
            Zone::Local => Local
                .from_local_datetime(naive)
                .earliest()
                .map(|t| t.with_timezone(&Utc)),
            Zone::Utc => Some(Utc.from_utc_datetime(naive)),
            Zone::Named(tz) => tz
                .from_local_datetime(naive)
                .earliest()
                .map(|t| t.with_timezone(&Utc)),
        }
    }
}

/// The reset time a rate-limit error names, clamped to
/// `(now, now + MAX_HOLD_DAYS]`; [`FALLBACK_HOLD_MINUTES`] from `now` when
/// it names none. Understands Codex's "try again at Oct 18th, 2026 1:59 AM"
/// and its same-day "try again at 1:59 AM" (a clock time already past
/// means tomorrow), relative "resets in 4m" / "try again in 2 hours", and
/// Claude's "resets 3pm" / "resets Oct 24, 9am", each in the zone the
/// message names (see [`Zone`]).
pub fn reset_at(reason: &str, now: DateTime<Utc>) -> DateTime<Utc> {
    let zone = Zone::of(reason);
    let parsed = absolute_reset(reason, &zone)
        .or_else(|| dated_reset(reason, now, &zone))
        .or_else(|| clock_reset(reason, now, &zone))
        .or_else(|| relative_reset(reason, now));
    match parsed {
        Some(t) if t > now => t.min(now + Duration::days(MAX_HOLD_DAYS)),
        _ => now + Duration::minutes(FALLBACK_HOLD_MINUTES),
    }
}

fn absolute_reset(reason: &str, zone: &Zone) -> Option<DateTime<Utc>> {
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
    zone.resolve(&naive)
}

/// Claude's "resets Oct 24, 9am": a month and day with no year, in the
/// current year (next year once that is past).
fn dated_reset(reason: &str, now: DateTime<Utc>, zone: &Zone) -> Option<DateTime<Utc>> {
    let re = regex::Regex::new(
        r"(?i)resets?(?: at| on)? ([A-Z][a-z]{2}) (\d{1,2}),?(?: at)? (\d{1,2})(?::(\d{2}))? ?([AP]M)",
    )
    .ok()?;
    let c = re.captures(reason)?;
    let minutes = c.get(4).map_or("00", |m| m.as_str());
    let year = zone.today(now).year();
    [year, year + 1].into_iter().find_map(|y| {
        let text = format!(
            "{} {} {y} {}:{minutes} {}",
            &c[1],
            &c[2],
            &c[3],
            c[5].to_uppercase()
        );
        let naive = NaiveDateTime::parse_from_str(&text, "%b %d %Y %I:%M %p").ok()?;
        zone.resolve(&naive).filter(|t| *t > now)
    })
}

/// "try again at 1:59 AM" / Claude's "resets 3pm" with no date: the next
/// occurrence of that clock time in the message's zone.
fn clock_reset(reason: &str, now: DateTime<Utc>, zone: &Zone) -> Option<DateTime<Utc>> {
    let re =
        regex::Regex::new(r"(?i)(?:try again at|resets?(?: at)?) (\d{1,2})(?::(\d{2}))? ?([AP]M)")
            .ok()?;
    let c = re.captures(reason)?;
    let minutes = c.get(2).map_or("00", |m| m.as_str());
    let text = format!("{}:{} {}", &c[1], minutes, c[3].to_uppercase());
    let time = chrono::NaiveTime::parse_from_str(&text, "%I:%M %p").ok()?;
    let today = zone.today(now);
    [today, today.succ_opt()?]
        .into_iter()
        .filter_map(|d| zone.resolve(&d.and_time(time)))
        .find(|t| *t > now)
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
            now + Duration::days(MAX_HOLD_DAYS)
        );
    }
    /// Codex drops the date when the reset is within a day.
    #[test]
    fn clock_only_reset_is_the_next_occurrence() {
        let now = now();
        let at = (Local::now() + Duration::hours(2)).format("%-I:%M %p");
        let delta = reset_at(&format!("usage limit ... or try again at {at}."), now) - now;
        assert!(
            delta > Duration::minutes(118) && delta <= Duration::minutes(121),
            "{delta}"
        );
        // A clock time already past rolls over to tomorrow.
        let past = (Local::now() - Duration::hours(1)).format("%-I:%M %p");
        let delta = reset_at(&format!("try again at {past}"), now) - now;
        assert!(
            delta > Duration::hours(22) && delta <= Duration::hours(23),
            "{delta}"
        );
    }
    /// Claude's subscription-cap wording: "resets 3pm" and the weekly
    /// "resets Oct 24, 9am" (no year).
    #[test]
    fn claude_resets_wording_is_parsed() {
        let now = now();
        let at = (Local::now() + Duration::hours(2)).format("%-I%P");
        let delta = reset_at(&format!("5-hour limit reached \u{2219} resets {at}"), now) - now;
        assert!(
            delta > Duration::minutes(59) && delta <= Duration::minutes(121),
            "{delta}"
        );
        let soon = (Local::now() + Duration::hours(5)).format("%b %-d, %-I:%M%P");
        let delta = reset_at(
            &format!("You've hit your weekly limit \u{00b7} resets {soon}"),
            now,
        ) - now;
        assert!(
            delta > Duration::minutes(298) && delta <= Duration::minutes(301),
            "{delta}"
        );
        let later = (Local::now() + Duration::days(3)).format("%b %-d, %-I%P");
        let delta = reset_at(
            &format!("weekly limit reached \u{00b7} resets {later}"),
            now,
        ) - now;
        assert!(
            delta > Duration::hours(70) && delta <= Duration::hours(73),
            "a reset days away is kept, not clamped to a day: {delta}"
        );
    }

    /// "resets 8am (UTC)" is 8am UTC whatever zone the server runs in, and
    /// a named zone is honoured too.
    #[test]
    fn a_reset_is_read_in_the_zone_the_message_names() {
        let now = Utc.with_ymd_and_hms(2026, 11, 28, 2, 30, 0).unwrap();
        assert_eq!(
            reset_at(
                "You've hit your weekly limit \u{00b7} resets 8am (UTC)",
                now
            ),
            Utc.with_ymd_and_hms(2026, 11, 28, 8, 0, 0).unwrap()
        );
        assert_eq!(
            reset_at(
                "You've hit your weekly limit \u{00b7} resets Dec 4, 12am (UTC)",
                now
            ),
            Utc.with_ymd_and_hms(2026, 12, 4, 0, 0, 0).unwrap()
        );
        // 3pm in Berlin (UTC+1 in winter) is 14:00 UTC.
        assert_eq!(
            reset_at(
                "5-hour limit reached \u{2219} resets 3pm (Europe/Berlin)",
                now
            ),
            Utc.with_ymd_and_hms(2026, 11, 28, 14, 0, 0).unwrap()
        );
    }

    /// Claude's caps are per model: Opus out of quota leaves Fable on the
    /// same login usable. "Retry now" drops the hold.
    #[test]
    fn a_claude_hold_is_scoped_to_the_model_and_can_be_cleared() {
        let now = now();
        let limits = RateLimits::default();
        limits.note(
            Some("claude:claude-opus-5-5@acc_1"),
            "You've hit your weekly limit \u{00b7} resets in 2 hours",
            now,
        );
        assert!(limits.is_limited(Some("claude:claude-opus-5-5@acc_1"), now));
        assert!(!limits.is_limited(Some("claude:fable@acc_1"), now));
        assert!(!limits.is_limited(Some("claude:claude-opus-5-5@acc_2"), now));
        assert_eq!(limits.active(now).len(), 1);
        assert_eq!(limits.clear(Some("claude:claude-opus-5-5@acc_1")), 1);
        assert!(!limits.is_limited(Some("claude:claude-opus-5-5@acc_1"), now));
    }

    #[test]
    fn only_a_short_notice_that_opens_with_the_limit_wording_is_a_limit_stop() {
        for notice in [
            "You've hit your weekly limit \u{00b7} resets 8am (UTC)",
            "You\u{2019}ve hit your limit \u{00b7} resets 3pm",
            "5-hour limit reached \u{2219} resets 3pm",
            "You've hit your usage limit. Upgrade to Pro, or try again at 1:59 AM.",
            "  You're out of extra usage \u{00b7} resets 9pm  ",
        ] {
            assert!(is_limit_stop(notice), "{notice}");
        }
        for reply in [
            "",
            "Done. All tests pass.",
            // Quoting the notice inside an answer is not a limit stop.
            "The worker ended with \"You've hit your weekly limit \u{00b7} resets 8am (UTC)\".",
            "The subagent crashed: You've hit your weekly limit.\nI restarted it on another account.",
            // Not a usage cap.
            "max turns limit reached",
            "I raised the rate limit to 100 requests per minute.",
        ] {
            assert!(!is_limit_stop(reply), "{reply}");
        }
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

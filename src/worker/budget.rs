//! Budget window helpers: compute the UTC start and reset date for a
//! daily / weekly / monthly spend window.

use chrono::{Datelike, Duration, TimeZone, Utc};

/// Return the UTC timestamp at which the current budget period started.
///
/// - `"daily"` → 00:00 UTC today.
/// - `"weekly"` → 00:00 UTC of the most recent Monday.
/// - `"monthly"` → 00:00 UTC of the 1st of the current month.
///
/// `period` is assumed to be one of the valid values; unrecognised values
/// fall back to the start of the current day.
pub fn budget_window_start(now: chrono::DateTime<Utc>, period: &str) -> chrono::DateTime<Utc> {
    match period {
        "weekly" => {
            // Walk back to the most recent Monday.
            let days_since_monday = now.weekday().num_days_from_monday();
            let monday = now.date_naive() - Duration::days(days_since_monday as i64);
            Utc.from_utc_datetime(&monday.and_hms_opt(0, 0, 0).unwrap())
        }
        "monthly" => {
            let first = chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1).unwrap();
            Utc.from_utc_datetime(&first.and_hms_opt(0, 0, 0).unwrap())
        }
        _ => {
            // "daily" or unknown
            Utc.from_utc_datetime(&now.date_naive().and_hms_opt(0, 0, 0).unwrap())
        }
    }
}

/// Return the UTC timestamp at which the NEXT budget period starts (i.e. when
/// the current window resets). Used for the pause-reason banner copy.
pub fn budget_window_reset(now: chrono::DateTime<Utc>, period: &str) -> chrono::DateTime<Utc> {
    match period {
        "weekly" => {
            let days_since_monday = now.weekday().num_days_from_monday();
            let next_monday =
                now.date_naive() - Duration::days(days_since_monday as i64) + Duration::days(7);
            Utc.from_utc_datetime(&next_monday.and_hms_opt(0, 0, 0).unwrap())
        }
        "monthly" => {
            let (year, month) = if now.month() == 12 {
                (now.year() + 1, 1)
            } else {
                (now.year(), now.month() + 1)
            };
            let first = chrono::NaiveDate::from_ymd_opt(year, month, 1).unwrap();
            Utc.from_utc_datetime(&first.and_hms_opt(0, 0, 0).unwrap())
        }
        _ => {
            // "daily"
            let tomorrow = now.date_naive() + Duration::days(1);
            Utc.from_utc_datetime(&tomorrow.and_hms_opt(0, 0, 0).unwrap())
        }
    }
}

// Suppress unused-import lint when Weekday is only needed for the trait.
#[allow(unused_imports)]
use chrono::Weekday as _;

#[cfg(test)]
fn utc(year: i32, month: u32, day: u32, h: u32, m: u32, s: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, h, m, s).unwrap()
}

#[test]
fn daily_start_is_midnight_today() {
    let now = utc(2026, 7, 13, 14, 30, 0); // Mon 2026-07-13 14:30 UTC
    let start = budget_window_start(now, "daily");
    assert_eq!(start, utc(2026, 7, 13, 0, 0, 0));
}

#[test]
fn daily_reset_is_midnight_tomorrow() {
    let now = utc(2026, 7, 13, 14, 30, 0);
    let reset = budget_window_reset(now, "daily");
    assert_eq!(reset, utc(2026, 7, 14, 0, 0, 0));
}

#[test]
fn weekly_start_is_last_monday() {
    // Wednesday 2026-07-15 → start should be Mon 2026-07-13
    let now = utc(2026, 7, 15, 10, 0, 0);
    let start = budget_window_start(now, "weekly");
    assert_eq!(start, utc(2026, 7, 13, 0, 0, 0));
}

#[test]
fn weekly_start_on_monday_is_today() {
    let now = utc(2026, 7, 13, 0, 0, 1); // Monday
    let start = budget_window_start(now, "weekly");
    assert_eq!(start, utc(2026, 7, 13, 0, 0, 0));
}

#[test]
fn weekly_reset_is_next_monday() {
    let now = utc(2026, 7, 15, 10, 0, 0); // Wednesday
    let reset = budget_window_reset(now, "weekly");
    assert_eq!(reset, utc(2026, 7, 20, 0, 0, 0));
}

#[test]
fn monthly_start_is_first_of_month() {
    let now = utc(2026, 7, 13, 12, 0, 0);
    let start = budget_window_start(now, "monthly");
    assert_eq!(start, utc(2026, 7, 1, 0, 0, 0));
}

#[test]
fn monthly_reset_is_first_of_next_month() {
    let now = utc(2026, 7, 13, 12, 0, 0);
    let reset = budget_window_reset(now, "monthly");
    assert_eq!(reset, utc(2026, 8, 1, 0, 0, 0));
}

#[test]
fn monthly_reset_wraps_december() {
    let now = utc(2026, 12, 15, 0, 0, 0);
    let reset = budget_window_reset(now, "monthly");
    assert_eq!(reset, utc(2027, 1, 1, 0, 0, 0));
}

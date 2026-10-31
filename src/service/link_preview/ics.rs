//! `.ics` / `text/calendar` links → an event card: the first `VEVENT`'s
//! title, start/end (UTC RFC 3339 when the zone is known, so the UI shows
//! it in the viewer's zone; a bare date for all-day events) and location.

use chrono::{NaiveDate, NaiveDateTime, TimeZone};
use serde_json::json;

use super::github::GhCard as Card;
use super::parse::clean;

fn unfold(ics: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in ics.split('\n') {
        let line = raw.trim_end_matches('\r');
        if (line.starts_with(' ') || line.starts_with('\t'))
            && let Some(last) = out.last_mut()
        {
            last.push_str(&line[1..]);
        } else {
            out.push(line.to_string());
        }
    }
    out
}

fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `(value, all_day, tzid)` → ISO string: `2026-11-01` (all-day),
/// `2026-11-01T09:00:00Z` (UTC / known zone), `2026-11-01T10:00:00`
/// (floating — shown as-is).
fn when(value: &str, params: &str) -> Option<(String, bool)> {
    let tzid = params
        .split(';')
        .find_map(|p| p.strip_prefix("TZID="))
        .map(|t| t.trim_matches('"'));
    let value = value.trim();
    if params.contains("VALUE=DATE") && !params.contains("VALUE=DATE-TIME") || value.len() == 8 {
        let d = NaiveDate::parse_from_str(value, "%Y%m%d").ok()?;
        return Some((d.format("%Y-%m-%d").to_string(), true));
    }
    if let Some(utc) = value.strip_suffix('Z') {
        let dt = NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S").ok()?;
        return Some((dt.format("%Y-%m-%dT%H:%M:%SZ").to_string(), false));
    }
    let dt = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok()?;
    if let Some(tz) = tzid.and_then(|t| t.parse::<chrono_tz::Tz>().ok())
        && let Some(local) = tz.from_local_datetime(&dt).earliest()
    {
        let utc = local.with_timezone(&chrono::Utc);
        return Some((utc.format("%Y-%m-%dT%H:%M:%SZ").to_string(), false));
    }
    Some((dt.format("%Y-%m-%dT%H:%M:%S").to_string(), false))
}

/// The event card for a calendar body, or `None` without a usable VEVENT.
pub(super) fn event_card(ics: &str) -> Option<Card> {
    let lines = unfold(ics);
    let mut events = 0usize;
    let mut in_event = false;
    let mut first = true;
    let (mut summary, mut start, mut end, mut location, mut description, mut tzid) =
        (None, None, None, None, None, None);
    let mut all_day = false;
    for line in &lines {
        if line == "BEGIN:VEVENT" {
            events += 1;
            in_event = true;
            continue;
        }
        if line == "END:VEVENT" {
            in_event = false;
            first = false;
            continue;
        }
        if !in_event || !first {
            continue;
        }
        let Some((head, value)) = line.split_once(':') else {
            continue;
        };
        let (name, params) = head.split_once(';').unwrap_or((head, ""));
        match name.to_ascii_uppercase().as_str() {
            "SUMMARY" => summary = clean(&unescape(value), 300),
            "LOCATION" => location = clean(&unescape(value), 300),
            "DESCRIPTION" => description = clean(&unescape(value), 500),
            "DTSTART" => {
                if let Some((v, day)) = when(value, params) {
                    start = Some(v);
                    all_day = day;
                }
                tzid = params
                    .split(';')
                    .find_map(|p| p.strip_prefix("TZID="))
                    .map(|t| t.trim_matches('"').to_string());
            }
            "DTEND" => end = when(value, params).map(|(v, _)| v),
            _ => {}
        }
    }
    let start = start?;
    Some(Card {
        title: summary.or_else(|| Some("Event".into())),
        description,
        image: None,
        details: json!({
            "type": "event",
            "start": start,
            "end": end,
            "all_day": all_day,
            "location": location,
            "tz": tzid,
            "events": events,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_event_parses_with_zone_and_folding() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\n\
            SUMMARY:Launch party\\, v2\r\n\
            DTSTART;TZID=Europe/Berlin:20261101T100000\r\n\
            DTEND;TZID=Europe/Berlin:20261101T120000\r\n\
            LOCATION:Main St 1\\n Berlin\r\n\
            DESCRIPTION:A long descr\r\n iption folded\r\n\
            END:VEVENT\r\nBEGIN:VEVENT\r\nSUMMARY:Second\r\nDTSTART:20261102T080000Z\r\nEND:VEVENT\r\n\
            END:VCALENDAR\r\n";
        let c = event_card(ics).unwrap();
        assert_eq!(c.title.as_deref(), Some("Launch party, v2"));
        assert_eq!(c.description.as_deref(), Some("A long description folded"));
        let d = &c.details;
        assert_eq!(d["start"], "2026-11-01T09:00:00Z");
        assert_eq!(d["end"], "2026-11-01T11:00:00Z");
        assert_eq!(d["all_day"], false);
        assert_eq!(d["location"], "Main St 1 Berlin");
        assert_eq!(d["events"], 2);

        let day = "BEGIN:VEVENT\nSUMMARY:Holiday\nDTSTART;VALUE=DATE:20261225\nEND:VEVENT";
        let c = event_card(day).unwrap();
        assert_eq!(c.details["start"], "2026-12-25");
        assert_eq!(c.details["all_day"], true);
        assert!(event_card("BEGIN:VCALENDAR\nEND:VCALENDAR").is_none());
    }
}

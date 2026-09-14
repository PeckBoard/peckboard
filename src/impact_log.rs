//! Test-only recorder of "which route did this request hit, and when".
//!
//! This is the backend half of the e2e impact map. e2e specs never import
//! app source — they drive a browser and this HTTP server — so nothing
//! static links `src/routes/sessions/mod.rs` to `session-lifecycle.spec.ts`.
//! Runtime evidence is the only thing that knows.
//!
//! Attribution is by wall clock, not by a request header: the suite runs
//! `workers: 1` against one server per shard, so exactly one test is in
//! flight at any moment. Joining this log against each test's start/end
//! window (recorded by `web/e2e/impact/reporter.ts`) is therefore exact —
//! and, unlike a header, it also captures WebSocket upgrades and any
//! request the app makes on its own.
//!
//! Inactive unless `PECKBOARD_E2E_ROUTE_LOG` names a file to append to, so
//! production pays one `OnceLock` read per request and nothing else.

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static SINK: OnceLock<Option<Mutex<File>>> = OnceLock::new();

fn sink() -> Option<&'static Mutex<File>> {
    SINK.get_or_init(|| {
        let path = std::env::var("PECKBOARD_E2E_ROUTE_LOG").ok()?;
        if path.is_empty() {
            return None;
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()
            .map(Mutex::new)
    })
    .as_ref()
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// One JSONL record: `{"t":<unix_ms>,"route":"/api/sessions/{id}"}`.
///
/// Always the route PATTERN, never the concrete URL — the impact map joins
/// these against the literal `.route("…")` registrations in
/// `src/routes/**`, and `/api/sessions/abc123` would match none of them.
fn record_line(route: &str, at_ms: u128) -> String {
    format!(
        "{{\"t\":{},\"route\":{}}}\n",
        at_ms,
        serde_json::to_string(route).unwrap_or_else(|_| "\"?\"".into())
    )
}

/// Appends a record per routed request.
///
/// Requests that match no route (static assets served by the fallback)
/// carry no [`MatchedPath`] and are skipped — they say nothing about which
/// Rust source a change should re-test.
pub async fn record_route(req: Request, next: Next) -> Response {
    if let Some(sink) = sink()
        && let Some(route) = req.extensions().get::<MatchedPath>()
    {
        let line = record_line(route.as_str(), now_ms());
        if let Ok(mut file) = sink.lock() {
            let _ = file.write_all(line.as_bytes());
        }
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_line_is_json_naming_the_pattern() {
        let line = record_line("/api/sessions/{id}", 1_700_000_000_000);
        let parsed: serde_json::Value =
            serde_json::from_str(line.trim()).expect("record is valid JSON");
        // The `{id}` braces are exactly the case a naive format! would
        // mangle, and the pattern is what the map keys off.
        assert_eq!(parsed["route"], "/api/sessions/{id}");
        assert_eq!(parsed["t"], 1_700_000_000_000u64);
        assert!(line.ends_with('\n'), "JSONL records must be newline-framed");
    }

    #[test]
    fn record_line_escapes_quotes_in_a_route() {
        let line = record_line("/api/\"odd\"", 1);
        let parsed: serde_json::Value =
            serde_json::from_str(line.trim()).expect("record is valid JSON");
        assert_eq!(parsed["route"], "/api/\"odd\"");
    }
}

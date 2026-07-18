//! Browser test-run recording — the data source for the `playwright-video`
//! plugin's LogRocket-style replay. Every `browser_open` starts a run; each
//! `browser_act` appends a timestamped step with a server-side screenshot
//! frame (captured out of the agent's token budget — frames never enter
//! model context); `browser_close` finalizes. The capture sidecar (see
//! `service/browser.rs`) additionally streams network request/response and
//! console events, which are ingested here after every step.
//!
//! Layout: `data_dir/browser-runs/<run_id>/meta.json` + `NNNN.png` frames.
//! `meta.json` is rewritten after every step (small, cheap) so a crashed
//! child still leaves a replayable prefix.
//!
//! Everything ingested from the page (headers, bodies, URLs, console text,
//! typed text) is masked via [`crate::service::redact`] BEFORE persisting —
//! secrets never reach disk.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::service::redact;

/// Hard caps keeping `meta.json` cheap to rewrite and bounded on disk.
const MAX_NET_EVENTS: usize = 600;
const MAX_CONSOLE_EVENTS: usize = 400;
const BODY_CAP_CHARS: usize = 4096;
const HEADER_VALUE_CAP_CHARS: usize = 512;
const CONSOLE_TEXT_CAP_CHARS: usize = 2000;

#[derive(Serialize, Deserialize, Clone)]
pub struct RunStep {
    pub n: u32,
    /// Wall-clock ms — real spacing drives time-scaled playback.
    pub ts_ms: i64,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Compact action payload (text/url/key/…), for the event track.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
    /// Frame filename within the run dir, when a screenshot was captured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame: Option<String>,
}

/// One captured network request (+ its response, once finished). Every
/// string field is stored masked.
#[derive(Serialize, Deserialize, Clone)]
pub struct NetEvent {
    /// Sidecar-assigned request id (unique within the run's page).
    pub id: u64,
    /// Request start, wall-clock ms.
    pub ts_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dur_ms: Option<i64>,
    pub method: String,
    pub url: String,
    /// Playwright resource type: document/xhr/fetch/script/stylesheet/…
    pub resource_type: String,
    /// None while in flight or when the request failed before a response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub req_headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub req_body: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resp_headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resp_body: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resp_body_truncated: bool,
    /// Response size in bytes (from content-length), when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// One captured console line (or page error). Text is stored masked.
#[derive(Serialize, Deserialize, Clone)]
pub struct ConsoleEvent {
    pub ts_ms: i64,
    /// Playwright console type: log/info/warning/error/debug/… (`error` also
    /// covers uncaught page errors).
    pub level: String,
    pub text: String,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[derive(Serialize, Deserialize, Clone)]
pub struct RunMeta {
    pub id: String,
    pub name: String,
    pub url: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card_id: Option<String>,
    pub started_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_ms: Option<i64>,
    pub steps: Vec<RunStep>,
    /// Captured network traffic, masked. Empty for runs recorded before
    /// capture existed (defaults keep old meta.json files loadable).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network: Vec<NetEvent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub console_events: Vec<ConsoleEvent>,
    /// Events dropped once the per-run caps were hit.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub network_truncated: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub console_truncated: u32,
}

struct ActiveRun {
    dir: PathBuf,
    meta: RunMeta,
    next_frame: u32,
    /// Sidecar event cursor — the `next` value of the last ingested batch.
    events_cursor: u64,
    /// Sidecar request id → index into `meta.network`, for merging the
    /// finish event into its request.
    net_index: HashMap<u64, usize>,
}

/// page_id → in-flight run. A page has at most one run.
fn active() -> &'static Mutex<HashMap<String, ActiveRun>> {
    static M: OnceLock<Mutex<HashMap<String, ActiveRun>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn runs_root(data_dir: &Path) -> PathBuf {
    data_dir.join("browser-runs")
}

fn persist(run: &ActiveRun) {
    if let Ok(json) = serde_json::to_string(&run.meta) {
        let _ = std::fs::write(run.dir.join("meta.json"), json);
    }
}

/// Begin recording for `page_id`. Best-effort: any fs failure just disables
/// recording for this run (browser tools must never fail because of it).
pub fn start(
    data_dir: &Path,
    page_id: &str,
    name: &str,
    url: &str,
    session_id: &str,
    project_id: Option<&str>,
    card_id: Option<&str>,
) {
    let id = uuid::Uuid::new_v4().to_string();
    let dir = runs_root(data_dir).join(&id);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let run = ActiveRun {
        dir,
        meta: RunMeta {
            id,
            name: name.to_string(),
            url: url.to_string(),
            session_id: session_id.to_string(),
            project_id: project_id.map(str::to_string),
            card_id: card_id.map(str::to_string),
            started_ms: now_ms(),
            ended_ms: None,
            steps: Vec::new(),
            network: Vec::new(),
            console_events: Vec::new(),
            network_truncated: 0,
            console_truncated: 0,
        },
        next_frame: 0,
        events_cursor: 0,
        net_index: HashMap::new(),
    };
    persist(&run);
    let mut map = active().lock().unwrap_or_else(|p| p.into_inner());
    map.insert(page_id.to_string(), run);
}

/// Append a step (with an optional base64 PNG frame) to `page_id`'s run.
/// Free-text detail values (typed/filled text) are masked before persisting.
/// No-op when the page isn't being recorded.
pub fn record_step(
    page_id: &str,
    action: &str,
    target: Option<&str>,
    detail: Option<serde_json::Value>,
    frame_base64: Option<&str>,
) {
    let mut map = active().lock().unwrap_or_else(|p| p.into_inner());
    let Some(run) = map.get_mut(page_id) else {
        return;
    };
    let frame = frame_base64.and_then(|b64| {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
        let name = format!("{:04}.png", run.next_frame);
        std::fs::write(run.dir.join(&name), bytes).ok()?;
        run.next_frame += 1;
        Some(name)
    });
    let detail = detail.map(|mut d| {
        redact::mask_json(&mut d);
        d
    });
    let n = run.meta.steps.len() as u32;
    run.meta.steps.push(RunStep {
        n,
        ts_ms: now_ms(),
        action: action.to_string(),
        target: target.map(str::to_string),
        detail,
        frame,
    });
    persist(run);
}

/// The sidecar event cursor for `page_id`, or None when the page isn't being
/// recorded (callers skip the events round-trip entirely).
pub fn events_cursor(page_id: &str) -> Option<u64> {
    let map = active().lock().unwrap_or_else(|p| p.into_inner());
    map.get(page_id).map(|r| r.events_cursor)
}

fn cap_chars(s: &str, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        (s.to_string(), false)
    } else {
        (s.chars().take(max).collect(), true)
    }
}

fn json_headers(v: Option<&serde_json::Value>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(obj) = v.and_then(|v| v.as_object()) else {
        return out;
    };
    for (k, val) in obj.iter().take(64) {
        if let Some(s) = val.as_str() {
            out.insert(k.clone(), cap_chars(s, HEADER_VALUE_CAP_CHARS).0);
        }
    }
    out
}

/// Ingest a batch of sidecar capture events (`{events: [...], next}`) into
/// `page_id`'s run, masking every string surface before it is persisted.
/// No-op when the page isn't being recorded.
pub fn ingest_events(page_id: &str, payload: &serde_json::Value) {
    let mut map = active().lock().unwrap_or_else(|p| p.into_inner());
    let Some(run) = map.get_mut(page_id) else {
        return;
    };
    if let Some(next) = payload.get("next").and_then(|v| v.as_u64()) {
        run.events_cursor = next;
    }
    let Some(events) = payload.get("events").and_then(|v| v.as_array()) else {
        return;
    };
    if events.is_empty() {
        return;
    }
    for ev in events {
        let kind = ev.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let ts = ev.get("ts").and_then(|v| v.as_i64()).unwrap_or_else(now_ms);
        match kind {
            "net-req" => {
                let Some(id) = ev.get("id").and_then(|v| v.as_u64()) else {
                    continue;
                };
                if run.meta.network.len() >= MAX_NET_EVENTS {
                    run.meta.network_truncated += 1;
                    continue;
                }
                let raw_headers = json_headers(ev.get("headers"));
                let content_type = raw_headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                    .map(|(_, v)| v.clone());
                let req_body = ev.get("postData").and_then(|v| v.as_str()).map(|b| {
                    cap_chars(
                        &redact::mask_body(content_type.as_deref(), b),
                        BODY_CAP_CHARS,
                    )
                    .0
                });
                let url = ev.get("url").and_then(|v| v.as_str()).unwrap_or("");
                run.net_index.insert(id, run.meta.network.len());
                run.meta.network.push(NetEvent {
                    id,
                    ts_ms: ts,
                    dur_ms: None,
                    method: ev
                        .get("method")
                        .and_then(|v| v.as_str())
                        .unwrap_or("GET")
                        .to_string(),
                    url: redact::mask_url(url),
                    resource_type: ev
                        .get("resourceType")
                        .and_then(|v| v.as_str())
                        .unwrap_or("other")
                        .to_string(),
                    status: None,
                    failure: None,
                    req_headers: redact::mask_headers(&raw_headers),
                    req_body,
                    resp_headers: BTreeMap::new(),
                    resp_body: None,
                    resp_body_truncated: false,
                    size: None,
                });
            }
            "net-fin" => {
                let Some(idx) = ev
                    .get("id")
                    .and_then(|v| v.as_u64())
                    .and_then(|id| run.net_index.get(&id).copied())
                else {
                    continue; // request was dropped by the cap, or unknown
                };
                let raw_headers = json_headers(ev.get("headers"));
                let content_type = raw_headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                    .map(|(_, v)| v.clone());
                let (body, sidecar_truncated) = (
                    ev.get("body").and_then(|v| v.as_str()),
                    ev.get("bodyTruncated")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                );
                let ne = &mut run.meta.network[idx];
                ne.dur_ms = Some((ts - ne.ts_ms).max(0));
                ne.status = ev
                    .get("status")
                    .and_then(|v| v.as_u64())
                    .map(|s| s.min(u64::from(u16::MAX)) as u16);
                ne.failure = ev
                    .get("failure")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                ne.size = ev.get("size").and_then(|v| v.as_u64());
                ne.resp_headers = redact::mask_headers(&raw_headers);
                if let Some(b) = body {
                    let (capped, was_capped) = cap_chars(
                        &redact::mask_body(content_type.as_deref(), b),
                        BODY_CAP_CHARS,
                    );
                    ne.resp_body = Some(capped);
                    ne.resp_body_truncated = was_capped || sidecar_truncated;
                }
            }
            "console" => {
                if run.meta.console_events.len() >= MAX_CONSOLE_EVENTS {
                    run.meta.console_truncated += 1;
                    continue;
                }
                let text = ev.get("text").and_then(|v| v.as_str()).unwrap_or("");
                run.meta.console_events.push(ConsoleEvent {
                    ts_ms: ts,
                    level: ev
                        .get("level")
                        .and_then(|v| v.as_str())
                        .unwrap_or("log")
                        .to_string(),
                    text: cap_chars(&redact::mask_text(text), CONSOLE_TEXT_CAP_CHARS).0,
                });
            }
            _ => {}
        }
    }
    persist(run);
}

/// Finalize `page_id`'s run (stamps `ended_ms`, drops the active handle).
pub fn finish(page_id: &str) {
    let mut map = active().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(mut run) = map.remove(page_id) {
        run.meta.ended_ms = Some(now_ms());
        persist(&run);
    }
}

// ── read side (host functions for the playwright-video plugin) ─────────

/// Reject anything that isn't one of our generated uuid-ish / frame names —
/// these strings come from plugin input and are joined into paths.
fn safe_component(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        && !s.contains("..")
}

/// Every run's meta, newest first, steps included.
pub fn list_runs(data_dir: &Path) -> Vec<RunMeta> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(runs_root(data_dir)) else {
        return out;
    };
    for e in entries.flatten() {
        if let Ok(json) = std::fs::read_to_string(e.path().join("meta.json"))
            && let Ok(meta) = serde_json::from_str::<RunMeta>(&json)
        {
            out.push(meta);
        }
    }
    out.sort_by(|a, b| b.started_ms.cmp(&a.started_ms));
    out
}

pub fn get_run(data_dir: &Path, run_id: &str) -> Option<RunMeta> {
    if !safe_component(run_id) {
        return None;
    }
    let json = std::fs::read_to_string(runs_root(data_dir).join(run_id).join("meta.json")).ok()?;
    serde_json::from_str(&json).ok()
}

/// A frame's bytes as base64 (frames are small viewport PNGs).
pub fn get_frame(data_dir: &Path, run_id: &str, frame: &str) -> Option<String> {
    if !safe_component(run_id) || !safe_component(frame) || !frame.ends_with(".png") {
        return None;
    }
    let bytes = std::fs::read(runs_root(data_dir).join(run_id).join(frame)).ok()?;
    use base64::Engine;
    Some(base64::engine::general_purpose::STANDARD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_read_back_a_run() {
        let dir = tempfile::tempdir().unwrap();
        start(
            dir.path(),
            "page-1",
            "login test",
            "https://example.com",
            "s-1",
            Some("p-1"),
            None,
        );
        // 1x1 transparent PNG.
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
        record_step("page-1", "open", None, None, Some(png_b64));
        record_step(
            "page-1",
            "click",
            Some("e3"),
            Some(serde_json::json!({ "outline": false })),
            Some(png_b64),
        );
        record_step("page-1", "wait_ms", None, None, None);
        finish("page-1");

        let runs = list_runs(dir.path());
        assert_eq!(runs.len(), 1);
        let run = &runs[0];
        assert_eq!(run.name, "login test");
        assert_eq!(run.session_id, "s-1");
        assert!(run.ended_ms.is_some());
        assert_eq!(run.steps.len(), 3);
        assert_eq!(run.steps[0].frame.as_deref(), Some("0000.png"));
        assert_eq!(run.steps[1].action, "click");
        assert_eq!(run.steps[1].target.as_deref(), Some("e3"));
        assert!(run.steps[2].frame.is_none());

        let full = get_run(dir.path(), &run.id).unwrap();
        assert_eq!(full.steps.len(), 3);
        let frame = get_frame(dir.path(), &run.id, "0000.png").unwrap();
        assert!(!frame.is_empty());

        // Traversal-ish inputs are rejected.
        assert!(get_run(dir.path(), "../etc").is_none());
        assert!(get_frame(dir.path(), &run.id, "../meta.json").is_none());
    }

    #[test]
    fn typed_secrets_in_step_detail_are_masked() {
        let dir = tempfile::tempdir().unwrap();
        start(
            dir.path(),
            "page-2",
            "t",
            "https://x.com",
            "s-1",
            None,
            None,
        );
        record_step(
            "page-2",
            "fill",
            Some("e9"),
            Some(serde_json::json!({ "text": "Bearer super.secret.value1234" })),
            None,
        );
        finish("page-2");
        let runs = list_runs(dir.path());
        let run = &runs[0];
        let detail = serde_json::to_string(&run.steps[0].detail).unwrap();
        assert!(!detail.contains("super.secret.value1234"), "got: {detail}");
    }

    #[test]
    fn ingest_masks_merges_and_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        start(
            dir.path(),
            "page-3",
            "api test",
            "https://app.example",
            "s-2",
            None,
            None,
        );
        ingest_events(
            "page-3",
            &serde_json::json!({
                "next": 3,
                "events": [
                    { "seq": 1, "kind": "net-req", "id": 7, "ts": 1000, "method": "POST",
                      "url": "https://api.example/login?access_token=shhh",
                      "resourceType": "xhr",
                      "headers": { "authorization": "Bearer topsecret123", "content-type": "application/json" },
                      "postData": "{\"user\":\"jo\",\"password\":\"hunter2\"}" },
                    { "seq": 2, "kind": "console", "ts": 1050, "level": "error",
                      "text": "boom with Bearer abc123456789" }
                ]
            }),
        );
        ingest_events(
            "page-3",
            &serde_json::json!({
                "next": 4,
                "events": [
                    { "seq": 3, "kind": "net-fin", "id": 7, "ts": 1420, "status": 401,
                      "headers": { "set-cookie": "sid=1", "content-type": "application/json" },
                      "body": "{\"error\":\"bad\",\"refresh_token\":\"r-1\"}", "size": 38 }
                ]
            }),
        );
        assert_eq!(events_cursor("page-3"), Some(4));
        finish("page-3");
        assert_eq!(events_cursor("page-3"), None);

        let run = get_run(dir.path(), &list_runs(dir.path())[0].id).unwrap();
        assert_eq!(run.network.len(), 1);
        let ne = &run.network[0];
        assert_eq!(ne.method, "POST");
        assert_eq!(ne.status, Some(401));
        assert_eq!(ne.dur_ms, Some(420));
        assert_eq!(ne.size, Some(38));
        assert!(ne.url.ends_with("access_token=«masked»"), "got: {}", ne.url);
        assert_eq!(ne.req_headers["authorization"], redact::MASK);
        assert_eq!(ne.resp_headers["set-cookie"], redact::MASK);
        assert!(!ne.req_body.as_ref().unwrap().contains("hunter2"));
        assert!(!ne.resp_body.as_ref().unwrap().contains("r-1"));
        assert_eq!(run.console_events.len(), 1);
        assert!(!run.console_events[0].text.contains("abc123456789"));

        // A legacy meta.json without the new fields still parses.
        let legacy =
            r#"{"id":"x","name":"n","url":"u","session_id":"s","started_ms":1,"steps":[]}"#;
        let m: RunMeta = serde_json::from_str(legacy).unwrap();
        assert!(m.network.is_empty() && m.console_events.is_empty());
    }

    #[test]
    fn event_caps_are_enforced() {
        let dir = tempfile::tempdir().unwrap();
        start(dir.path(), "page-4", "cap", "https://x", "s", None, None);
        let events: Vec<serde_json::Value> = (0..(MAX_NET_EVENTS as u64 + 10))
            .map(|i| {
                serde_json::json!({ "seq": i + 1, "kind": "net-req", "id": i, "ts": 1,
                    "method": "GET", "url": "https://x/a", "resourceType": "fetch", "headers": {} })
            })
            .collect();
        ingest_events(
            "page-4",
            &serde_json::json!({ "next": events.len(), "events": events }),
        );
        finish("page-4");
        let runs = list_runs(dir.path());
        let run = &runs[0];
        assert_eq!(run.network.len(), MAX_NET_EVENTS);
        assert_eq!(run.network_truncated, 10);
    }
}

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
const MAX_POINTER_EVENTS: usize = 5000;
/// rrweb DOM recording. Events live in `dom.jsonl` (one JSON value per line),
/// never in `meta.json` — a full snapshot is larger than the wasm plugin can
/// hold in a list response. The cap is the whole file.
const DOM_DISK_CAP: u64 = 50 * 1024 * 1024;
const DOM_CHUNK_TARGET: usize = 256 * 1024;
const DOM_EVENT_MAX: usize = 8 * 1024 * 1024;

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

/// One sampled pointer position (cursor replay). `t` is `move` or `down`;
/// coordinates are CSS px within a `vw`×`vh` viewport (the player
/// normalizes, so device pixel ratio never matters).
#[derive(Serialize, Deserialize, Clone)]
pub struct PointerEvent {
    pub ts_ms: i64,
    pub t: String,
    pub x: i32,
    pub y: i32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub vw: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub vh: u32,
}
fn is_zero(n: &u32) -> bool {
    *n == 0
}
fn is_false(b: &bool) -> bool {
    !*b
}
fn is_zero_u64(n: &u64) -> bool {
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
    /// Sampled cursor positions from the capture sidecar (cursor replay).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pointer_events: Vec<PointerEvent>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub pointer_truncated: u32,
    /// True once at least one rrweb event was written to `dom.jsonl`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub has_dom: bool,
    /// Events dropped by the disk cap or by the sidecar buffer.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub dom_truncated: u32,
    /// Bytes written to `dom.jsonl`, including newlines.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub dom_bytes: u64,
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
    /// Sidecar DOM-buffer cursor (`next` from `/dom?since=`).
    dom_cursor: u64,
    /// Cumulative `dropped` last reported by the sidecar.
    dom_sidecar_dropped: u64,
    /// Per-run disk cap. Production uses [`DOM_DISK_CAP`]; tests shrink it.
    dom_cap: u64,
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

/// Begin recording for `page_id`, returning the new run id. Best-effort: any
/// fs failure just disables recording for this run and returns `None`
/// (browser tools must never fail because of it).
pub fn start(
    data_dir: &Path,
    page_id: &str,
    name: &str,
    url: &str,
    session_id: &str,
    project_id: Option<&str>,
    card_id: Option<&str>,
) -> Option<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let dir = runs_root(data_dir).join(&id);
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
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
            pointer_events: Vec::new(),
            pointer_truncated: 0,
            has_dom: false,
            dom_truncated: 0,
            dom_bytes: 0,
        },
        next_frame: 0,
        events_cursor: 0,
        dom_cursor: 0,
        dom_sidecar_dropped: 0,
        dom_cap: DOM_DISK_CAP,
        net_index: HashMap::new(),
    };
    persist(&run);
    let ret = run.meta.id.clone();
    let mut map = active().lock().unwrap_or_else(|p| p.into_inner());
    map.insert(page_id.to_string(), run);
    Some(ret)
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

/// The active run id for `page_id`, or None when the page isn't recorded.
/// Lets browser tool results carry the id so the chat can deep-link the
/// replay.
pub fn run_id(page_id: &str) -> Option<String> {
    let map = active().lock().unwrap_or_else(|p| p.into_inner());
    map.get(page_id).map(|r| r.meta.id.clone())
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
                // Prefer the sidecar's browser-reported HAR timing (start = epoch
                // ms, dur = ms to last body byte): CDP delivers events in batches,
                // so req/fin arrival deltas flatten real load times.
                if let Some(s) = ev.get("start").and_then(|v| v.as_f64()) {
                    if s > 0.0 {
                        ne.ts_ms = s.round() as i64;
                    }
                }
                ne.dur_ms = Some(match ev.get("dur").and_then(|v| v.as_f64()) {
                    Some(d) if d >= 0.0 => d.round() as i64,
                    _ => (ts - ne.ts_ms).max(0),
                });
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
            "pointer" => {
                if run.meta.pointer_events.len() >= MAX_POINTER_EVENTS {
                    run.meta.pointer_truncated += 1;
                    continue;
                }
                run.meta.pointer_events.push(PointerEvent {
                    ts_ms: ts,
                    t: match ev.get("t").and_then(|v| v.as_str()) {
                        Some("down") => "down".to_string(),
                        _ => "move".to_string(),
                    },
                    x: ev.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
                    y: ev.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
                    vw: ev
                        .get("vw")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0)
                        .min(u64::from(u32::MAX)) as u32,
                    vh: ev
                        .get("vh")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0)
                        .min(u64::from(u32::MAX)) as u32,
                });
            }
            _ => {}
        }
    }
    persist(run);
}

/// Whether `page_id` still has an in-flight run. The background DOM drain
/// loops on this and stops once [`finish`] drops the handle.
pub fn is_recording(page_id: &str) -> bool {
    let map = active().lock().unwrap_or_else(|p| p.into_inner());
    map.contains_key(page_id)
}

/// Sidecar DOM cursor for `page_id`, or None when the page isn't recorded.
pub fn dom_cursor(page_id: &str) -> Option<u64> {
    let map = active().lock().unwrap_or_else(|p| p.into_inner());
    map.get(page_id).map(|r| r.dom_cursor)
}

/// Ingest a sidecar DOM batch (`{events: [{seq, event}], next, dropped}`).
/// Each rrweb event is masked (`mask_text` only — not the high-entropy pass,
/// which would destroy CSS hashes) and appended to `dom.jsonl`.
pub fn ingest_dom(page_id: &str, payload: &serde_json::Value) {
    let mut map = active().lock().unwrap_or_else(|p| p.into_inner());
    let Some(run) = map.get_mut(page_id) else {
        return;
    };
    let before = (run.meta.has_dom, run.meta.dom_truncated);
    let seen = run.dom_cursor;
    if let Some(next) = payload.get("next").and_then(|v| v.as_u64())
        && next > run.dom_cursor
    {
        run.dom_cursor = next;
    }
    if let Some(dropped) = payload.get("dropped").and_then(|v| v.as_u64())
        && dropped > run.dom_sidecar_dropped
    {
        let delta = dropped - run.dom_sidecar_dropped;
        run.meta.dom_truncated = run
            .meta
            .dom_truncated
            .saturating_add(delta.min(u64::from(u32::MAX)) as u32);
        run.dom_sidecar_dropped = dropped;
    }
    let Some(events) = payload.get("events").and_then(|v| v.as_array()) else {
        return;
    };
    for ev in events {
        let seq = ev.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
        // Two drains can pull the same `since` before either lands. Seq is the
        // sidecar's cursor; writing those events again doubles the recording.
        if seq > 0 && seq <= seen {
            continue;
        }
        let Some(raw) = ev.get("event") else {
            continue;
        };
        let mut event = raw.clone();
        mask_dom_strings(&mut event);
        write_dom_event(run, &event);
    }
    // meta.json only when what the player reads changes. `dom_bytes` lands
    // with `finish`; rewriting meta every drain tick is wasted I/O.
    if (run.meta.has_dom, run.meta.dom_truncated) != before {
        persist(run);
    }
}

fn mask_dom_strings(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::String(s) => *s = redact::mask_text(s),
        serde_json::Value::Array(items) => {
            for item in items {
                mask_dom_strings(item);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values_mut() {
                mask_dom_strings(item);
            }
        }
        _ => {}
    }
}

/// Append one event line. Returns false when the cap (or I/O) refused it.
fn write_dom_event(run: &mut ActiveRun, event: &serde_json::Value) -> bool {
    let Ok(line) = serde_json::to_string(event) else {
        return false;
    };
    let extra = (line.len() + 1) as u64;
    if run.meta.dom_bytes.saturating_add(extra) > run.dom_cap {
        run.meta.dom_truncated = run.meta.dom_truncated.saturating_add(1);
        return false;
    }
    let path = run.dir.join("dom.jsonl");
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(_) => return false,
    };
    use std::io::Write;
    // One write: a reader polling a live run must never see half a line.
    let mut buf = line.into_bytes();
    buf.push(b'\n');
    if file.write_all(&buf).is_err() {
        return false;
    }
    run.meta.dom_bytes += extra;
    run.meta.has_dom = true;
    true
}

/// One chunk of a run's rrweb recording, for `peckboard_browser_run_dom`.
#[derive(Debug)]
pub struct DomChunk {
    pub events: Vec<serde_json::Value>,
    pub next_offset: u64,
    pub eof: bool,
    pub truncated: bool,
}

/// Read complete `dom.jsonl` lines starting at byte `offset`.
///
/// Targets ~256 KB. A single event larger than that is returned alone, up to
/// 8 MB; a larger line is skipped and `truncated` is set. `None` when the run
/// does not exist.
pub fn read_dom_chunk(data_dir: &Path, run_id: &str, offset: u64) -> Option<DomChunk> {
    read_dom_chunk_limited(data_dir, run_id, offset, DOM_CHUNK_TARGET, DOM_EVENT_MAX)
}

fn read_dom_chunk_limited(
    data_dir: &Path,
    run_id: &str,
    offset: u64,
    chunk_target: usize,
    event_max: usize,
) -> Option<DomChunk> {
    use std::io::{BufRead, Seek, SeekFrom};
    let meta = get_run(data_dir, run_id)?;
    let truncated_run = meta.dom_truncated > 0;
    let empty = |next_offset: u64| DomChunk {
        events: Vec::new(),
        next_offset,
        eof: true,
        truncated: truncated_run,
    };
    let path = runs_root(data_dir).join(run_id).join("dom.jsonl");
    let Ok(file) = std::fs::File::open(&path) else {
        return Some(empty(0));
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if offset >= len {
        return Some(empty(offset));
    }
    // Seek instead of reading the file: a long run's recording is tens of
    // MB and the player pulls it in 256 KB chunks.
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    let mut pos = offset;
    if offset > 0 {
        // Re-read the byte before `offset`: a newline there means the offset
        // is on a line boundary; anything else resumes at the next line.
        reader.seek(SeekFrom::Start(offset - 1)).ok()?;
        let n = reader.read_until(b'\n', &mut line).ok()? as u64;
        if line.last() != Some(&b'\n') {
            return Some(empty(offset));
        }
        pos = offset - 1 + n;
    }
    let mut events = Vec::new();
    let mut out_bytes = 0usize;
    let mut skipped = false;
    let mut partial = false;
    loop {
        line.clear();
        let n = match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        // No newline yet: a live run is mid-append. Leave the line for the
        // next pull instead of skipping it as unparseable.
        if line.last() != Some(&b'\n') {
            partial = true;
            break;
        }
        let body = &line[..n - 1];
        let body = body.strip_suffix(b"\r").unwrap_or(body);
        if body.is_empty() {
            pos += n as u64;
            continue;
        }
        if body.len() > event_max {
            skipped = true;
            pos += n as u64;
            continue;
        }
        if !events.is_empty() && out_bytes + body.len() > chunk_target {
            break;
        }
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
            events.push(v);
            out_bytes += body.len();
        }
        pos += n as u64;
        if out_bytes >= chunk_target {
            break;
        }
    }
    Some(DomChunk {
        events,
        next_offset: pos,
        eof: partial || pos >= len,
        truncated: truncated_run || skipped,
    })
}

#[cfg(test)]
fn set_dom_cap_for_test(page_id: &str, cap: u64) {
    let mut map = active().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(run) = map.get_mut(page_id) {
        run.dom_cap = cap;
    }
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

/// One run's list-view summary: identity, timing, steps slimmed to their
/// action/frame skeleton, and precomputed list-badge counts. This is what
/// `peckboard_browser_runs` hands the plugin instead of full metas: serving
/// every run's network bodies, console text and pointer samples in one JSON
/// blob OOMed the plugin's 128 MB wasm instance once ~170 runs (~23 MB of
/// meta.json) had accumulated. The player refetches one full run on open via
/// [`get_run`].
#[derive(Serialize, Clone)]
pub struct RunSummary {
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
    /// Steps without `target`/`detail` (the free-text bulk). Kept as an array
    /// so a pre-0.4.2 playwright-video plugin can still derive its step and
    /// frame counts from `steps`.
    pub steps: Vec<RunStep>,
    /// `network.len()` of the full meta (the list's "N reqs" badge).
    pub request_count: u32,
    /// Failed/4xx/5xx requests plus `error`-level console lines (the list's
    /// red badge). Mirrors the plugin's own pre-0.4.2 computation.
    pub error_count: u32,
    /// Present when the run has an rrweb `dom.jsonl`. Absent on old runs.
    #[serde(default, skip_serializing_if = "is_false")]
    pub has_dom: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub dom_truncated: u32,
}

fn summarize(meta: RunMeta) -> RunSummary {
    let net_errs = meta
        .network
        .iter()
        .filter(|n| n.failure.is_some() || n.status.unwrap_or(0) >= 400)
        .count();
    let con_errs = meta
        .console_events
        .iter()
        .filter(|c| c.level == "error")
        .count();
    RunSummary {
        id: meta.id,
        name: meta.name,
        url: meta.url,
        session_id: meta.session_id,
        project_id: meta.project_id,
        card_id: meta.card_id,
        started_ms: meta.started_ms,
        ended_ms: meta.ended_ms,
        request_count: meta.network.len() as u32,
        error_count: (net_errs + con_errs) as u32,
        has_dom: meta.has_dom,
        dom_truncated: meta.dom_truncated,
        steps: meta
            .steps
            .into_iter()
            .map(|s| RunStep {
                target: None,
                detail: None,
                ..s
            })
            .collect(),
    }
}

/// Every run's list summary, newest first — bounded regardless of run count.
pub fn list_run_summaries(data_dir: &Path) -> Vec<RunSummary> {
    list_runs(data_dir).into_iter().map(summarize).collect()
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
    fn net_fin_browser_timing_overrides_arrival_deltas() {
        let dir = tempfile::tempdir().unwrap();
        start(
            dir.path(),
            "page-t",
            "timing",
            "https://x",
            "s-3",
            None,
            None,
        );
        ingest_events(
            "page-t",
            &serde_json::json!({
                "next": 5,
                "events": [
                    { "seq": 1, "kind": "net-req", "id": 1, "ts": 5000, "method": "GET",
                      "url": "https://cdn.example/a.css", "resourceType": "stylesheet" },
                    { "seq": 2, "kind": "net-req", "id": 2, "ts": 5001, "method": "GET",
                      "url": "https://cdn.example/b.css", "resourceType": "stylesheet" },
                    { "seq": 3, "kind": "net-fin", "id": 1, "ts": 5020, "status": 200,
                      "start": 4980.4, "dur": 351.6 },
                    { "seq": 4, "kind": "net-fin", "id": 2, "ts": 5021, "status": 200 }
                ]
            }),
        );
        finish("page-t");
        let run = get_run(dir.path(), &list_runs(dir.path())[0].id).unwrap();
        let a = &run.network[0];
        // browser timing wins: start rewrites ts_ms, dur used verbatim
        assert_eq!(a.ts_ms, 4980);
        assert_eq!(a.dur_ms, Some(352));
        let b = &run.network[1];
        // no timing fields → arrival-delta fallback
        assert_eq!(b.ts_ms, 5001);
        assert_eq!(b.dur_ms, Some(20));
    }

    #[test]
    fn pointer_events_ingest_and_cap() {
        let dir = tempfile::tempdir().unwrap();
        start(
            dir.path(),
            "page-ptr",
            "ptr",
            "https://x",
            "s-9",
            None,
            None,
        );
        ingest_events(
            "page-ptr",
            &serde_json::json!({
                "next": 3,
                "events": [
                    { "seq": 1, "kind": "pointer", "ts": 100, "t": "move",
                      "x": 10, "y": 20, "vw": 1280, "vh": 720 },
                    { "seq": 2, "kind": "pointer", "ts": 160, "t": "down",
                      "x": 11, "y": 21, "vw": 1280, "vh": 720 },
                    { "seq": 3, "kind": "pointer", "ts": 200, "t": "weird",
                      "x": -5, "y": 7 }
                ]
            }),
        );
        finish("page-ptr");
        let run = get_run(dir.path(), &list_runs(dir.path())[0].id).unwrap();
        assert_eq!(run.pointer_events.len(), 3);
        assert_eq!(run.pointer_events[0].t, "move");
        assert_eq!(run.pointer_events[1].t, "down");
        // Unknown kinds coerce to "move"; missing viewport stays 0.
        assert_eq!(run.pointer_events[2].t, "move");
        assert_eq!(run.pointer_events[2].x, -5);
        assert_eq!(run.pointer_events[0].vw, 1280);
        assert_eq!(run.pointer_events[2].vw, 0);

        // Cap: past MAX_POINTER_EVENTS only the truncation counter grows.
        let dir2 = tempfile::tempdir().unwrap();
        start(
            dir2.path(),
            "page-ptr2",
            "ptr2",
            "https://x",
            "s-9",
            None,
            None,
        );
        let events: Vec<serde_json::Value> = (0..(MAX_POINTER_EVENTS as u64 + 10))
            .map(|i| {
                serde_json::json!({ "seq": i + 1, "kind": "pointer", "ts": i,
                    "t": "move", "x": i, "y": 0 })
            })
            .collect();
        ingest_events(
            "page-ptr2",
            &serde_json::json!({ "next": MAX_POINTER_EVENTS as u64 + 10, "events": events }),
        );
        finish("page-ptr2");
        let run2 = get_run(dir2.path(), &list_runs(dir2.path())[0].id).unwrap();
        assert_eq!(run2.pointer_events.len(), MAX_POINTER_EVENTS);
        assert_eq!(run2.pointer_truncated, 10);

        // A legacy meta.json without pointer fields still parses.
        let legacy =
            r#"{"id":"x","name":"n","url":"u","session_id":"s","started_ms":1,"steps":[]}"#;
        let m: RunMeta = serde_json::from_str(legacy).unwrap();
        assert!(m.pointer_events.is_empty() && m.pointer_truncated == 0);
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

    #[test]
    fn list_summaries_are_slim_with_correct_counts() {
        let dir = tempfile::tempdir().unwrap();
        start(
            dir.path(),
            "page-5",
            "summary test",
            "https://app.example",
            "s-3",
            None,
            None,
        );
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
        record_step("page-5", "open", None, None, Some(png_b64));
        record_step(
            "page-5",
            "fill",
            Some("e2"),
            Some(serde_json::json!({ "text": "hello" })),
            None,
        );
        ingest_events(
            "page-5",
            &serde_json::json!({
                "next": 4,
                "events": [
                    { "seq": 1, "kind": "net-req", "id": 1, "ts": 1000, "method": "GET",
                      "url": "https://api.example/ok", "resourceType": "xhr", "headers": {} },
                    { "seq": 2, "kind": "net-fin", "id": 1, "ts": 1100, "status": 500,
                      "headers": {}, "body": "oops" },
                    { "seq": 3, "kind": "console", "ts": 1200, "level": "error", "text": "boom" },
                    { "seq": 4, "kind": "pointer", "ts": 1250, "t": "move", "x": 5, "y": 6,
                      "vw": 100, "vh": 100 }
                ]
            }),
        );
        finish("page-5");

        let summaries = list_run_summaries(dir.path());
        assert_eq!(summaries.len(), 1);
        let s = &summaries[0];
        assert_eq!(s.name, "summary test");
        assert_eq!(s.request_count, 1);
        // One 5xx response + one error console line.
        assert_eq!(s.error_count, 2);
        // Steps keep count + frame but shed the free-text bulk.
        assert_eq!(s.steps.len(), 2);
        assert_eq!(s.steps[0].frame.as_deref(), Some("0000.png"));
        assert!(
            s.steps
                .iter()
                .all(|st| st.target.is_none() && st.detail.is_none())
        );

        // The serialized payload carries no heavy per-event arrays at all.
        let json = serde_json::to_string(s).unwrap();
        assert!(!json.contains("\"network\""), "got: {json}");
        assert!(!json.contains("\"console_events\""), "got: {json}");
        assert!(!json.contains("\"pointer_events\""), "got: {json}");
        assert!(json.contains("\"request_count\":1"), "got: {json}");
    }

    #[test]
    fn dom_chunks_round_trip_and_old_meta_has_no_dom() {
        let dir = tempfile::tempdir().unwrap();
        start(
            dir.path(),
            "page-dom",
            "dom",
            "https://example.com",
            "s",
            None,
            None,
        );
        let big = "x".repeat(300);
        ingest_dom(
            "page-dom",
            &serde_json::json!({
                "next": 2,
                "dropped": 0,
                "events": [
                    { "seq": 1, "event": { "type": 4, "timestamp": 10, "data": { "href": "https://example.com" } } },
                    { "seq": 2, "event": { "type": 3, "timestamp": 20, "data": { "text": big } } }
                ]
            }),
        );
        let id = run_id("page-dom").unwrap();
        // Same batch pulled twice (flush racing the 1s drain) must not append twice.
        ingest_dom(
            "page-dom",
            &serde_json::json!({
                "next": 2,
                "events": [{ "seq": 1, "event": { "type": 4, "data": { "text": "duplicate" } } }]
            }),
        );
        finish("page-dom");

        let full = get_run(dir.path(), &id).unwrap();
        assert!(full.has_dom);
        assert_eq!(full.dom_truncated, 0);
        let raw =
            std::fs::read_to_string(dir.path().join("browser-runs").join(&id).join("dom.jsonl"))
                .unwrap();
        assert_eq!(raw.lines().count(), 2, "replayed pull appended: {raw}");
        assert!(!raw.contains("duplicate"), "got: {raw}");
        assert!(full.dom_bytes > 0);

        // Target smaller than the second line, so the read splits.
        let c1 = super::read_dom_chunk_limited(dir.path(), &id, 0, 80, 8_000_000).unwrap();
        assert_eq!(c1.events.len(), 1);
        assert_eq!(c1.events[0]["type"], 4);
        assert!(!c1.eof);
        let c2 =
            super::read_dom_chunk_limited(dir.path(), &id, c1.next_offset, 80, 8_000_000).unwrap();
        assert_eq!(c2.events.len(), 1);
        assert_eq!(c2.events[0]["data"]["text"], big);
        assert!(c2.eof);
        let c3 =
            super::read_dom_chunk_limited(dir.path(), &id, c2.next_offset, 80, 8_000_000).unwrap();
        assert!(c3.events.is_empty());
        assert!(c3.eof);

        // A meta.json from before DOM recording still loads.
        let old = dir.path().join("browser-runs").join("oldrun1");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(
            old.join("meta.json"),
            r#"{"id":"oldrun1","name":"old","url":"https://x","session_id":"s","started_ms":1,"steps":[]}"#,
        )
        .unwrap();
        let old_meta = get_run(dir.path(), "oldrun1").unwrap();
        assert!(!old_meta.has_dom);
        assert_eq!(old_meta.dom_bytes, 0);
        let empty = read_dom_chunk(dir.path(), "oldrun1", 0).unwrap();
        assert!(empty.eof && empty.events.is_empty() && !empty.truncated);
    }

    #[test]
    fn dom_text_secrets_are_masked_and_the_cap_stops_appends() {
        let dir = tempfile::tempdir().unwrap();
        start(
            dir.path(),
            "page-dom2",
            "dom",
            "https://example.com",
            "s",
            None,
            None,
        );
        super::set_dom_cap_for_test("page-dom2", 300);
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.dBjftJeZ4CVP";
        ingest_dom(
            "page-dom2",
            &serde_json::json!({
                "next": 1,
                "events": [{ "seq": 1, "event": {
                    "type": 3,
                    "data": { "text": format!("hello Bearer abcdef123456.xyz token={jwt} card 4111 1111 1111 1111 tail") }
                }}]
            }),
        );
        // Over the 80-byte cap: counted, not written.
        ingest_dom(
            "page-dom2",
            &serde_json::json!({
                "next": 2,
                "events": [{ "seq": 2, "event": { "type": 3, "data": { "text": "yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy" } } }]
            }),
        );
        let id = run_id("page-dom2").unwrap();
        finish("page-dom2");

        let raw =
            std::fs::read_to_string(dir.path().join("browser-runs").join(&id).join("dom.jsonl"))
                .unwrap();
        assert!(!raw.contains("abcdef123456"), "got: {raw}");
        assert!(!raw.contains("dBjftJeZ4CVP"), "got: {raw}");
        assert!(!raw.contains("4111 1111"), "got: {raw}");
        assert!(raw.contains("hello"), "got: {raw}");
        assert!(raw.contains("tail"), "got: {raw}");
        assert!(!raw.contains("yyyyyyyyyyyyyyyyyyyy"));
        let parsed: serde_json::Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert!(parsed["data"]["text"].as_str().unwrap().contains("tail"));

        let meta = get_run(dir.path(), &id).unwrap();
        assert!(meta.dom_truncated >= 1);
        let chunk = read_dom_chunk(dir.path(), &id, 0).unwrap();
        assert!(chunk.truncated);
        assert_eq!(chunk.events.len(), 1);
    }

    #[test]
    fn oversized_dom_event_is_skipped_on_read() {
        let dir = tempfile::tempdir().unwrap();
        start(
            dir.path(),
            "page-dom3",
            "dom",
            "https://example.com",
            "s",
            None,
            None,
        );
        ingest_dom(
            "page-dom3",
            &serde_json::json!({
                "next": 2,
                "events": [
                    { "seq": 1, "event": { "type": 4, "data": { "text": "keep-me" } } },
                    { "seq": 2, "event": { "type": 3, "data": { "text": "0123456789abcdef0123456789abcdef" } } }
                ]
            }),
        );
        let id = run_id("page-dom3").unwrap();
        finish("page-dom3");
        // The short line is ~35 bytes; 45 keeps it and drops the long one.
        let chunk = super::read_dom_chunk_limited(dir.path(), &id, 0, 256 * 1024, 45).unwrap();
        assert!(chunk.truncated);
        assert_eq!(chunk.events.len(), 1);
        assert_eq!(chunk.events[0]["data"]["text"], "keep-me");
        assert!(chunk.eof);
    }

    #[test]
    fn half_written_dom_line_waits_for_the_next_pull() {
        let dir = tempfile::tempdir().unwrap();
        start(dir.path(), "page-dom4", "dom", "https://x", "s", None, None);
        ingest_dom(
            "page-dom4",
            &serde_json::json!({
                "next": 1,
                "events": [{ "seq": 1, "event": { "type": 4, "data": { "n": 1 } } }]
            }),
        );
        let id = run_id("page-dom4").unwrap();
        finish("page-dom4");
        let path = dir.path().join("browser-runs").join(&id).join("dom.jsonl");
        let mut raw = std::fs::read(&path).unwrap();
        raw.extend_from_slice(b"{\"type\":3,\"da");
        std::fs::write(&path, &raw).unwrap();

        let c1 = read_dom_chunk(dir.path(), &id, 0).unwrap();
        assert_eq!(c1.events.len(), 1);
        assert!(c1.eof, "a partial tail must not loop the reader");
        // The partial line is left in place, not skipped.
        raw.extend_from_slice(b"ta\":{}}\n");
        std::fs::write(&path, &raw).unwrap();
        let c2 = read_dom_chunk(dir.path(), &id, c1.next_offset).unwrap();
        assert_eq!(c2.events.len(), 1);
        assert_eq!(c2.events[0]["type"], 3);
        assert!(c2.eof);
    }
}

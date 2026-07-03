//! Browser test-run recording — the data source for the `playwright-video`
//! plugin's LogRocket-style replay. Every `browser_open` starts a run; each
//! `browser_act` appends a timestamped step with a server-side screenshot
//! frame (captured out of the agent's token budget — frames never enter
//! model context); `browser_close` finalizes.
//!
//! Layout: `data_dir/browser-runs/<run_id>/meta.json` + `NNNN.png` frames.
//! `meta.json` is rewritten after every step (small, cheap) so a crashed
//! child still leaves a replayable prefix.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

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
}

struct ActiveRun {
    dir: PathBuf,
    meta: RunMeta,
    next_frame: u32,
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
        },
        next_frame: 0,
    };
    persist(&run);
    let mut map = active().lock().unwrap_or_else(|p| p.into_inner());
    map.insert(page_id.to_string(), run);
}

/// Append a step (with an optional base64 PNG frame) to `page_id`'s run.
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
}

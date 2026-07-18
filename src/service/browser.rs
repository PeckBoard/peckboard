//! Headless-browser testing service — wraps the `better-playwright-mcp3`
//! HTTP server (github.com/livoras/better-playwright-mcp) behind the
//! `browser_*` MCP tools. The incorporated idea: compressed page outlines
//! (~91% DOM reduction with `ref=eN` element handles, list folding) plus
//! regex search over the snapshot, instead of dumping raw DOMs into agent
//! context.
//!
//! The node side is spawned lazily on first use and killed after
//! [`IDLE_SHUTDOWN_SECS`] without a call. It runs the pinned upstream
//! package through our capture sidecar (`browser_sidecar.mjs`, embedded at
//! compile time): the unmodified upstream server plus per-page
//! request/response/console capture served at `/api/pages/:id/events`,
//! which `browser_runs` ingests (masked) after every recorded step.
//! `PECKBOARD_BROWSER_URL` overrides the whole lifecycle — point it at an
//! already-running instance (this is also how tests stub the HTTP API).

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

const DEFAULT_PORT: u16 = 3111;
/// First run may `npx`-download the package, so be generous.
const SPAWN_TIMEOUT_SECS: u64 = 120;
const IDLE_SHUTDOWN_SECS: u64 = 900;
/// Page loads / actions can legitimately take a while.
const REQUEST_TIMEOUT_SECS: u64 = 90;

/// Pinned upstream package: the sidecar patches its internals (pages map,
/// express app), so an unvetted `@latest` bump must not reach it.
const UPSTREAM_PKG: &str = "better-playwright-mcp3@3.2.0";
/// Pinned alongside it: the upstream outline calls Playwright's PRIVATE
/// `page._snapshotForAI()`, which its own `^1.49.1` range no longer
/// guarantees (gone in 1.60 — fresh installs 500 on every outline). 1.55 is
/// the era the package shipped against; npm hoists it to the one copy the
/// upstream import resolves.
const PINNED_PLAYWRIGHT: &str = "playwright@1.55.0";
/// The capture sidecar source, embedded so the binary is self-contained.
const SIDECAR_SRC: &str = include_str!("browser_sidecar.mjs");

/// Materialize the sidecar into the temp dir (idempotent overwrite — cheap,
/// and keeps upgrades in sync with the binary).
fn write_sidecar() -> anyhow::Result<std::path::PathBuf> {
    let path = std::env::temp_dir().join("peckboard-browser-sidecar.mjs");
    std::fs::write(&path, SIDECAR_SRC)
        .map_err(|e| anyhow::anyhow!("failed to write browser sidecar to {path:?}: {e}"))?;
    Ok(path)
}
struct Managed {
    child: tokio::process::Child,
    base: String,
    last_used: Instant,
}

fn managed() -> &'static Mutex<Option<Managed>> {
    static M: OnceLock<Mutex<Option<Managed>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(None))
}

fn http() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .expect("reqwest client")
    })
}
#[cfg(test)]
static TEST_BASE: OnceLock<String> = OnceLock::new();

/// Test hook: route every browser call at a stub HTTP server instead of
/// spawning the real node child.
#[cfg(test)]
pub(crate) fn set_test_base_url(url: &str) {
    let _ = TEST_BASE.set(url.trim_end_matches('/').to_string());
}

/// Marker header the capture sidecar stamps on every response — how core
/// tells "our live sidecar" from an orphaned/foreign server that happens
/// to answer on the port (the classic failure: a crashed predecessor's
/// fallback child squats the port, every new sidecar dies EADDRINUSE, and
/// a naive health check silently adopts the capture-less orphan — runs
/// then record no network/console at all).
const SIDECAR_MARKER_HEADER: &str = "x-peckboard-capture";
/// How many consecutive ports to try when squatted.
const MAX_PORT_HOPS: u16 = 10;

/// Resolve the browser server's base URL, spawning the managed child if
/// needed. An explicit `PECKBOARD_BROWSER_URL` wins and disables the
/// managed lifecycle entirely.
async fn base_url() -> anyhow::Result<String> {
    #[cfg(test)]
    if let Some(u) = TEST_BASE.get() {
        return Ok(u.clone());
    }
    if let Ok(url) = std::env::var("PECKBOARD_BROWSER_URL")
        && !url.trim().is_empty()
    {
        return Ok(url.trim().trim_end_matches('/').to_string());
    }

    let mut guard = managed().lock().await;

    // Healthy child already running?
    if let Some(m) = guard.as_mut() {
        match m.child.try_wait() {
            Ok(None) => {
                m.last_used = Instant::now();
                return Ok(m.base.clone());
            }
            _ => {
                // Exited behind our back — clear and respawn below.
                *guard = None;
            }
        }
    }

    let first_port = std::env::var("PECKBOARD_BROWSER_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);
    let sidecar = write_sidecar()?;

    for hop in 0..MAX_PORT_HOPS {
        let Some(port) = first_port.checked_add(hop) else {
            break;
        };
        match spawn_on_port(&sidecar, port).await? {
            SpawnOutcome::Ready { child, capture } => {
                if !capture {
                    tracing::warn!(
                        port,
                        "browser sidecar is running WITHOUT capture (fallback mode); \
                         recorded runs will have no network/console data"
                    );
                }
                let base = format!("http://127.0.0.1:{port}");
                *guard = Some(Managed {
                    child,
                    base: base.clone(),
                    last_used: Instant::now(),
                });
                spawn_idle_reaper_once();
                return Ok(base);
            }
            SpawnOutcome::PortSquatted => {
                tracing::warn!(
                    port,
                    "a server answers on this port but is not our capture sidecar \
                     (orphaned browser server?); trying the next port"
                );
            }
        }
    }
    anyhow::bail!(
        "no usable port for the browser sidecar in {first_port}..{} — every port is \
         owned by a foreign server; kill stale `better-playwright-mcp3` processes \
         or set PECKBOARD_BROWSER_PORT",
        first_port.saturating_add(MAX_PORT_HOPS)
    )
}

enum SpawnOutcome {
    /// The child came up: with the sidecar marker (capture on), or as our
    /// own declared no-capture fallback (browsing works, recording data
    /// won't be captured).
    Ready {
        child: tokio::process::Child,
        capture: bool,
    },
    /// The port belongs to some other process — our child died EADDRINUSE
    /// behind a server that answers without the marker. Hop to the next.
    PortSquatted,
}

/// Spawn the sidecar on `port` and poll until it is verifiably OURS (marker
/// header or declared fallback), the port turns out squatted, or startup
/// fails/times out.
async fn spawn_on_port(sidecar: &std::path::Path, port: u16) -> anyhow::Result<SpawnOutcome> {
    let base = format!("http://127.0.0.1:{port}");
    tracing::info!(port, "Spawning browser server (sidecar + {UPSTREAM_PKG})");
    let mut cmd = tokio::process::Command::new("npx");
    cmd.args(["-y", "-p", UPSTREAM_PKG, "-p", PINNED_PLAYWRIGHT, "node"])
        .arg(sidecar)
        .env("PORT", port.to_string())
        .env("HEADLESS", "true")
        .env("NO_USER_PROFILE", "true")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    // Own process group so reaping kills the whole npx → node → chrome tree
    // — killing only the wrapper is exactly how capture-less orphans got
    // left behind to squat the port.
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(|e| {
        anyhow::anyhow!(
            "failed to spawn `npx {UPSTREAM_PKG}` ({e}); browser tools need \
             Node.js/npx on PATH"
        )
    })?;

    // Keep a rolling stderr tail so a failed startup has a real diagnosis.
    let stderr_tail = Arc::new(std::sync::Mutex::new(String::new()));
    if let Some(stderr) = child.stderr.take() {
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut t = tail.lock().unwrap_or_else(|p| p.into_inner());
                t.push_str(&line);
                t.push('\n');
                let len = t.len();
                if len > 4096 {
                    *t = t.split_off(len - 4096);
                }
            }
        });
    }
    let tail_now = || {
        stderr_tail
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    };

    // Health-poll until the API answers AS OURS (or the child dies / times
    // out). A success response without the marker proves nothing yet: it
    // may be a foreign server racing our child's boot.
    let deadline = Instant::now() + Duration::from_secs(SPAWN_TIMEOUT_SECS);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            let tail = tail_now();
            if tail.contains("EADDRINUSE") {
                return Ok(SpawnOutcome::PortSquatted);
            }
            anyhow::bail!("browser server exited during startup ({status}). stderr tail:\n{tail}");
        }
        if let Ok(resp) = http().get(format!("{base}/api/pages")).send().await
            && resp.status().is_success()
        {
            if resp.headers().contains_key(SIDECAR_MARKER_HEADER) {
                return Ok(SpawnOutcome::Ready {
                    child,
                    capture: true,
                });
            }
            // Marker-less answer: our own no-capture fallback announces
            // itself on stderr; anything else keeps polling until the child
            // crashes (EADDRINUSE → squatted) or the deadline calls it.
            if tail_now().contains("capture unavailable") {
                return Ok(SpawnOutcome::Ready {
                    child,
                    capture: false,
                });
            }
        }
        if Instant::now() > deadline {
            kill_group(&mut child);
            let tail = tail_now();
            anyhow::bail!(
                "browser server did not become healthy within {SPAWN_TIMEOUT_SECS}s. \
                 stderr tail:\n{tail}"
            );
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

/// Kill the managed child and (on unix) its whole process group — the tree
/// is npx → node sidecar → chrome/fallback children.
fn kill_group(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // Negative pid targets the process group created at spawn.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
}

/// One global reaper: kills the managed child after `IDLE_SHUTDOWN_SECS`
/// without a call, so an afternoon of web testing doesn't leave a headless
/// Chrome running overnight.
fn spawn_idle_reaper_once() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let mut guard = managed().lock().await;
                let idle = guard
                    .as_ref()
                    .map(|m| m.last_used.elapsed().as_secs() > IDLE_SHUTDOWN_SECS)
                    .unwrap_or(false);
                if idle && let Some(mut m) = guard.take() {
                    tracing::info!("Reaping idle browser server");
                    kill_group(&mut m.child);
                }
            }
        });
    });
}

/// POST `path` (relative, e.g. `/api/pages/p1/click`) with a JSON body and
/// return the parsed JSON response. Errors carry the server's body text.
pub(crate) async fn post(path: &str, body: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    request(reqwest::Method::POST, path, Some(body)).await
}

/// GET `path` and return the parsed JSON response.
pub(crate) async fn get(path: &str) -> anyhow::Result<serde_json::Value> {
    request(reqwest::Method::GET, path, None).await
}
/// DELETE `path`, ignoring the response body shape.
pub(crate) async fn delete(path: &str) -> anyhow::Result<serde_json::Value> {
    request(reqwest::Method::DELETE, path, None).await
}

async fn request(
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> anyhow::Result<serde_json::Value> {
    let base = base_url().await?;
    let mut req = http().request(method, format!("{base}{path}"));
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("browser server request failed: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("browser server {status}: {text}");
    }
    // Touch the idle clock only on successful calls.
    if let Some(m) = managed().lock().await.as_mut() {
        m.last_used = Instant::now();
    }
    if text.is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("browser server returned non-JSON ({e}): {text}"))
}

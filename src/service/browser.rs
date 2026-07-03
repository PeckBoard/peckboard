//! Headless-browser testing service — wraps the `better-playwright-mcp3`
//! HTTP server (github.com/livoras/better-playwright-mcp) behind the
//! `browser_*` MCP tools. The incorporated idea: compressed page outlines
//! (~91% DOM reduction with `ref=eN` element handles, list folding) plus
//! regex search over the snapshot, instead of dumping raw DOMs into agent
//! context.
//!
//! The node server is spawned lazily on first use (`npx -y
//! better-playwright-mcp3@latest server --headless --no-user-profile`) on a
//! loopback port and killed after [`IDLE_SHUTDOWN_SECS`] without a call.
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

    let port = std::env::var("PECKBOARD_BROWSER_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);
    let base = format!("http://127.0.0.1:{port}");

    tracing::info!(port, "Spawning better-playwright-mcp3 browser server");
    let mut child = tokio::process::Command::new("npx")
        .args([
            "-y",
            "better-playwright-mcp3@latest",
            "server",
            "--headless",
            "--no-user-profile",
            "--port",
            &port.to_string(),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to spawn `npx better-playwright-mcp3` ({e}); browser tools need \
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

    // Health-poll until the API answers (or the child dies / times out).
    let deadline = Instant::now() + Duration::from_secs(SPAWN_TIMEOUT_SECS);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            let tail = stderr_tail
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            anyhow::bail!("browser server exited during startup ({status}). stderr tail:\n{tail}");
        }
        if let Ok(resp) = http().get(format!("{base}/api/pages")).send().await
            && resp.status().is_success()
        {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.start_kill();
            let tail = stderr_tail
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            anyhow::bail!(
                "browser server did not become healthy within {SPAWN_TIMEOUT_SECS}s. \
                 stderr tail:\n{tail}"
            );
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    *guard = Some(Managed {
        child,
        base: base.clone(),
        last_used: Instant::now(),
    });
    spawn_idle_reaper_once();
    Ok(base)
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
                    let _ = m.child.start_kill();
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

//! Outbound WebSocket client: dials home to core `/ws/agent`, performs the
//! `hello` handshake, keeps the link alive, and dispatches inbound
//! [`ServerFrame::Request`]s to the capability executors.
//!
//! The connection is one-way to establish (agent → server) so it works
//! through firewalls with no inbound ports. On any drop it reconnects with
//! exponential backoff + jitter. [`ServerFrame::Shutdown`] ends the loop.
//!
//! Every inbound request — including one refused by the kill-switch or a
//! disabled capability — is written to the local [`AuditLog`] and mirrored
//! to the server as an `audit` event. In-flight requests carry a
//! [`CancellationToken`] keyed by `corr_id` so a [`ServerFrame::Cancel`]
//! can stop a running executor (e.g. kill a terminal child).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use tokio::sync::mpsc;
use tokio::time::{interval, sleep};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, HeaderValue};
use tokio_util::sync::CancellationToken;

use peckboard_agent_protocol::{AgentFrame, Envelope, PROTOCOL_VERSION, ServerFrame};

use crate::audit::{AuditEntry, AuditLog, Outcome};
use crate::config::Config;
use crate::executor::{ExecContext, Executors};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Buffered outbound frames (heartbeats, capability results, streamed
/// events) waiting to hit the socket. Bounded so a slow link applies
/// backpressure.
const OUTBOUND_BUFFER: usize = 256;

/// Cancellation tokens for in-flight requests, keyed by `corr_id`. Shared
/// between the frame handler (which registers/removes tokens) and the
/// cancel path (which trips them).
type CancelMap = Arc<Mutex<HashMap<String, CancellationToken>>>;

/// Why a connection attempt ended.
enum ConnOutcome {
    /// Server asked us to shut down; stop reconnecting.
    Shutdown,
    /// Link dropped (close, error, or heartbeat death); reconnect.
    Disconnected,
}

/// Run the daemon: connect, serve, and reconnect forever until the server
/// sends [`ServerFrame::Shutdown`] (or an unrecoverable config error).
pub async fn run(config: Config) -> anyhow::Result<()> {
    if config.token.is_empty() || config.server_url.is_empty() {
        anyhow::bail!(
            "not enrolled: run `peckboard-agent enroll --server <url> --token <t>` first"
        );
    }
    let config = Arc::new(config);
    let executors = Arc::new(Executors::with_defaults(config.clone()));
    let audit = Arc::new(AuditLog::new(
        Config::audit_path().unwrap_or_else(|_| std::path::PathBuf::from("audit.jsonl")),
    ));
    let mut backoff = INITIAL_BACKOFF;

    loop {
        match connect_once(config.clone(), executors.clone(), audit.clone()).await {
            Ok(ConnOutcome::Shutdown) => {
                tracing::info!("server requested shutdown; exiting");
                return Ok(());
            }
            Ok(ConnOutcome::Disconnected) => {
                tracing::warn!("disconnected; reconnecting");
                backoff = INITIAL_BACKOFF; // we were connected — reset.
            }
            Err(e) => {
                tracing::warn!(error = %e, "connection attempt failed");
            }
        }
        let delay = with_jitter(backoff);
        tracing::debug!(
            delay_ms = delay.as_millis() as u64,
            "backing off before reconnect"
        );
        sleep(delay).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Add up to +50% random jitter so a fleet of daemons doesn't reconnect in
/// lockstep after a server blip (thundering herd).
fn with_jitter(base: Duration) -> Duration {
    let extra = rand::thread_rng().gen_range(0..=(base.as_millis() / 2) as u64);
    base + Duration::from_millis(extra)
}

/// One connection lifecycle: connect, `hello`, then serve until the link
/// ends. Never returns `Err` for a normal disconnect — only for a failed
/// dial/handshake.
async fn connect_once(
    config: Arc<Config>,
    executors: Arc<Executors>,
    audit: Arc<AuditLog>,
) -> anyhow::Result<ConnOutcome> {
    let url = config.ws_url();
    let mut request = url
        .as_str()
        .into_client_request()
        .with_context(|| format!("building WS request for {url}"))?;
    request.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", config.token))
            .context("enrollment token is not a valid header value")?,
    );

    let (ws, _resp) = connect_async(request)
        .await
        .with_context(|| format!("dialing {url}"))?;
    tracing::info!(url = %url, "connected to server");
    let (mut sink, mut stream) = ws.split();

    // hello: identify ourselves + advertise the capabilities we'll serve.
    let hello = AgentFrame::Hello {
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        platform: std::env::consts::OS.to_string(),
        hostname: gethostname::gethostname().to_string_lossy().into_owned(),
        capabilities: config.enabled_capabilities(),
        features: vec![peckboard_agent_protocol::FEATURE_WINDOW_TARGETS.to_string()],
    };
    sink.send(Message::Text(encode(&hello).into())).await?;

    // Single writer owns the sink; heartbeat + per-request response tasks +
    // streamed events funnel frames through this channel so writes never
    // race. The channel carries `AgentFrame`s and the writer encodes them.
    let (tx, mut rx) = mpsc::channel::<AgentFrame>(OUTBOUND_BUFFER);
    let cancels: CancelMap = Arc::new(Mutex::new(HashMap::new()));

    let hb_tx = tx.clone();
    let heartbeat = tokio::spawn(async move {
        let mut ticker = interval(HEARTBEAT_INTERVAL);
        ticker.tick().await; // fires immediately; skip it.
        loop {
            ticker.tick().await;
            if hb_tx.send(AgentFrame::Heartbeat).await.is_err() {
                break; // writer gone.
            }
        }
    });

    let outcome = loop {
        tokio::select! {
            Some(frame) = rx.recv() => {
                if sink.send(Message::Text(encode(&frame).into())).await.is_err() {
                    break ConnOutcome::Disconnected;
                }
            }
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if let Some(o) = handle_text(text.as_str(), &config, &executors, &audit, &tx, &cancels) {
                            break o;
                        }
                    }
                    // tungstenite auto-pongs pings; nothing to do here.
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => break ConnOutcome::Disconnected,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "socket error");
                        break ConnOutcome::Disconnected;
                    }
                }
            }
        }
    };

    heartbeat.abort();
    // Trip any still-in-flight cancel tokens so spawned executors wind down.
    for (_, token) in cancels.lock().unwrap().drain() {
        token.cancel();
    }
    Ok(outcome)
}

/// Handle one decoded server frame. Returns `Some(ConnOutcome)` to end the
/// connection (only on shutdown), `None` to keep serving. Capability work
/// is spawned so a slow executor can't stall heartbeats or other requests.
fn handle_text(
    text: &str,
    config: &Arc<Config>,
    executors: &Arc<Executors>,
    audit: &Arc<AuditLog>,
    tx: &mpsc::Sender<AgentFrame>,
    cancels: &CancelMap,
) -> Option<ConnOutcome> {
    let env = match serde_json::from_str::<Envelope<ServerFrame>>(text) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(error = %e, "unparseable server frame ignored");
            return None;
        }
    };
    if env.v != PROTOCOL_VERSION {
        tracing::warn!(
            v = env.v,
            "server frame with mismatched protocol version ignored"
        );
        return None;
    }

    match env.frame {
        ServerFrame::Ping => {
            let _ = tx.try_send(AgentFrame::Heartbeat);
            None
        }
        ServerFrame::Shutdown => Some(ConnOutcome::Shutdown),
        ServerFrame::Cancel { corr_id } => {
            // Trip the in-flight token if we still have it; the executor
            // observes cancellation and unwinds.
            if let Some(token) = cancels.lock().unwrap().get(&corr_id) {
                token.cancel();
                tracing::debug!(%corr_id, "cancel signalled");
            } else {
                tracing::debug!(%corr_id, "cancel for unknown/finished request ignored");
            }
            None
        }
        ServerFrame::Request {
            corr_id,
            capability,
            args,
        } => {
            let config = config.clone();
            let executors = executors.clone();
            let audit = audit.clone();
            let tx = tx.clone();
            let cancels = cancels.clone();
            tokio::spawn(async move {
                serve_request(
                    config, executors, audit, tx, cancels, corr_id, capability, args,
                )
                .await;
            });
            None
        }
    }
}

/// Run one request end to end: enforce gating, dispatch, audit the outcome,
/// mirror an `audit` event, and reply with a [`AgentFrame::Result`].
#[allow(clippy::too_many_arguments)]
async fn serve_request(
    config: Arc<Config>,
    executors: Arc<Executors>,
    audit: Arc<AuditLog>,
    tx: mpsc::Sender<AgentFrame>,
    cancels: CancelMap,
    corr_id: String,
    capability: String,
    args: serde_json::Value,
) {
    // Gate first: the kill-switch and per-capability flag are enforced (and
    // audited as `refused`) before an executor ever runs. Re-read the
    // on-disk config so a freshly flipped kill-switch or capability toggle
    // applies to the NEXT request — not only after a daemon restart (the
    // input executors do the same per-action live re-check); a transient
    // read failure falls back to the startup snapshot.
    let gate_config = Config::load().unwrap_or_else(|_| (*config).clone());
    if !gate_config.is_enabled(&capability) {
        tracing::warn!(%capability, "refused: capability disabled");
        let err = "capability disabled".to_string();
        record(
            &audit,
            &tx,
            &corr_id,
            &capability,
            Outcome::Refused,
            Some(err.clone()),
        )
        .await;
        let _ = tx
            .send(AgentFrame::Result {
                corr_id,
                ok: false,
                payload: None,
                error: Some(err),
            })
            .await;
        return;
    }

    // Register a cancel token for the lifetime of the dispatch.
    let cancel = CancellationToken::new();
    cancels
        .lock()
        .unwrap()
        .insert(corr_id.clone(), cancel.clone());
    let ctx = ExecContext {
        corr_id: corr_id.clone(),
        events: tx.clone(),
        cancel,
    };

    let result = executors.dispatch(&ctx, &capability, args).await;
    cancels.lock().unwrap().remove(&corr_id);

    let frame = match &result {
        Ok(payload) => {
            record(&audit, &tx, &corr_id, &capability, Outcome::Ok, None).await;
            AgentFrame::Result {
                corr_id: corr_id.clone(),
                ok: true,
                payload: Some(payload.clone()),
                error: None,
            }
        }
        Err(error) => {
            record(
                &audit,
                &tx,
                &corr_id,
                &capability,
                Outcome::Error,
                Some(error.clone()),
            )
            .await;
            AgentFrame::Result {
                corr_id: corr_id.clone(),
                ok: false,
                payload: None,
                error: Some(error.clone()),
            }
        }
    };
    let _ = tx.send(frame).await;
}

/// Append an audit entry locally and mirror it to the server as an `audit`
/// event.
async fn record(
    audit: &AuditLog,
    tx: &mpsc::Sender<AgentFrame>,
    corr_id: &str,
    capability: &str,
    outcome: Outcome,
    error: Option<String>,
) {
    let entry = AuditEntry::now(corr_id, capability, outcome, error);
    audit.append(&entry);
    let _ = tx
        .send(AgentFrame::Event {
            kind: "audit".to_string(),
            data: entry.to_value(),
        })
        .await;
}

/// Serialize a frame into a versioned envelope JSON string.
fn encode(frame: &AgentFrame) -> String {
    // Frames are plain serde structs — serialization cannot realistically
    // fail; fall back to an empty object rather than panicking the daemon.
    serde_json::to_string(&Envelope::new(frame.clone())).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_stamps_version_and_tag() {
        let s = encode(&AgentFrame::Heartbeat);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["v"], PROTOCOL_VERSION);
        assert_eq!(v["type"], "heartbeat");
    }

    #[test]
    fn with_jitter_stays_within_bounds() {
        let base = Duration::from_secs(4);
        for _ in 0..100 {
            let d = with_jitter(base);
            assert!(d >= base);
            assert!(d <= base + Duration::from_secs(2)); // +50% max
        }
    }
}

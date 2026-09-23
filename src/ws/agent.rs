//! `/ws/agent` — the outbound-dial WebSocket a `peckboard-agent` daemon
//! holds against this server.
//!
//! The daemon connects OUT to us (firewall-friendly, no inbound ports on the
//! user's machine) and authenticates with its enrollment token. The token is
//! verified BEFORE the upgrade completes — a bad credential costs one 401 and
//! never allocates a socket. Only the SHA-256 hex of the token is ever
//! compared (`devices.secret_hash`); the plaintext is never stored or logged.
//!
//! Daemons send no `Origin` header, so `origin_check` (src/security.rs)
//! passes these requests untouched — no carve-out needed.
//!
//! After the upgrade the daemon must open with a wire-protocol `hello` frame
//! ([`peckboard_agent_protocol::AgentFrame::Hello`]) within [`HELLO_TIMEOUT`].
//! The socket then runs the same liveness cadence as `/ws` and
//! `/ws/plugin-ui` (30s Ping / 90s timeout) plus a 10s re-check that the
//! device row still exists and is `active`. Revoking or disabling a device
//! severs its socket immediately — the device routes call
//! [`DeviceRegistry::sever`] — with the 10s poll as the backstop, mirroring
//! the `get_auth_session` re-check on user sockets.
//!
//! [`DeviceRegistry`] maps device_id → live connection (outbound frame
//! sender + close handle) and carries the correlation-id request/response
//! bridge: [`DeviceRegistry::send_request`] allocates a corr_id, queues a
//! [`ServerFrame::Request`], and awaits the daemon's matching
//! [`AgentFrame::Result`] (routed back by [`DeviceRegistry::handle_frame`])
//! under the caller's deadline. Pending requests fail fast when the
//! connection drops, and every in-flight-count change is broadcast as a
//! `device-update` event so the Agents panel can render live activity.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use peckboard_agent_protocol::{AgentFrame, Envelope, PROTOCOL_VERSION, ServerFrame};
use serde_json::Value;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::Instant as TokioInstant;

use crate::db::models::{Device, device_status};
use crate::state::AppState;
use crate::ws::broadcaster::{Broadcaster, WsEvent};

/// Same cadence as `/ws` and `/ws/plugin-ui`: ping idle sockets, drop
/// half-open ones.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(90);

/// How often the socket re-checks that its device row is still `active`.
/// Revoke/disable severs the connection within this window.
const DEVICE_CHECK_INTERVAL: Duration = Duration::from_secs(10);

/// How long a freshly upgraded socket may sit silent before its `hello`
/// arrives. Generous for a slow link, short enough that a port-scanner
/// holding sockets open gets dropped quickly.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// Outbound-frame buffer per device connection. Requests queue here between
/// the registry and the socket writer.
const OUTBOUND_BUFFER: usize = 64;

// ── DeviceRegistry + correlation-id request/response bridge ────────────────

/// corr_id → reply channel for requests awaiting an [`AgentFrame::Result`].
/// A reply is `Ok(result payload)` or `Err(agent-reported error)`; dropping
/// a sender resolves its caller with [`RequestError::Disconnected`].
type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>>;

/// Why a [`DeviceRegistry::send_request`] call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestError {
    /// The device has no live connection.
    Offline,
    /// The connection dropped (or was superseded) before the reply arrived.
    Disconnected,
    /// No reply within the caller's deadline; a best-effort
    /// [`ServerFrame::Cancel`] was queued for the daemon.
    Timeout,
    /// The daemon replied `ok: false`.
    Agent(String),
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestError::Offline => write!(f, "device is offline"),
            RequestError::Disconnected => write!(f, "device disconnected before replying"),
            RequestError::Timeout => write!(f, "request timed out"),
            RequestError::Agent(e) => write!(f, "agent error: {e}"),
        }
    }
}

impl std::error::Error for RequestError {}
struct DeviceConn {
    /// Monotonic per-process id: lets a stale socket's cleanup avoid
    /// removing the entry of the connection that replaced it.
    conn_id: u64,
    tx: mpsc::Sender<ServerFrame>,
    close: Arc<Notify>,
    /// This connection's in-flight requests. Per-connection (not
    /// per-device) so a superseded socket fails exactly its own callers.
    pending: PendingMap,
    /// Optional behaviours the daemon advertised in its `hello`
    /// (`peckboard_agent_protocol::FEATURE_*`).
    features: Vec<String>,
}

/// Live agent connections, device_id → socket handle. One connection per
/// device: a reconnect supersedes (and severs) the previous socket.
#[derive(Default)]
pub struct DeviceRegistry {
    inner: Mutex<HashMap<String, DeviceConn>>,
    next_conn_id: AtomicU64,
}

/// What a socket task gets back from [`DeviceRegistry::connect`].
pub struct DeviceConnection {
    pub conn_id: u64,
    /// Frames the server wants sent down this device's socket.
    pub outbound: mpsc::Receiver<ServerFrame>,
    /// Fired when a newer connection for the same device supersedes this one.
    pub close: Arc<Notify>,
}

impl DeviceRegistry {
    /// Register a new live connection for `device_id`, severing any previous
    /// one (its `close` handle fires, its outbound channel drops, and its
    /// pending requests fail with [`RequestError::Disconnected`]).
    pub fn connect(&self, device_id: &str) -> DeviceConnection {
        self.connect_with_features(device_id, Vec::new())
    }

    /// [`Self::connect`], recording the daemon's advertised `features` so
    /// callers can refuse requests an older agent would misinterpret.
    pub fn connect_with_features(
        &self,
        device_id: &str,
        features: Vec<String>,
    ) -> DeviceConnection {
        let conn_id = self.next_conn_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = mpsc::channel(OUTBOUND_BUFFER);
        let close = Arc::new(Notify::new());
        let old = self.inner.lock().unwrap().insert(
            device_id.to_string(),
            DeviceConn {
                conn_id,
                tx,
                close: close.clone(),
                pending: PendingMap::default(),
                features,
            },
        );
        if let Some(old) = old {
            // notify_one stores a permit, so the old socket task picks this
            // up even if it isn't parked on `notified()` right now.
            old.close.notify_one();
            // Clearing the map drops the reply senders; each waiting
            // caller's receiver resolves to `RequestError::Disconnected`.
            old.pending.lock().unwrap().clear();
        }
        DeviceConnection {
            conn_id,
            outbound: rx,
            close,
        }
    }

    /// Remove `device_id`'s entry — but only if it still belongs to this
    /// `conn_id`. A socket that was superseded must not evict its successor.
    /// Removal fails all of the connection's pending requests.
    pub fn disconnect(&self, device_id: &str, conn_id: u64) -> bool {
        let removed = {
            let mut inner = self.inner.lock().unwrap();
            if inner.get(device_id).is_some_and(|c| c.conn_id == conn_id) {
                inner.remove(device_id)
            } else {
                None
            }
        };
        match removed {
            Some(conn) => {
                // Dropping the reply senders resolves every waiting caller
                // with `RequestError::Disconnected`.
                conn.pending.lock().unwrap().clear();
                true
            }
            None => false,
        }
    }

    /// Immediately sever `device_id`'s live connection, if any. Called by
    /// the device routes on revoke/disable so the status change cuts the
    /// socket NOW; the socket task's 10s DB re-check stays as the backstop.
    /// Removing the entry drops the outbound sender (ending the socket
    /// task's `outbound.recv()`), the `close` permit wakes a parked task,
    /// and clearing `pending` fails every in-flight request with
    /// [`RequestError::Disconnected`].
    pub fn sever(&self, device_id: &str) -> bool {
        let removed = self.inner.lock().unwrap().remove(device_id);
        match removed {
            Some(conn) => {
                conn.close.notify_one();
                conn.pending.lock().unwrap().clear();
                true
            }
            None => false,
        }
    }

    pub fn is_online(&self, device_id: &str) -> bool {
        self.inner.lock().unwrap().contains_key(device_id)
    }
    /// Whether `device_id`'s live connection advertised `feature`. `false`
    /// when offline.
    pub fn has_feature(&self, device_id: &str, feature: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .get(device_id)
            .is_some_and(|c| c.features.iter().any(|f| f == feature))
    }

    /// How many requests are currently awaiting a reply from `device_id`.
    /// `0` when the device is offline.
    pub fn in_flight(&self, device_id: &str) -> usize {
        self.inner
            .lock()
            .unwrap()
            .get(device_id)
            .map_or(0, |c| c.pending.lock().unwrap().len())
    }

    /// Announce the device's current online/in-flight snapshot so the
    /// Agents panel tracks live activity. Also called by the device routes
    /// after a rename / status change so open panels refetch.
    pub fn broadcast_state(&self, events: &Broadcaster, device_id: &str) {
        let in_flight = {
            let inner = self.inner.lock().unwrap();
            inner
                .get(device_id)
                .map(|c| c.pending.lock().unwrap().len())
        };
        events.broadcast(device_update_event(
            device_id,
            in_flight.is_some(),
            in_flight.unwrap_or(0),
        ));
    }
    /// Queue a frame for the device's socket. `false` if the device is
    /// offline or its socket task has gone away.
    pub async fn send(&self, device_id: &str, frame: ServerFrame) -> bool {
        let tx = self
            .inner
            .lock()
            .unwrap()
            .get(device_id)
            .map(|c| c.tx.clone());
        match tx {
            Some(tx) => tx.send(frame).await.is_ok(),
            None => false,
        }
    }

    /// Inbound frames from a device land here. `Result` frames resolve the
    /// pending request with the matching `corr_id`; anything else is
    /// surfaced in logs only (unsolicited events land with a later card).
    pub fn handle_frame(&self, events: &Broadcaster, device_id: &str, frame: AgentFrame) {
        match frame {
            AgentFrame::Heartbeat => {}
            AgentFrame::Result {
                corr_id,
                ok,
                payload,
                error,
            } => {
                let waiter = self
                    .inner
                    .lock()
                    .unwrap()
                    .get(device_id)
                    .and_then(|c| c.pending.lock().unwrap().remove(&corr_id));
                match waiter {
                    Some(reply) => {
                        let outcome = if ok {
                            Ok(payload.unwrap_or(Value::Null))
                        } else {
                            Err(error.unwrap_or_else(|| "agent reported failure".to_string()))
                        };
                        // The caller may have timed out and gone away.
                        let _ = reply.send(outcome);
                        self.broadcast_state(events, device_id);
                    }
                    None => {
                        tracing::debug!(
                            device_id,
                            %corr_id,
                            "result frame with no pending request ignored"
                        );
                    }
                }
            }
            other => {
                tracing::debug!(device_id, frame = ?other, "unsolicited agent frame ignored");
            }
        }
    }

    /// Run `capability` on the device and await its reply.
    ///
    /// Allocates a correlation id, queues a [`ServerFrame::Request`], and
    /// resolves when the daemon's matching [`AgentFrame::Result`] arrives
    /// (routed here by [`DeviceRegistry::handle_frame`]) — or fails on an
    /// offline device, a dropped connection, an agent-reported error, or
    /// `timeout` (which withdraws the pending entry and queues a
    /// best-effort cancel). Every in-flight-count change is broadcast as a
    /// `device-update` event.
    pub async fn send_request(
        &self,
        events: &Broadcaster,
        device_id: &str,
        capability: &str,
        args: Value,
        timeout: Duration,
    ) -> Result<Value, RequestError> {
        let (tx, pending) = {
            let inner = self.inner.lock().unwrap();
            match inner.get(device_id) {
                Some(c) => (c.tx.clone(), c.pending.clone()),
                None => return Err(RequestError::Offline),
            }
        };

        let corr_id = uuid::Uuid::new_v4().to_string();
        let (reply_tx, reply_rx) = oneshot::channel();
        pending.lock().unwrap().insert(corr_id.clone(), reply_tx);
        self.broadcast_state(events, device_id);

        let request = ServerFrame::Request {
            corr_id: corr_id.clone(),
            capability: capability.to_string(),
            args,
        };
        if tx.send(request).await.is_err() {
            // Socket task gone; withdraw the slot before reporting.
            pending.lock().unwrap().remove(&corr_id);
            self.broadcast_state(events, device_id);
            return Err(RequestError::Disconnected);
        }

        match tokio::time::timeout(timeout, reply_rx).await {
            Ok(Ok(Ok(payload))) => Ok(payload),
            Ok(Ok(Err(msg))) => Err(RequestError::Agent(msg)),
            // Reply sender dropped: the connection was severed or
            // superseded and `disconnect`/`connect` cleared the map.
            Ok(Err(_)) => Err(RequestError::Disconnected),
            Err(_) => {
                pending.lock().unwrap().remove(&corr_id);
                self.broadcast_state(events, device_id);
                // Best-effort: tell the daemon to stop working on it.
                let _ = tx.try_send(ServerFrame::Cancel { corr_id });
                Err(RequestError::Timeout)
            }
        }
    }
}

// ── Handler ────────────────────────────────────────────────────────────────

/// The enrollment token, from `Authorization: Bearer …` (preferred) or a
/// `Sec-WebSocket-Protocol` entry of the form `token.<t>` — for browser WS
/// clients, which cannot set headers (they offer
/// `['peckboard-agent', 'token.<t>']` and [`agent_ws_handler`] selects the
/// static `peckboard-agent` entry in the reply). Never the URL: a query
/// parameter would put the long-lived device credential in every proxy /
/// access log that records request lines.
fn presented_token(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get(axum::http::header::AUTHORIZATION)
        && let Ok(s) = v.to_str()
        && let Some(rest) = s.strip_prefix("Bearer ")
        && !rest.trim().is_empty()
    {
        return Some(rest.trim().to_string());
    }
    let protocols = headers
        .get(axum::http::header::SEC_WEBSOCKET_PROTOCOL)?
        .to_str()
        .ok()?;
    protocols
        .split(',')
        .map(str::trim)
        .find_map(|p| p.strip_prefix("token."))
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

fn sha256_hex(s: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(s.as_bytes()))
}

/// Token → active device, or nothing. One generic failure path: callers
/// can't distinguish "unknown token" from "revoked device" (no oracle).
async fn authenticate(state: &AppState, headers: &HeaderMap) -> Option<Device> {
    let token = presented_token(headers)?;
    let device = state
        .db
        .get_device_by_secret_hash(&sha256_hex(&token))
        .await
        .ok()
        .flatten()?;
    (device.status == device_status::ACTIVE).then_some(device)
}

/// Upgrade handler. The token is verified BEFORE the upgrade completes, so a
/// bad credential costs one 401 and never allocates a socket.
pub async fn agent_ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Response {
    let Some(device) = authenticate(&state, &headers).await else {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({ "error": "invalid enrollment token" })),
        )
            .into_response();
    };
    // Select the static subprotocol when the client offered one — a browser
    // WebSocket aborts if its offered protocol list goes unanswered. The
    // `token.<t>` entry is deliberately never echoed back.
    ws.protocols(["peckboard-agent"])
        .on_upgrade(move |socket| handle_agent_socket(socket, state, device))
        .into_response()
}

fn device_update_event(device_id: &str, online: bool, in_flight: usize) -> WsEvent {
    WsEvent {
        event_type: "device-update".to_string(),
        session_id: String::new(),
        data: serde_json::json!({
            "device_id": device_id,
            "online": online,
            "in_flight": in_flight,
        }),
    }
}

async fn handle_agent_socket(socket: WebSocket, state: Arc<AppState>, device: Device) {
    let (mut sender, mut receiver) = socket.split();

    // First frame must be a versioned `hello` — anything else (or silence)
    // drops the socket before it costs a registry slot.
    let hello_text = tokio::time::timeout(HELLO_TIMEOUT, async {
        while let Some(msg) = receiver.next().await {
            match msg {
                Ok(Message::Text(t)) => return Some(t),
                Ok(Message::Close(_)) | Err(_) => return None,
                Ok(_) => continue,
            }
        }
        None
    })
    .await
    .ok()
    .flatten();

    let hello = hello_text
        .as_deref()
        .and_then(|t| serde_json::from_str::<Envelope<AgentFrame>>(t).ok());
    let (capabilities, features) = match hello {
        Some(env) if env.v == PROTOCOL_VERSION => match env.frame {
            AgentFrame::Hello {
                capabilities,
                features,
                ..
            } => (capabilities, features),
            _ => {
                close_with(&mut sender, 4400, "expected hello frame").await;
                return;
            }
        },
        Some(_) => {
            close_with(&mut sender, 4400, "protocol version mismatch").await;
            return;
        }
        None => {
            close_with(&mut sender, 4400, "expected hello frame").await;
            return;
        }
    };
    tracing::info!(
        device_id = %device.id,
        capabilities = ?capabilities,
        features = ?features,
        "agent connected"
    );

    let conn = state
        .device_registry
        .connect_with_features(&device.id, features);
    let conn_id = conn.conn_id;
    let close = conn.close;
    let mut outbound = conn.outbound;
    let _ = state
        .db
        .update_device_last_seen(&device.id, &Utc::now().to_rfc3339())
        .await;
    state
        .broadcaster
        .broadcast(device_update_event(&device.id, true, 0));

    // Same shape as `/ws/plugin-ui`: revocation re-check + half-open
    // detection, plus this socket's outbound frame queue.
    let mut device_check = tokio::time::interval(DEVICE_CHECK_INTERVAL);
    device_check.tick().await;
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.tick().await;
    let mut last_seen = TokioInstant::now();

    loop {
        tokio::select! {
            _ = close.notified() => {
                close_with(&mut sender, 4000, "superseded or severed by the server").await;
                break;
            }
            _ = device_check.tick() => {
                let active = state
                    .db
                    .get_device(&device.id)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|d| d.status == device_status::ACTIVE);
                if !active {
                    close_with(&mut sender, 4001, "device revoked or disabled").await;
                    break;
                }
                let _ = state
                    .db
                    .update_device_last_seen(&device.id, &Utc::now().to_rfc3339())
                    .await;
            }
            _ = heartbeat.tick() => {
                if last_seen.elapsed() > HEARTBEAT_TIMEOUT {
                    break;
                }
                if sender.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
            msg = receiver.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        last_seen = TokioInstant::now();
                        match serde_json::from_str::<Envelope<AgentFrame>>(&text) {
                            Ok(env) if env.v == PROTOCOL_VERSION => {
                                state.device_registry.handle_frame(&state.broadcaster, &device.id, env.frame);
                            }
                            Ok(env) => {
                                tracing::warn!(device_id = %device.id, v = env.v, "agent frame with mismatched protocol version ignored");
                            }
                            Err(e) => {
                                tracing::debug!(device_id = %device.id, error = %e, "unparseable agent frame ignored");
                            }
                        }
                    }
                    // Pongs (tungstenite auto-answers our Pings) and any
                    // other frame count as liveness.
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => { last_seen = TokioInstant::now(); }
                    Some(Err(_)) => break,
                }
            }
            frame = outbound.recv() => {
                match frame {
                    Some(f) => {
                        let text = match serde_json::to_string(&Envelope::new(f)) {
                            Ok(t) => t,
                            Err(_) => continue,
                        };
                        if sender.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    // Registry entry dropped out from under us.
                    None => break,
                }
            }
        }
    }

    state.device_registry.disconnect(&device.id, conn_id);
    let _ = state
        .db
        .update_device_last_seen(&device.id, &Utc::now().to_rfc3339())
        .await;
    state
        .broadcaster
        .broadcast(device_update_event(&device.id, false, 0));
    tracing::info!(device_id = %device.id, "agent disconnected");
}

async fn close_with(sender: &mut (impl SinkExt<Message> + Unpin), code: u16, reason: &'static str) {
    let _ = sender
        .send(Message::Close(Some(axum::extract::ws::CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::middleware::tests::test_state;
    use crate::db::models::NewDevice;
    use axum::{Router, routing::get};

    #[test]
    fn presented_token_prefers_bearer_header_over_subprotocol() {
        let mut headers = HeaderMap::new();
        assert_eq!(presented_token(&headers), None);
        headers.insert(
            axum::http::header::SEC_WEBSOCKET_PROTOCOL,
            "peckboard-agent, token.ptok".parse().unwrap(),
        );
        assert_eq!(presented_token(&headers), Some("ptok".into()));
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer htok".parse().unwrap(),
        );
        assert_eq!(presented_token(&headers), Some("htok".into()));
    }

    #[tokio::test]
    async fn a_reconnect_supersedes_and_cleanup_is_conn_id_guarded() {
        let reg = DeviceRegistry::default();
        let first = reg.connect("d1");
        let second = reg.connect("d1");
        assert_ne!(first.conn_id, second.conn_id);
        // The first connection was told to close (permit survives even
        // though nobody was awaiting at notify time).
        tokio::time::timeout(Duration::from_secs(1), first.close.notified())
            .await
            .expect("superseded connection must be notified");
        // The stale socket's cleanup must not evict its successor.
        assert!(!reg.disconnect("d1", first.conn_id));
        assert!(reg.is_online("d1"));
        assert!(reg.disconnect("d1", second.conn_id));
        assert!(!reg.is_online("d1"));
    }

    #[tokio::test]
    async fn send_request_round_trips_and_tracks_in_flight() {
        let reg = Arc::new(DeviceRegistry::default());
        let events = Broadcaster::new();
        let mut event_rx = events.subscribe_all();
        let mut conn = reg.connect("d1");

        assert_eq!(
            reg.send_request(&events, "nope", "echo", Value::Null, Duration::from_secs(1))
                .await,
            Err(RequestError::Offline)
        );

        let reg2 = Arc::clone(&reg);
        let events2 = Arc::clone(&events);
        let call = tokio::spawn(async move {
            reg2.send_request(
                &events2,
                "d1",
                "echo",
                serde_json::json!({ "msg": "hi" }),
                Duration::from_secs(5),
            )
            .await
        });

        // Fake daemon: pop the request off this connection's outbound queue…
        let frame = tokio::time::timeout(Duration::from_secs(5), conn.outbound.recv())
            .await
            .unwrap()
            .unwrap();
        let ServerFrame::Request {
            corr_id,
            capability,
            args,
        } = frame
        else {
            panic!("expected a request frame");
        };
        assert_eq!(capability, "echo");
        assert_eq!(args, serde_json::json!({ "msg": "hi" }));
        assert_eq!(next_in_flight(&mut event_rx, "d1").await, 1);
        assert_eq!(reg.in_flight("d1"), 1);

        // A result for an unknown corr_id is ignored without disturbing
        // the pending request.
        reg.handle_frame(
            &events,
            "d1",
            AgentFrame::Result {
                corr_id: "bogus".into(),
                ok: true,
                payload: None,
                error: None,
            },
        );
        assert_eq!(reg.in_flight("d1"), 1);

        // …and answer it.
        reg.handle_frame(
            &events,
            "d1",
            AgentFrame::Result {
                corr_id,
                ok: true,
                payload: Some(serde_json::json!({ "echo": "hi" })),
                error: None,
            },
        );
        assert_eq!(call.await.unwrap(), Ok(serde_json::json!({ "echo": "hi" })));
        assert_eq!(next_in_flight(&mut event_rx, "d1").await, 0);
        assert_eq!(reg.in_flight("d1"), 0);
    }

    #[tokio::test]
    async fn disconnect_fails_pending_requests() {
        let reg = Arc::new(DeviceRegistry::default());
        let events = Broadcaster::new();
        let mut conn = reg.connect("d1");

        let reg2 = Arc::clone(&reg);
        let events2 = Arc::clone(&events);
        let call = tokio::spawn(async move {
            reg2.send_request(&events2, "d1", "echo", Value::Null, Duration::from_secs(30))
                .await
        });
        // Wait until the request is actually in flight.
        tokio::time::timeout(Duration::from_secs(5), conn.outbound.recv())
            .await
            .unwrap()
            .unwrap();

        assert!(reg.disconnect("d1", conn.conn_id));
        assert_eq!(call.await.unwrap(), Err(RequestError::Disconnected));
        assert_eq!(reg.in_flight("d1"), 0);
    }

    #[tokio::test]
    async fn sever_cuts_connection_and_fails_pending() {
        let reg = Arc::new(DeviceRegistry::default());
        let events = Broadcaster::new();
        let mut conn = reg.connect("d1");

        let reg2 = Arc::clone(&reg);
        let events2 = Arc::clone(&events);
        let call = tokio::spawn(async move {
            reg2.send_request(&events2, "d1", "echo", Value::Null, Duration::from_secs(5))
                .await
        });
        // Wait until the request frame is queued so it's genuinely pending.
        tokio::time::timeout(Duration::from_secs(5), conn.outbound.recv())
            .await
            .expect("request frame")
            .expect("channel open");

        assert!(reg.sever("d1"));
        assert!(!reg.is_online("d1"));
        assert_eq!(call.await.unwrap(), Err(RequestError::Disconnected));
        // A parked socket task wakes via the stored close permit…
        tokio::time::timeout(Duration::from_secs(1), conn.close.notified())
            .await
            .expect("severed connection must be notified");
        // …and its outbound queue ends because the sender is gone.
        assert!(conn.outbound.recv().await.is_none());
        assert!(!reg.sever("d1"));
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_withdraws_pending_and_sends_cancel() {
        let reg = Arc::new(DeviceRegistry::default());
        let events = Broadcaster::new();
        let mut conn = reg.connect("d1");

        let reg2 = Arc::clone(&reg);
        let events2 = Arc::clone(&events);
        let call = tokio::spawn(async move {
            reg2.send_request(
                &events2,
                "d1",
                "echo",
                Value::Null,
                Duration::from_millis(100),
            )
            .await
        });
        let frame = tokio::time::timeout(Duration::from_secs(5), conn.outbound.recv())
            .await
            .unwrap()
            .unwrap();
        let ServerFrame::Request { corr_id, .. } = frame else {
            panic!("expected a request frame");
        };

        // No reply arrives: virtual time races past the deadline.
        assert_eq!(call.await.unwrap(), Err(RequestError::Timeout));
        assert_eq!(reg.in_flight("d1"), 0);
        let cancel = conn.outbound.recv().await.unwrap();
        assert_eq!(cancel, ServerFrame::Cancel { corr_id });
    }

    async fn next_in_flight(
        events: &mut tokio::sync::broadcast::Receiver<WsEvent>,
        device_id: &str,
    ) -> u64 {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let ev = events.recv().await.expect("broadcast channel open");
                if ev.event_type == "device-update"
                    && ev.data.get("device_id").and_then(|v| v.as_str()) == Some(device_id)
                {
                    return ev.data.get("in_flight").and_then(|v| v.as_u64()).unwrap();
                }
            }
        })
        .await
        .expect("device-update event")
    }
    async fn insert_active_device(state: &AppState, id: &str, token: &str) {
        state
            .db
            .insert_device(NewDevice {
                id: id.to_string(),
                user_id: "u1".to_string(),
                name: "test box".to_string(),
                platform: "linux".to_string(),
                secret_hash: sha256_hex(token),
                status: device_status::ACTIVE.to_string(),
                last_seen_at: None,
                created_at: Utc::now().to_rfc3339(),
            })
            .await
            .unwrap();
    }

    async fn spawn_agent_ws_server(state: Arc<AppState>) -> std::net::SocketAddr {
        let app = Router::new()
            .route("/ws/agent", get(agent_ws_handler))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    /// The handshake itself is refused with 401 — the upgrade never
    /// completes, so a bad credential never allocates a socket. (Tested
    /// over a real connection: axum's `WebSocketUpgrade` extractor needs
    /// hyper's `OnUpgrade` extension, which `tower::oneshot` can't fake.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bad_token_gets_401_before_any_socket_exists() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        insert_active_device(&state, "d1", "good-token").await;
        let addr = spawn_agent_ws_server(state.clone()).await;

        let assert_401 = |token: Option<&'static str>| async move {
            use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
            let mut req = format!("ws://{addr}/ws/agent")
                .into_client_request()
                .unwrap();
            if let Some(t) = token {
                req.headers_mut()
                    .insert("authorization", format!("Bearer {t}").parse().unwrap());
            }
            match tokio_tungstenite::connect_async(req).await {
                Err(tokio_tungstenite::tungstenite::Error::Http(res)) => {
                    assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "token {token:?}");
                }
                other => panic!("expected 401 handshake rejection for {token:?}, got {other:?}"),
            }
        };
        assert_401(None).await;
        assert_401(Some("wrong")).await;

        // A disabled device's still-valid token is refused identically —
        // no oracle distinguishing "unknown token" from "kill-switched".
        state
            .db
            .update_device_status("d1", device_status::DISABLED)
            .await
            .unwrap();
        assert_401(Some("good-token")).await;
        assert!(!state.device_registry.is_online("d1"));
    }

    /// Browser-style auth: token smuggled in `Sec-WebSocket-Protocol`
    /// (browsers can't set an Authorization header on a WebSocket); the
    /// server must select the static `peckboard-agent` entry — and only
    /// that entry — in the reply.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subprotocol_token_authenticates_without_urls_or_headers() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        insert_active_device(&state, "d1", "tok-1").await;
        let addr = spawn_agent_ws_server(state.clone()).await;

        use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
        let mut req = format!("ws://{addr}/ws/agent")
            .into_client_request()
            .unwrap();
        req.headers_mut().insert(
            "sec-websocket-protocol",
            "peckboard-agent, token.tok-1".parse().unwrap(),
        );
        let (_ws, resp) = tokio_tungstenite::connect_async(req)
            .await
            .expect("subprotocol auth must upgrade");
        assert_eq!(
            resp.headers()
                .get("sec-websocket-protocol")
                .and_then(|v| v.to_str().ok()),
            Some("peckboard-agent")
        );
    }

    /// Full loop over a real socket: hello → online event + last_seen,
    /// registry.send → frame on the wire, disable → severed by the 10s
    /// re-check, then the offline event.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn socket_full_loop_online_forward_and_revoke_sever() {
        use futures_util::{SinkExt as _, StreamExt as _};
        use tokio_tungstenite::tungstenite::Message as TgMessage;

        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        insert_active_device(&state, "d1", "tok-1").await;

        let addr = spawn_agent_ws_server(state.clone()).await;

        let mut events = state.broadcaster.subscribe_all();
        let req = {
            use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
            let mut r = format!("ws://{addr}/ws/agent")
                .into_client_request()
                .unwrap();
            r.headers_mut()
                .insert("authorization", "Bearer tok-1".parse().unwrap());
            r
        };
        let (mut ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .expect("upgrade with a valid token");

        let hello = serde_json::to_string(&Envelope::new(AgentFrame::Hello {
            agent_version: "0.0.1".into(),
            platform: "linux".into(),
            hostname: "box".into(),
            capabilities: vec!["echo".into()],
            features: vec![peckboard_agent_protocol::FEATURE_WINDOW_TARGETS.into()],
        }))
        .unwrap();
        ws.send(TgMessage::Text(hello.into())).await.unwrap();

        let online = wait_for_device_update(&mut events, "d1").await;
        assert!(online);
        assert!(state.device_registry.is_online("d1"));
        assert!(
            state
                .device_registry
                .has_feature("d1", peckboard_agent_protocol::FEATURE_WINDOW_TARGETS),
            "hello features must be recorded on the connection"
        );
        assert!(!state.device_registry.has_feature("d1", "nope"));
        let row = state.db.get_device("d1").await.unwrap().unwrap();
        assert!(row.last_seen_at.is_some(), "hello must stamp last_seen_at");

        // Outbound forwarding: a frame queued in the registry reaches the
        // daemon as a versioned envelope.
        assert!(state.device_registry.send("d1", ServerFrame::Ping).await);
        let frame = next_text(&mut ws).await;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&frame).unwrap(),
            serde_json::json!({ "v": 1, "type": "ping" })
        );

        // Kill-switch: disabling the device severs the socket within the
        // 10s re-check window.
        state
            .db
            .update_device_status("d1", device_status::DISABLED)
            .await
            .unwrap();
        let closed = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match ws.next().await {
                    Some(Ok(TgMessage::Close(_))) | None => break,
                    Some(Ok(_)) => continue,
                    Some(Err(_)) => break,
                }
            }
        })
        .await;
        assert!(closed.is_ok(), "socket must be severed after disable");

        let offline = wait_for_device_update(&mut events, "d1").await;
        assert!(!offline);
        assert!(!state.device_registry.is_online("d1"));
    }

    async fn wait_for_device_update(
        events: &mut tokio::sync::broadcast::Receiver<WsEvent>,
        device_id: &str,
    ) -> bool {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let ev = events.recv().await.expect("broadcast channel open");
                if ev.event_type == "device-update"
                    && ev.data.get("device_id").and_then(|v| v.as_str()) == Some(device_id)
                {
                    return ev.data.get("online").and_then(|v| v.as_bool()).unwrap();
                }
            }
        })
        .await
        .expect("device-update event")
    }

    async fn next_text(
        ws: &mut (
                 impl StreamExt<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin
             ),
    ) -> String {
        use tokio_tungstenite::tungstenite::Message as TgMessage;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match ws.next().await {
                    Some(Ok(TgMessage::Text(t))) => return t.to_string(),
                    Some(Ok(_)) => continue,
                    other => panic!("socket ended early: {other:?}"),
                }
            }
        })
        .await
        .expect("text frame")
    }
}

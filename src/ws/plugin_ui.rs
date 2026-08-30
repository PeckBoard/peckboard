//! `/ws/plugin-ui` — the restricted WebSocket a sandboxed plugin-page iframe
//! may hold itself.
//!
//! The page iframes run with an opaque origin and never see the user's JWT
//! (see `web/src/components/PluginFullPage.tsx`). To give them live updates
//! without polling — and without smuggling the full-authority token into
//! third-party plugin code — the parent app mints a **one-time, short-lived,
//! plugin-scoped ticket** over its authenticated fetch (`POST
//! /api/plugin-ws/ticket`) and hands it into the iframe via postMessage. The
//! iframe redeems it here. The socket is strictly narrower than `/ws`:
//!
//! - it only ever streams `plugin-data` frames for the ticket's plugin
//!   (identifiers only — plugin id + collection, never stored values);
//! - every client frame is ignored: no auth, no subscribe, no resume;
//! - the ticket is single-use and expires in [`TICKET_TTL`], so a leaked URL
//!   is dead moments later and can never be replayed;
//! - the ticket is bound to the minting user's auth session, and the socket
//!   closes when that session is revoked — same revocation story as `/ws`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use rand::RngCore;
use rand::rngs::OsRng;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant as TokioInstant;

use crate::state::AppState;
use crate::ws::broadcaster::WsEvent;

/// How long a minted ticket stays redeemable. Long enough for a postMessage
/// round-trip and a WS handshake, short enough that a logged URL is useless.
const TICKET_TTL: Duration = Duration::from_secs(60);

/// Outstanding-ticket cap — a backstop against a stuck client minting in a
/// loop, far above what real pages (one ticket per connect) ever need.
const MAX_OUTSTANDING: usize = 512;

/// Mirrors the `/ws` heartbeat: ping idle sockets, drop half-open ones.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(90);

struct Ticket {
    plugin_id: String,
    /// The minting user's auth session (JWT `jti`). The socket re-checks it
    /// periodically so revoking the session also severs the iframe's stream.
    auth_session_id: String,
    expires_at: Instant,
}

/// In-memory store of unredeemed tickets. Ephemeral by design: a server
/// restart invalidates them all and pages simply mint a new one to reconnect.
#[derive(Default)]
pub struct PluginWsTickets {
    inner: Mutex<HashMap<String, Ticket>>,
}

impl PluginWsTickets {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a ticket for `plugin_id`, bound to `auth_session_id`. Returns
    /// `None` when the outstanding cap is hit even after purging expired
    /// entries (a client gone haywire, not a normal condition).
    pub fn issue(&self, plugin_id: &str, auth_session_id: &str) -> Option<String> {
        self.issue_with_ttl(plugin_id, auth_session_id, TICKET_TTL)
    }

    fn issue_with_ttl(
        &self,
        plugin_id: &str,
        auth_session_id: &str,
        ttl: Duration,
    ) -> Option<String> {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        let token = hex::encode(bytes);

        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap();
        inner.retain(|_, t| t.expires_at > now);
        if inner.len() >= MAX_OUTSTANDING {
            return None;
        }
        inner.insert(
            token.clone(),
            Ticket {
                plugin_id: plugin_id.to_string(),
                auth_session_id: auth_session_id.to_string(),
                expires_at: now + ttl,
            },
        );
        Some(token)
    }

    /// Redeem a ticket: removes it (single-use) and returns its scope when it
    /// is still live. An unknown, reused, or expired token returns `None` —
    /// indistinguishable on purpose.
    fn redeem(&self, token: &str) -> Option<(String, String)> {
        let mut inner = self.inner.lock().unwrap();
        let ticket = inner.remove(token)?;
        if ticket.expires_at <= Instant::now() {
            return None;
        }
        Some((ticket.plugin_id, ticket.auth_session_id))
    }
}

/// The frame a plugin page receives. Identifiers only — the page refetches
/// whatever it renders through the authed fetch bridge.
fn frame_for(plugin_id: &str, event: &WsEvent) -> Option<String> {
    if event.event_type != "plugin-data" {
        return None;
    }
    if event.data.get("plugin_id").and_then(|v| v.as_str()) != Some(plugin_id) {
        return None;
    }
    let collection = event
        .data
        .get("collection")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    Some(
        serde_json::json!({ "type": "plugin-data", "plugin_id": plugin_id, "collection": collection })
            .to_string(),
    )
}

#[derive(serde::Deserialize)]
pub struct PluginWsQuery {
    #[serde(default)]
    ticket: String,
}

/// Upgrade handler. The ticket is redeemed BEFORE the upgrade completes, so a
/// bad token costs one 401 and never allocates a socket or a broadcast slot.
pub async fn plugin_ws_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<PluginWsQuery>,
    State(state): State<Arc<AppState>>,
) -> Response {
    let Some((plugin_id, auth_session_id)) = state.plugin_ws_tickets.redeem(query.ticket.trim())
    else {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({ "error": "invalid or expired ticket" })),
        )
            .into_response();
    };
    ws.on_upgrade(move |socket| handle_plugin_socket(socket, state, plugin_id, auth_session_id))
        .into_response()
}

async fn handle_plugin_socket(
    socket: WebSocket,
    state: Arc<AppState>,
    plugin_id: String,
    auth_session_id: String,
) {
    let (mut sender, mut receiver) = socket.split();
    let mut broadcast_rx = state.broadcaster.subscribe_all();

    // Same cadence as `/ws`: revocation check + half-open detection.
    let mut auth_check = tokio::time::interval(Duration::from_secs(10));
    auth_check.tick().await;
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.tick().await;
    let mut last_seen = TokioInstant::now();

    loop {
        tokio::select! {
            _ = auth_check.tick() => {
                let alive = state
                    .db
                    .get_auth_session(&auth_session_id)
                    .await
                    .ok()
                    .flatten()
                    .is_some();
                if !alive {
                    let _ = sender
                        .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                            code: 4001,
                            reason: "session revoked".into(),
                        })))
                        .await;
                    break;
                }
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
                    // This socket accepts nothing from the client — pages
                    // only listen. Frames still count as liveness (pongs
                    // arrive here too, via tungstenite's auto-answer).
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => { last_seen = TokioInstant::now(); }
                    Some(Err(_)) => break,
                }
            }
            event = broadcast_rx.recv() => {
                match event {
                    Ok(ws_event) => {
                        if let Some(frame) = frame_for(&plugin_id, &ws_event)
                            && sender.send(Message::Text(frame.into())).await.is_err()
                        {
                            break;
                        }
                    }
                    // Dropped events are unrecoverable from the channel; the
                    // page refetches on any frame, so a nudge fully heals it.
                    Err(RecvError::Lagged(_)) => {
                        let nudge = serde_json::json!({ "type": "resync" }).to_string();
                        if sender.send(Message::Text(nudge.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tickets_are_single_use_and_expire() {
        let store = PluginWsTickets::new();
        let token = store.issue("session-control", "auth-1").unwrap();

        // Wrong token: nothing.
        assert!(store.redeem("nope").is_none());
        // First redeem wins and carries the scope.
        assert_eq!(
            store.redeem(&token),
            Some(("session-control".into(), "auth-1".into()))
        );
        // Second redeem of the same token is refused.
        assert!(store.redeem(&token).is_none());

        // Expired tickets are dead even on first redeem.
        let stale = store
            .issue_with_ttl("session-control", "auth-1", Duration::ZERO)
            .unwrap();
        assert!(store.redeem(&stale).is_none());
    }

    #[test]
    fn frames_are_scoped_to_the_tickets_plugin_and_carry_no_values() {
        let event = WsEvent {
            event_type: "plugin-data".into(),
            session_id: String::new(),
            data: serde_json::json!({ "plugin_id": "ui-gauge", "collection": "baselines" }),
        };
        let frame = frame_for("ui-gauge", &event).unwrap();
        assert!(frame.contains("\"baselines\""));
        assert!(!frame.contains("value"));
        // Another plugin's ticket sees nothing.
        assert!(frame_for("session-control", &event).is_none());
        // Non-plugin-data events never cross, whatever they carry.
        let other = WsEvent {
            event_type: "message".into(),
            session_id: "s1".into(),
            data: serde_json::json!({ "plugin_id": "ui-gauge", "text": "secret" }),
        };
        assert!(frame_for("ui-gauge", &other).is_none());
    }
}

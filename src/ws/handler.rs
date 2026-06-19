use axum::{
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::time::{Duration, timeout};

use crate::auth::token::validate_token;
use crate::state::AppState;

static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

/// Incoming frame types from the client.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientFrame {
    Auth { token: String },
    Subscribe { session_id: String },
    Unsubscribe { session_id: String },
    Resume { session_id: String, last_seq: i32 },
}

/// Outgoing frame types to the client.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerFrame {
    AuthOk {
        user_id: String,
    },
    #[allow(dead_code)]
    Error {
        message: String,
    },
    Event {
        session_id: String,
        event: serde_json::Value,
    },
    ResumeComplete {
        session_id: String,
    },
}

/// WebSocket upgrade handler.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_connection(socket, state))
}

async fn handle_connection(socket: WebSocket, state: Arc<AppState>) {
    let client_id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
    let (mut sender, mut receiver) = socket.split();

    // Auth handshake: first frame must be auth within 10 seconds.
    let auth_result = timeout(Duration::from_secs(10), async {
        let msg = receiver.next().await?.ok()?;
        let Message::Text(text) = msg else {
            return None;
        };
        let ClientFrame::Auth { token } = serde_json::from_str(&text).ok()? else {
            return None;
        };
        validate_token(&state.jwt_secret, &token)
            .map(|claims| (claims.sub, claims.jti, claims.role))
            .ok()
    })
    .await;

    let (user_id, session_id, role) = match auth_result {
        Ok(Some(triple)) => triple,
        _ => {
            let _ = sender
                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                    code: 4001,
                    reason: "auth required".into(),
                })))
                .await;
            return;
        }
    };
    let is_admin = role == "admin";

    // Send auth ok
    let _ = sender
        .send(Message::Text(
            serde_json::to_string(&ServerFrame::AuthOk {
                user_id: user_id.clone(),
            })
            .unwrap()
            .into(),
        ))
        .await;

    tracing::info!("WS client {client_id} authenticated as {user_id}");

    // Get a broadcast receiver
    let mut broadcast_rx = state.broadcaster.subscribe_all();

    // Periodic auth session check
    let mut auth_check_interval = tokio::time::interval(Duration::from_secs(10));
    auth_check_interval.tick().await; // consume the immediate first tick

    // Main message loop
    loop {
        tokio::select! {
            // Periodic auth session validity check
            _ = auth_check_interval.tick() => {
                let session_exists = state
                    .db
                    .get_auth_session(&session_id)
                    .await
                    .ok()
                    .flatten()
                    .is_some();

                if !session_exists {
                    tracing::info!("WS client {client_id} auth session revoked, closing");
                    let _ = sender
                        .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                            code: 4001,
                            reason: "session revoked".into(),
                        })))
                        .await;
                    break;
                }
            }
            // Handle incoming client frames
            msg = receiver.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(frame) = serde_json::from_str::<ClientFrame>(&text) {
                            // Re-validate the auth session before honouring
                            // any action frame. The periodic 10s tick above
                            // catches revoked sessions for the broadcast
                            // path, but a Subscribe / Unsubscribe / Resume
                            // arriving within that window would otherwise
                            // run against a session that's already been
                            // revoked. Re-checking here closes the window.
                            let still_valid = state
                                .db
                                .get_auth_session(&session_id)
                                .await
                                .ok()
                                .flatten()
                                .is_some();
                            if !still_valid {
                                tracing::info!(
                                    "WS client {client_id} action on revoked auth session, closing"
                                );
                                let _ = sender
                                    .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                                        code: 4001,
                                        reason: "session revoked".into(),
                                    })))
                                    .await;
                                break;
                            }
                            match frame {
                                ClientFrame::Subscribe { session_id } => {
                                    // Sessions are not per-user partitioned; gate
                                    // subscription on admin role so a non-admin
                                    // user can't tap into another's stream by
                                    // guessing session UUIDs.
                                    if !is_admin {
                                        tracing::info!(
                                            "WS client {client_id} non-admin subscribe denied"
                                        );
                                        continue;
                                    }
                                    state.broadcaster.subscribe(client_id, &session_id).await;
                                }
                                ClientFrame::Unsubscribe { session_id } => {
                                    state.broadcaster.unsubscribe(client_id, &session_id).await;
                                }
                                ClientFrame::Resume { session_id, last_seq } => {
                                    if !is_admin {
                                        tracing::info!(
                                            "WS client {client_id} non-admin resume denied"
                                        );
                                        // Send ResumeComplete so the client unblocks
                                        // instead of hanging on a forbidden replay.
                                        let _ = sender.send(Message::Text(
                                            serde_json::to_string(&ServerFrame::ResumeComplete {
                                                session_id,
                                            }).unwrap().into()
                                        )).await;
                                        continue;
                                    }
                                    // Replay events since last_seq
                                    if let Ok(events) = state.db.events_since(&session_id, last_seq).await {
                                        for event in events.iter().take(500) {
                                            let frame = ServerFrame::Event {
                                                session_id: session_id.clone(),
                                                event: serde_json::json!({
                                                    "id": event.id,
                                                    "seq": event.seq,
                                                    "ts": event.ts,
                                                    "kind": event.kind,
                                                    "data": serde_json::from_str::<serde_json::Value>(&event.data).unwrap_or_default(),
                                                }),
                                            };
                                            let _ = sender.send(Message::Text(
                                                serde_json::to_string(&frame).unwrap().into()
                                            )).await;
                                        }
                                    }
                                    let _ = sender.send(Message::Text(
                                        serde_json::to_string(&ServerFrame::ResumeComplete {
                                            session_id,
                                        }).unwrap().into()
                                    )).await;
                                }
                                ClientFrame::Auth { .. } => {
                                    // Already authenticated, ignore
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        break;
                    }
                    _ => {}
                }
            }
            // Handle broadcast events
            event = broadcast_rx.recv() => {
                if let Ok(ws_event) = event {
                    // Global events (card-update, announcement, queue) go to all clients
                    let is_global = matches!(
                        ws_event.event_type.as_str(),
                        "card-update"
                            | "card-delete"
                            | "worker-question"
                            | "announcement"
                            | "queue"
                            | "project-update"
                            // session-deleted must reach every connected
                            // client — devices that had the session open
                            // but unsubscribed (or never subscribed,
                            // because they were sitting on a different
                            // session) still need to drop the tab + clear
                            // an orphaned activeSessionId.
                            | "session-deleted"
                            // plugin-approval must reach every client so an
                            // open approval prompt updates the moment any
                            // operator (in any tab) decides.
                            | "plugin-approval"
                    );

                    let should_send = if is_global {
                        true
                    } else {
                        state
                            .broadcaster
                            .is_subscribed(client_id, &ws_event.session_id)
                            .await
                    };

                    if should_send {
                        let frame = serde_json::json!({
                            "type": ws_event.event_type,
                            "session_id": ws_event.session_id,
                            "event": ws_event.data,
                            "data": ws_event.data,
                        });
                        if sender.send(Message::Text(
                            serde_json::to_string(&frame).unwrap().into()
                        )).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }

    // Cleanup
    state.broadcaster.remove_client(client_id).await;
    tracing::info!("WS client {client_id} disconnected");
}

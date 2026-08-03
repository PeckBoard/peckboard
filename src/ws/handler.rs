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
use std::time::Instant;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{Duration, timeout};

use crate::auth::token::validate_token;
use crate::state::AppState;

static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

/// Events per page when replaying a `resume` backlog. Bounds the memory a
/// single resume pulls out of SQLite; the handler keeps paging until the
/// session is caught up, so a long gap is no longer a permanent hole.
const RESUME_PAGE_SIZE: i64 = 500;

/// Reason returned to a client whose `Subscribe` / `Resume` the stream gate
/// refused. Kept in one place so the WS test and the UI copy stay in sync.
const STREAM_DENIED_REASON: &str = "You don't have access to this session's live updates.";

/// How often the server pings an idle connection. Half-open sockets
/// (sleeping laptops, dropped mobile connections) otherwise linger until a
/// TCP-level timeout and keep their broadcast slot allocated.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Grace window since the last frame of any kind (client message, Pong, or
/// auto-answered Ping) before a connection is considered dead and dropped.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(90);

/// True once `elapsed` since the last frame from the client exceeds the

/// heartbeat grace window. Pulled out as a pure fn so the threshold logic is
/// unit-testable without spinning up a socket.
fn heartbeat_expired(elapsed: Duration) -> bool {
    elapsed > HEARTBEAT_TIMEOUT
}
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
    /// A `Subscribe` / `Resume` the server refused. Sent so the client can
    /// surface an error instead of waiting on a stream that never opens.
    SubscribeDenied {
        session_id: String,
        reason: String,
    },
    /// The client's broadcast slot overflowed and `dropped` events were
    /// discarded before we could forward them. Nothing can recover them from
    /// the channel, so the client must re-run its Resume flow. `session_id` /
    /// `last_seq` name an affected session and the newest seq the server has
    /// for it; both are omitted when the client has no session subscriptions
    /// (it still lost global frames and should refetch).
    Resync {
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        last_seq: Option<i32>,
        dropped: u64,
    },
}

/// May this client stream `session_id`'s events?
///
/// Admins keep blanket access. Everyone else may stream only sessions they
/// own, or a worker/expert session attached to a project (shared board) —
/// same rule as the REST layer's `require_session_access`
/// ([`crate::auth::access::may_access_session`]).
///
/// One id here is not a session at all: `doc-review-update` frames are keyed
/// by the *review* id, so the review screen subscribes to that. Reviews carry
/// no owner column and their REST surface is `require_auth` only, so any
/// authenticated client may stream one — matching what it could already read
/// over HTTP.
async fn may_stream_session(
    state: &AppState,
    is_admin: bool,
    user_id: &str,
    session_id: &str,
) -> bool {
    if is_admin {
        return true;
    }
    match state.db.get_session(session_id).await {
        Ok(Some(session)) => crate::auth::access::may_access_session(
            is_admin,
            user_id,
            session.user_id.as_deref(),
            session.project_id.as_deref(),
        ),
        Ok(None) => matches!(state.db.get_doc_review(session_id).await, Ok(Some(_))),
        Err(e) => {
            tracing::warn!("WS ownership check failed for session {session_id}: {e}");
            false
        }
    }
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

    // Ping/pong heartbeat: detects half-open sockets (sleeping laptops,
    // dropped mobile connections) that would otherwise linger until a TCP
    // timeout, keeping their broadcast slot allocated indefinitely.
    let mut heartbeat_interval = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat_interval.tick().await; // consume the immediate first tick
    let mut last_seen = Instant::now();

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
            // Ping the client, or drop it if it's gone quiet for too long.
            // A dropped connection falls through to the cleanup below,
            // which releases its broadcast subscriptions.
            _ = heartbeat_interval.tick() => {
                if heartbeat_expired(last_seen.elapsed()) {
                    tracing::info!("WS client {client_id} heartbeat timeout, closing");
                    let _ = sender
                        .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                            code: 1001,
                            reason: "heartbeat timeout".into(),
                        })))
                        .await;
                    break;
                }
                // tungstenite auto-answers client-initiated Pings with a
                // Pong, so we only need to originate our own Ping here.
                if sender.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
            // Handle incoming client frames
            msg = receiver.next() => {
                if matches!(msg, Some(Ok(_))) {
                    last_seen = Instant::now();
                }
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
                                    // Same owner-or-shared-board rule as the
                                    // REST layer's `require_session_access`:
                                    // admins get blanket access, everyone else
                                    // only their own sessions (or a board-
                                    // attached worker/expert session).
                                    if !may_stream_session(&state, is_admin, &user_id, &session_id)
                                        .await
                                    {
                                        tracing::info!(
                                            "WS client {client_id} subscribe denied for {session_id}"
                                        );
                                        // Tell the client it was refused. A silent
                                        // drop left the UI waiting forever on a
                                        // stream that would never open.
                                        let _ = sender
                                            .send(Message::Text(
                                                serde_json::to_string(
                                                    &ServerFrame::SubscribeDenied {
                                                        session_id,
                                                        reason: STREAM_DENIED_REASON.to_string(),
                                                    },
                                                )
                                                .unwrap()
                                                .into(),
                                            ))
                                            .await;
                                        continue;
                                    }
                                    state.broadcaster.subscribe(client_id, &session_id).await;
                                }
                                ClientFrame::Unsubscribe { session_id } => {
                                    state.broadcaster.unsubscribe(client_id, &session_id).await;
                                }
                                ClientFrame::Resume { session_id, last_seq } => {
                                    if !may_stream_session(&state, is_admin, &user_id, &session_id)
                                        .await
                                    {
                                        tracing::info!(
                                            "WS client {client_id} resume denied for {session_id}"
                                        );
                                        // Same refusal frame as Subscribe so the UI
                                        // has one error state for both, then
                                        // ResumeComplete so the client unblocks
                                        // instead of hanging on a forbidden replay.
                                        let _ = sender
                                            .send(Message::Text(
                                                serde_json::to_string(
                                                    &ServerFrame::SubscribeDenied {
                                                        session_id: session_id.clone(),
                                                        reason: STREAM_DENIED_REASON.to_string(),
                                                    },
                                                )
                                                .unwrap()
                                                .into(),
                                            ))
                                            .await;
                                        let _ = sender.send(Message::Text(
                                            serde_json::to_string(&ServerFrame::ResumeComplete {
                                                session_id,
                                            }).unwrap().into()
                                        )).await;
                                        continue;
                                    }
                                    // Replay events since last_seq, page by
                                    // page. A single capped query left a
                                    // permanent hole whenever the gap was
                                    // bigger than the cap, so keep pulling
                                    // pages until the session is caught up.
                                    let mut cursor = last_seq;
                                    loop {
                                        let page = match state
                                            .db
                                            .events_since_page(&session_id, cursor, RESUME_PAGE_SIZE)
                                            .await
                                        {
                                            Ok(page) => page,
                                            Err(e) => {
                                                tracing::warn!(
                                                    "WS client {client_id} resume page failed: {e}"
                                                );
                                                break;
                                            }
                                        };
                                        if page.is_empty() {
                                            break;
                                        }
                                        let page_len = page.len() as i64;
                                        let mut send_failed = false;
                                        for event in &page {
                                            cursor = event.seq;
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
                                            if sender.send(Message::Text(
                                                serde_json::to_string(&frame).unwrap().into()
                                            )).await.is_err() {
                                                send_failed = true;
                                                break;
                                            }
                                        }
                                        // A short page means we drained the
                                        // tail; a full one means there may be
                                        // more behind it.
                                        if send_failed || page_len < RESUME_PAGE_SIZE {
                                            break;
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
                match event {
                    Ok(ws_event) => {
                    // Global events (card-update, announcement, …) go to all
                    // clients. `queue` is deliberately NOT here: its payload
                    // names a session (and used to carry the message text), so
                    // it must go through the per-client `is_subscribed` gate
                    // that `may_stream_session` backs with owner-or-admin.
                    let is_global = matches!(
                        ws_event.event_type.as_str(),
                        "card-update"
                            | "card-delete"
                            | "worker-question"
                            | "announcement"
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
                            // askpass must reach every client: a sudo prompt
                            // in a session whose tab was closed still needs a
                            // human, and the resolve must dismiss every
                            // dialog, not just the answering tab's.
                            | "askpass-request"
                            | "askpass-resolved"
                            // env-unlock mirrors askpass: the vars' owner
                            // may not have the requesting session's tab
                            // open, and the resolve must dismiss every
                            // open dialog.
                            | "env-unlock-request"
                            | "env-unlock-resolved"
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
                    // The client's broadcast slot overflowed: `dropped`
                    // events are gone from the channel for good. Ask the
                    // client to re-run its Resume flow for every session it
                    // watches, so the gap is refilled from the event log
                    // instead of silently persisting until a reload.
                    Err(RecvError::Lagged(dropped)) => {
                        tracing::warn!(
                            "WS client {client_id} lagged, {dropped} event(s) dropped; requesting resync"
                        );
                        let sessions = state.broadcaster.sessions_for_client(client_id).await;
                        let mut frames: Vec<ServerFrame> = Vec::new();
                        for sid in sessions {
                            let last_seq = state.db.latest_seq(&sid).await.ok().flatten();
                            frames.push(ServerFrame::Resync {
                                session_id: Some(sid),
                                last_seq,
                                dropped,
                            });
                        }
                        if frames.is_empty() {
                            // No session subscriptions, but global frames
                            // (card updates, announcements) were dropped too
                            // — the client still needs to refetch.
                            frames.push(ServerFrame::Resync {
                                session_id: None,
                                last_seq: None,
                                dropped,
                            });
                        }
                        let mut send_failed = false;
                        for frame in frames {
                            if sender
                                .send(Message::Text(
                                    serde_json::to_string(&frame).unwrap().into(),
                                ))
                                .await
                                .is_err()
                            {
                                send_failed = true;
                                break;
                            }
                        }
                        if send_failed {
                            break;
                        }
                    }
                    // Broadcaster gone (shutdown): recv() would return
                    // instantly forever, so stop rather than spin.
                    Err(RecvError::Closed) => break,
                }
            }
        }
    }

    // Cleanup
    state.broadcaster.remove_client(client_id).await;
    tracing::info!("WS client {client_id} disconnected");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::middleware::tests::test_state;

    #[test]
    fn heartbeat_expired_at_grace_window() {
        assert!(!heartbeat_expired(Duration::from_secs(89)));
        assert!(!heartbeat_expired(HEARTBEAT_TIMEOUT));
        assert!(heartbeat_expired(Duration::from_secs(91)));
    }
    use crate::db::models::{NewFolder, NewProject, NewSession, NewUser};

    async fn seed_user(state: &Arc<AppState>, id: &str, username: &str, role: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        state
            .db
            .create_user(NewUser {
                id: id.into(),
                username: username.into(),
                email: None,
                password_hash: "x".into(),
                role: role.into(),
                created_at: now.clone(),
                updated_at: now,
            })
            .await
            .unwrap();
    }

    /// Seed a chat session owned by `owner` (`None` = legacy/internal row).
    async fn seed_session(state: &Arc<AppState>, id: &str, owner: Option<&str>) {
        let now = chrono::Utc::now().to_rfc3339();
        state
            .db
            .create_folder(NewFolder {
                id: "f1".into(),
                name: "f1".into(),
                path: "/tmp/f1".into(),
                created_at: now.clone(),
            })
            .await
            .ok();
        state
            .db
            .create_session(NewSession {
                id: id.into(),
                name: id.into(),
                folder_id: "f1".into(),
                created_at: now.clone(),
                last_activity: now,
                user_id: owner.map(str::to_string),
                ..Default::default()
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_non_admin_may_stream_a_session_they_own() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        seed_user(&state, "u1", "alice", "user").await;
        seed_session(&state, "s-own", Some("u1")).await;

        assert!(may_stream_session(&state, false, "u1", "s-own").await);
    }

    #[tokio::test]
    async fn a_non_admin_may_not_stream_someone_elses_session() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        seed_user(&state, "u1", "alice", "user").await;
        seed_user(&state, "u2", "bob", "user").await;
        seed_session(&state, "s-theirs", Some("u2")).await;

        assert!(!may_stream_session(&state, false, "u1", "s-theirs").await);
    }

    #[tokio::test]
    async fn an_unowned_session_never_matches_a_non_admin() {
        // NULL owner = non-matching, the same rule the session-control
        // send_message gate applies to legacy / internally-spawned rows.
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        seed_user(&state, "u1", "alice", "user").await;
        seed_session(&state, "s-orphan", None).await;

        assert!(!may_stream_session(&state, false, "u1", "s-orphan").await);
    }

    #[tokio::test]
    async fn a_non_admin_may_not_stream_an_unknown_session_id() {
        // Guessing UUIDs must not open a stream, and must not error open.
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        seed_user(&state, "u1", "alice", "user").await;

        assert!(!may_stream_session(&state, false, "u1", "no-such-session").await);
    }

    #[tokio::test]
    async fn an_admin_may_stream_any_session() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        seed_user(&state, "u1", "alice", "user").await;
        seed_user(&state, "admin", "root", "admin").await;
        seed_session(&state, "s-theirs", Some("u1")).await;

        assert!(may_stream_session(&state, true, "admin", "s-theirs").await);
    }

    #[tokio::test]
    async fn a_non_admin_may_stream_a_board_attached_session_they_dont_own() {
        // A worker/expert session tied to a project (shared board) is
        // reachable by any logged-in user, same as the board itself —
        // even though its `user_id` belongs to someone else.
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        seed_user(&state, "u1", "alice", "user").await;
        seed_user(&state, "u2", "bob", "user").await;
        let now = chrono::Utc::now().to_rfc3339();
        state
            .db
            .create_folder(NewFolder {
                id: "f1".into(),
                name: "f1".into(),
                path: "/tmp/f1".into(),
                created_at: now.clone(),
            })
            .await
            .ok();
        state
            .db
            .create_project(NewProject {
                id: "proj-1".into(),
                name: "proj-1".into(),
                context: "".into(),
                folder_id: "f1".into(),
                worker_count: 1,
                status: "active".into(),
                workflow: "default".into(),
                model: None,
                effort: None,
                parallel_instructions: false,
                auto_notify_changes: false,
                worker_communication: false,
                worktree_isolation: false,
                created_at: now.clone(),
                last_accessed_at: now.clone(),
                budget_usd_cents: None,
                budget_period: None,
            })
            .await
            .ok();
        state
            .db
            .create_session(NewSession {
                id: "s-worker".into(),
                name: "s-worker".into(),
                folder_id: "f1".into(),
                is_worker: true,
                project_id: Some("proj-1".into()),
                created_at: now.clone(),
                last_activity: now,
                user_id: Some("u2".into()),
                ..Default::default()
            })
            .await
            .unwrap();

        assert!(may_stream_session(&state, false, "u1", "s-worker").await);
    }
}

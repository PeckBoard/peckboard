//! `/ws/terminal/{id}` — a browser's live view of an interactive SSH
//! terminal ([`crate::terminal`]).
//!
//! Authenticated like the main `/ws`: the first frame must be
//! `{"type":"auth","token":"<JWT>","cols":N,"rows":N}` within 10 s. Only the
//! terminal's owner (or an admin) may attach, and the socket closes when the
//! auth session is revoked.
//!
//! Wire protocol (one terminal per socket):
//!
//! - server → client, text: `{"type":"status","phase":…,"persistent":…,
//!   "message":…}` after auth and on every change; `{"type":"replay",
//!   "bytes":N}` right after the first status (the next binary frame, when
//!   N > 0, is the scrollback snapshot); `{"type":"resync"}` when this
//!   viewer fell behind (reconnect to replay);
//! - server → client, binary: raw PTY output — the scrollback snapshot
//!   first, then live output as it arrives;
//! - client → server, binary: raw keystrokes (sent per keystroke, unbuffered);
//! - client → server, text: `{"type":"resize","cols":N,"rows":N}`,
//!   `{"type":"restart"}` (start a fresh shell after `ended` / `error`).
//!
//! Several viewers (tabs, a pop-out window, other devices) may attach to one
//! terminal; each gets the full stream and all of them may type.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{Instant, timeout};

use crate::auth::token::validate_token;
use crate::state::AppState;
use crate::terminal::{MAX_COLS, MAX_ROWS, Status, TermSession};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(90);
/// Largest single input frame accepted (a big paste).
const MAX_INPUT_FRAME: usize = 256 * 1024;

pub async fn terminal_ws_handler(
    ws: WebSocketUpgrade,
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_terminal_socket(socket, state, id))
}

fn status_frame(s: &Status) -> Message {
    let mut v = serde_json::to_value(s).unwrap_or_default();
    v["type"] = json!("status");
    Message::Text(v.to_string().into())
}

fn geometry(v: &Value) -> Option<(u16, u16)> {
    let cols = v.get("cols").and_then(Value::as_u64)?;
    let rows = v.get("rows").and_then(Value::as_u64)?;
    if (2..=u64::from(MAX_COLS)).contains(&cols) && (1..=u64::from(MAX_ROWS)).contains(&rows) {
        Some((cols as u16, rows as u16))
    } else {
        None
    }
}

/// Apply one client text frame. `true` = restart requested.
fn apply_text_command(term: &TermSession, text: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    match v.get("type").and_then(Value::as_str) {
        Some("resize") => {
            if let Some((cols, rows)) = geometry(&v) {
                term.resize(cols, rows);
            }
            false
        }
        Some("restart") => true,
        _ => false,
    }
}

async fn close_with(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    code: u16,
    reason: &str,
) {
    let _ = sender
        .send(Message::Close(Some(axum::extract::ws::CloseFrame {
            code,
            reason: reason.to_string().into(),
        })))
        .await;
}

async fn handle_terminal_socket(socket: WebSocket, state: Arc<AppState>, id: String) {
    let (mut sender, mut receiver) = socket.split();

    // Auth handshake: first frame must be auth within 10 seconds.
    let auth = timeout(Duration::from_secs(10), async {
        let Message::Text(text) = receiver.next().await?.ok()? else {
            return None;
        };
        let v: Value = serde_json::from_str(&text).ok()?;
        if v.get("type").and_then(Value::as_str) != Some("auth") {
            return None;
        }
        let token = v.get("token").and_then(Value::as_str)?;
        let claims = validate_token(&state.jwt_secret, token).ok()?;
        Some((claims.sub, claims.jti, claims.role, geometry(&v)))
    })
    .await;
    let Ok(Some((user_id, auth_session_id, role, geom))) = auth else {
        close_with(&mut sender, 4001, "auth required").await;
        return;
    };
    let session_alive = state
        .db
        .get_auth_session(&auth_session_id)
        .await
        .ok()
        .flatten()
        .is_some();
    if !session_alive {
        close_with(&mut sender, 4001, "session revoked").await;
        return;
    }
    // Owner (or admin) of an open terminal only. Unknown, closed, and
    // foreign terminals are indistinguishable.
    let row = match state.db.get_terminal(&id).await {
        Ok(Some(row)) if row.closed_at.is_none() && (row.user_id == user_id || role == "admin") => {
            row
        }
        _ => {
            close_with(&mut sender, 4004, "no such terminal").await;
            return;
        }
    };

    let (cols, rows) = geom.unwrap_or((80, 24));
    let (term, _viewer) = state.terminals.attach(&row, cols, rows);
    let mut status_rx = term.status_rx();
    let current = status_rx.borrow_and_update().clone();
    let (snapshot, mut out_rx) = term.attach_output();
    if sender.send(status_frame(&current)).await.is_err() {
        return;
    }
    // Announce the replay so the client resets its screen and ignores the
    // replies xterm generates for terminal queries recorded in it.
    let replay = json!({ "type": "replay", "bytes": snapshot.len() }).to_string();
    if sender.send(Message::Text(replay.into())).await.is_err() {
        return;
    }
    if !snapshot.is_empty() && sender.send(Message::Binary(snapshot.into())).await.is_err() {
        return;
    }

    let mut auth_check = tokio::time::interval(Duration::from_secs(10));
    auth_check.tick().await;
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.tick().await;
    let mut last_seen = Instant::now();

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
                    close_with(&mut sender, 4001, "session revoked").await;
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
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(Message::Binary(data))) => {
                        last_seen = Instant::now();
                        if data.len() <= MAX_INPUT_FRAME && term.input(data) {
                            state.terminals.note_activity(&term);
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        last_seen = Instant::now();
                        if apply_text_command(&term, text.as_str()) {
                            state.terminals.restart(&term);
                        }
                    }
                    Some(Ok(_)) => { last_seen = Instant::now(); }
                }
            }
            out = out_rx.recv() => {
                match out {
                    Ok(bytes) => {
                        if sender.send(Message::Binary(bytes)).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(_)) => {
                        let nudge = json!({ "type": "resync" }).to_string();
                        let _ = sender.send(Message::Text(nudge.into())).await;
                        break;
                    }
                    Err(RecvError::Closed) => break,
                }
            }
            changed = status_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let s = status_rx.borrow_and_update().clone();
                if sender.send(status_frame(&s)).await.is_err() {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_accepts_only_sane_sizes() {
        assert_eq!(geometry(&json!({"cols": 132, "rows": 40})), Some((132, 40)));
        assert_eq!(geometry(&json!({"cols": 0, "rows": 40})), None);
        assert_eq!(geometry(&json!({"cols": 80, "rows": 9999})), None);
        assert_eq!(geometry(&json!({"rows": 40})), None);
    }
}

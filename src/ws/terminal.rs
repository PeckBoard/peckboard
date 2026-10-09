//! `/ws/terminal` — a plugin page's live view of an interactive SSH
//! terminal ([`crate::plugin::ssh_term`]).
//!
//! Authentication is the `/ws/plugin-ui` ticket model ([`super::plugin_ui`]):
//! the parent app mints a one-time, plugin-scoped ticket over its authed
//! fetch and hands it into the sandboxed iframe, which redeems it here
//! together with the terminal id. The socket attaches only when the
//! terminal was opened by **that plugin** — a ticket for plugin A can never
//! reach plugin B's shells — and closes when the minting auth session is
//! revoked.
//!
//! Wire protocol (one terminal per socket):
//!
//! - server → client, text: `{"type":"hello","terminal":{…}}` first, then
//!   `{"type":"exited","code":<n|null>,"reason":"…"}` when the shell ends,
//!   and `{"type":"resync"}` if this viewer fell behind (the page reconnects
//!   to replay from the scrollback);
//! - server → client, binary: raw PTY output — the scrollback replay as the
//!   first binary frame(s), then live output as it arrives;
//! - client → server, binary: raw keystrokes for the shell;
//! - client → server, text: `{"type":"resize","cols":N,"rows":N}`, or
//!   `{"type":"input","data":"…"}` as a text alternative to a binary frame.
//!
//! Several viewers may attach to one terminal (two browser tabs); each gets
//! the full stream and all of them may type.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant as TokioInstant;

use crate::plugin::ssh_term::{self, Terminal};
use crate::state::AppState;

/// Mirrors `/ws` and `/ws/plugin-ui`: ping idle sockets, drop half-open ones.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(90);
/// Largest single input frame accepted from a viewer (a paste).
const MAX_INPUT_FRAME: usize = 256 * 1024;

#[derive(serde::Deserialize)]
pub struct TerminalWsQuery {
    #[serde(default)]
    ticket: String,
    #[serde(default)]
    term: String,
}

/// Upgrade handler. Ticket redeemed and ownership checked BEFORE the
/// upgrade, so a bad token or a foreign terminal costs one error response
/// and never allocates a socket.
pub async fn terminal_ws_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<TerminalWsQuery>,
    State(state): State<Arc<AppState>>,
) -> Response {
    let Some((plugin_id, auth_session_id)) = state.plugin_ws_tickets.redeem(query.ticket.trim())
    else {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({ "error": "invalid or expired ticket" })),
        )
            .into_response();
    };
    let Some(term) = ssh_term::registry()
        .get(query.term.trim())
        .filter(|t| t.plugin_id == plugin_id)
    else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": "no such terminal" })),
        )
            .into_response();
    };
    ws.on_upgrade(move |socket| handle_terminal_socket(socket, state, term, auth_session_id))
        .into_response()
}

/// Apply one client text frame. Returns `false` when the frame is not a
/// recognised command (ignored, but still counts as liveness).
fn apply_text_command(term: &Terminal, text: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    match v.get("type").and_then(Value::as_str) {
        Some("resize") => {
            let cols = v.get("cols").and_then(Value::as_u64).unwrap_or(0);
            let rows = v.get("rows").and_then(Value::as_u64).unwrap_or(0);
            if (2..=1000).contains(&cols) && (1..=500).contains(&rows) {
                term.resize(cols as u16, rows as u16);
            }
            true
        }
        Some("input") => {
            if let Some(data) = v.get("data").and_then(Value::as_str)
                && data.len() <= MAX_INPUT_FRAME
            {
                term.input(Bytes::copy_from_slice(data.as_bytes()));
            }
            true
        }
        _ => false,
    }
}

fn exited_frame(info: &ssh_term::ExitInfo) -> String {
    json!({ "type": "exited", "code": info.code, "reason": info.reason }).to_string()
}

async fn handle_terminal_socket(
    socket: WebSocket,
    state: Arc<AppState>,
    term: Arc<Terminal>,
    auth_session_id: String,
) {
    let (mut sender, mut receiver) = socket.split();

    // Subscribe to the exit signal BEFORE reading the current state, so an
    // exit landing in between is seen exactly once (as the current value).
    let mut exit_rx = term.exit_rx();
    let already_exited = exit_rx.borrow_and_update().clone();
    let (snapshot, mut out_rx) = term.attach();

    let hello = json!({ "type": "hello", "terminal": term.info() }).to_string();
    if sender.send(Message::Text(hello.into())).await.is_err() {
        return;
    }
    if !snapshot.is_empty()
        && sender
            .send(Message::Binary(Bytes::from(snapshot)))
            .await
            .is_err()
    {
        return;
    }
    if let Some(info) = &already_exited
        && sender
            .send(Message::Text(exited_frame(info).into()))
            .await
            .is_err()
    {
        return;
    }

    let mut auth_check = tokio::time::interval(Duration::from_secs(10));
    auth_check.tick().await;
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.tick().await;
    let mut last_seen = TokioInstant::now();
    let mut exit_seen = already_exited.is_some();

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
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(Message::Binary(data))) => {
                        last_seen = TokioInstant::now();
                        if data.len() <= MAX_INPUT_FRAME {
                            term.input(data);
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        last_seen = TokioInstant::now();
                        apply_text_command(&term, text.as_str());
                    }
                    // Pongs (tungstenite auto-answers our pings) and pings.
                    Some(Ok(_)) => { last_seen = TokioInstant::now(); }
                }
            }
            out = out_rx.recv() => {
                match out {
                    Ok(bytes) => {
                        if sender.send(Message::Binary(bytes)).await.is_err() {
                            break;
                        }
                    }
                    // This viewer fell behind the shell's output. The lost
                    // bytes are gone from the channel but still in the
                    // scrollback: tell the page to reconnect and replay.
                    Err(RecvError::Lagged(_)) => {
                        let nudge = json!({ "type": "resync" }).to_string();
                        let _ = sender.send(Message::Text(nudge.into())).await;
                        break;
                    }
                    Err(RecvError::Closed) => break,
                }
            }
            changed = exit_rx.changed(), if !exit_seen => {
                exit_seen = true;
                if changed.is_err() {
                    break;
                }
                let info = exit_rx.borrow_and_update().clone();
                if let Some(info) = info
                    && sender.send(Message::Text(exited_frame(&info).into())).await.is_err()
                {
                    break;
                }
                // Stay attached: the page keeps showing the final screen
                // until the user closes the terminal (which ends the socket
                // from the client side).
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_commands_resize_and_type_into_the_shell() {
        let reg = ssh_term::Registry::new();
        let (term, mut cmd_rx) = reg.insert_detached("ssh-fleet", "t");

        assert!(apply_text_command(
            &term,
            r#"{"type":"resize","cols":132,"rows":40}"#
        ));
        assert!(apply_text_command(
            &term,
            r#"{"type":"input","data":"ls\n"}"#
        ));
        // Out-of-range geometry is dropped, not clamped, so a bogus client
        // can't make the PTY unusable for the other viewers.
        assert!(apply_text_command(
            &term,
            r#"{"type":"resize","cols":0,"rows":9999}"#
        ));
        assert!(!apply_text_command(&term, "not json"));
        assert!(!apply_text_command(&term, r#"{"type":"bogus"}"#));

        assert_eq!(
            cmd_rx.try_recv().unwrap(),
            ssh_term::TermCmd::Resize {
                cols: 132,
                rows: 40
            }
        );
        assert_eq!(
            cmd_rx.try_recv().unwrap(),
            ssh_term::TermCmd::Input(Bytes::from_static(b"ls\n"))
        );
        assert!(
            cmd_rx.try_recv().is_err(),
            "bad resize never reached the shell"
        );
    }
}

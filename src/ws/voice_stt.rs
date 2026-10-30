//! `/api/voice/stt` — live speech recognition for the voice assistant,
//! relayed to the external speech server's streaming transcription socket
//! (`service::speech_server`; VoiceStudio's
//! `/v1/audio/transcriptions/stream`). Peckboard holds the upstream
//! connection, so the API key never reaches the browser, and the browser
//! (which can't set headers on a WebSocket) authenticates like `/ws`.
//!
//! Admin-only, like the rest of the voice surface. The first frame must be
//! `{"type":"auth","token":"<JWT>"}` within 10 s; the socket closes when the
//! auth session is revoked.
//!
//! Wire protocol (one recognition session per socket):
//!
//! - client → server, binary: raw signed 16-bit LE mono PCM at 16 kHz;
//! - client → server, text: `{"type":"end"}` — no more audio; the server
//!   sends its last finals, then the socket closes;
//! - server → client, text: `{"type":"ready"}` once the speech server
//!   accepted the stream, `{"type":"partial","text":…}`,
//!   `{"type":"final","text":…,"final_kind":"utterance"|"summary"}`, and
//!   `{"type":"error","error":…,"code"?:…}` (then close). `code` is
//!   `not-configured` when no speech server is set.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message as UpMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::auth::token::validate_token;
use crate::service::speech_server::{self, SpeechServer};
use crate::state::AppState;

/// Largest client audio frame (~1 s of 16 kHz PCM16 is 32 KiB).
const MAX_CLIENT_FRAME: usize = 256 * 1024;
/// Largest upstream message (a transcript).
const MAX_UPSTREAM_MESSAGE: usize = 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Sample rate the client streams at.
pub const STT_RATE: u32 = 16_000;

pub async fn voice_stt_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.max_message_size(MAX_CLIENT_FRAME)
        .max_frame_size(MAX_CLIENT_FRAME)
        .on_upgrade(move |socket| handle_stt_socket(socket, state))
}

type ClientSink = SplitSink<WebSocket, Message>;

async fn send_json(sender: &mut ClientSink, v: Value) -> bool {
    sender
        .send(Message::Text(v.to_string().into()))
        .await
        .is_ok()
}

async fn close_with(sender: &mut ClientSink, code: u16, reason: &str) {
    let _ = sender
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.to_string().into(),
        })))
        .await;
}

/// Send `{"type":"error"}` and close normally.
async fn fail(sender: &mut ClientSink, error: &str, code: Option<&str>) {
    let mut v = json!({ "type": "error", "error": error });
    if let Some(c) = code {
        v["code"] = json!(c);
    }
    send_json(sender, v).await;
    close_with(sender, 1000, "").await;
}

/// The streaming-transcription URL for `server`: its root as ws(s)://.
fn stream_url(server: &SpeechServer) -> String {
    let root = if let Some(rest) = server.root.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = server.root.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        server.root.clone()
    };
    format!("{root}/v1/audio/transcriptions/stream?pcm=1&sr={STT_RATE}")
}

/// An upstream text frame as the client frame to forward (`None` = drop),
/// and whether it ends the session.
fn map_upstream(text: &str) -> (Option<Value>, bool) {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return (None, false);
    };
    let text_of = |v: &Value| v["text"].as_str().unwrap_or("").trim().to_string();
    match v["type"].as_str() {
        Some("partial") => (
            Some(json!({ "type": "partial", "text": text_of(&v) })),
            false,
        ),
        Some("final") => {
            let kind = match v["final_kind"].as_str() {
                Some("utterance") => "utterance",
                _ => "summary",
            };
            (
                Some(json!({ "type": "final", "text": text_of(&v), "final_kind": kind })),
                false,
            )
        }
        Some("error") => {
            let msg = v["error"]["message"]
                .as_str()
                .or_else(|| v["error"].as_str())
                .or_else(|| v["message"].as_str())
                .or_else(|| v["detail"].as_str())
                .unwrap_or("speech server error");
            (
                Some(json!({ "type": "error", "error": format!("speech server: {msg}") })),
                true,
            )
        }
        _ => (None, false),
    }
}

async fn auth_session_alive(state: &AppState, jti: &str) -> bool {
    state
        .db
        .get_auth_session(jti)
        .await
        .ok()
        .flatten()
        .is_some()
}

async fn handle_stt_socket(socket: WebSocket, state: Arc<AppState>) {
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
        Some((claims.jti, claims.role))
    })
    .await;
    let Ok(Some((auth_session_id, role))) = auth else {
        close_with(&mut sender, 4001, "auth required").await;
        return;
    };
    if !auth_session_alive(&state, &auth_session_id).await {
        close_with(&mut sender, 4001, "session revoked").await;
        return;
    }
    if role != "admin" {
        close_with(&mut sender, 4003, "admin only").await;
        return;
    }

    let server = match speech_server::load(&state.db).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            fail(
                &mut sender,
                "no speech server is configured (Settings → Assistant → Speech Server)",
                Some("not-configured"),
            )
            .await;
            return;
        }
        Err(e) => {
            fail(&mut sender, &e.to_string(), None).await;
            return;
        }
    };

    let mut req = match stream_url(&server).into_client_request() {
        Ok(r) => r,
        Err(e) => {
            fail(
                &mut sender,
                &format!("invalid speech server URL: {e}"),
                None,
            )
            .await;
            return;
        }
    };
    if let Some(key) = &server.api_key
        && let Ok(v) = format!("Bearer {key}").parse()
    {
        req.headers_mut().insert("authorization", v);
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_UPSTREAM_MESSAGE))
        .max_frame_size(Some(MAX_UPSTREAM_MESSAGE));
    let upstream = timeout(
        CONNECT_TIMEOUT,
        tokio_tungstenite::connect_async_with_config(req, Some(config), false),
    )
    .await;
    let upstream = match upstream {
        Ok(Ok((ws, _))) => ws,
        Ok(Err(tokio_tungstenite::tungstenite::Error::Http(res))) => {
            let msg = match res.status().as_u16() {
                401 | 403 => "the speech server rejected the API key".to_string(),
                404 => "the speech server has no streaming transcription endpoint".to_string(),
                s => format!("the speech server answered HTTP {s}"),
            };
            fail(&mut sender, &msg, None).await;
            return;
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "speech server stt connect failed");
            fail(
                &mut sender,
                &format!("couldn't reach the speech server: {e}"),
                None,
            )
            .await;
            return;
        }
        Err(_) => {
            fail(&mut sender, "the speech server didn't answer in time", None).await;
            return;
        }
    };
    let (mut up_tx, mut up_rx) = upstream.split();
    if !send_json(&mut sender, json!({ "type": "ready" })).await {
        let _ = up_tx.close().await;
        return;
    }

    let mut auth_check = tokio::time::interval(Duration::from_secs(10));
    auth_check.tick().await;
    // The client said it has no more audio: an upstream close is the
    // normal end of the session, not a failure.
    let mut ended = false;

    loop {
        tokio::select! {
            _ = auth_check.tick() => {
                if !auth_session_alive(&state, &auth_session_id).await {
                    close_with(&mut sender, 4001, "session revoked").await;
                    break;
                }
            }
            msg = receiver.next() => {
                match msg {
                    Some(Ok(Message::Binary(data))) => {
                        if ended || data.len() > MAX_CLIENT_FRAME {
                            continue;
                        }
                        if up_tx.send(UpMessage::Binary(data)).await.is_err() {
                            fail(&mut sender, "the speech server connection dropped", None).await;
                            break;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        let end = serde_json::from_str::<Value>(&text)
                            .ok()
                            .is_some_and(|v| v["type"] == "end");
                        if end && !ended {
                            ended = true;
                            let frame = json!({ "type": "input_audio.end" }).to_string();
                            if up_tx.send(UpMessage::Text(frame.into())).await.is_err() {
                                close_with(&mut sender, 1000, "").await;
                                break;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                }
            }
            msg = up_rx.next() => {
                match msg {
                    Some(Ok(UpMessage::Text(text))) => {
                        let (out, done) = map_upstream(&text);
                        if let Some(out) = out
                            && !send_json(&mut sender, out).await
                        {
                            break;
                        }
                        if done {
                            close_with(&mut sender, 1000, "").await;
                            break;
                        }
                    }
                    Some(Ok(UpMessage::Close(frame))) => {
                        if ended {
                            close_with(&mut sender, 1000, "").await;
                        } else {
                            let reason = frame
                                .map(|f| f.reason.to_string())
                                .filter(|r| !r.is_empty())
                                .unwrap_or_else(|| "it closed the stream".to_string());
                            fail(&mut sender, &format!("speech server: {reason}"), None).await;
                        }
                        break;
                    }
                    None | Some(Err(_)) => {
                        if ended {
                            close_with(&mut sender, 1000, "").await;
                        } else {
                            fail(&mut sender, "the speech server connection dropped", None).await;
                        }
                        break;
                    }
                    Some(Ok(_)) => {}
                }
            }
        }
    }
    // Client gone (or session over): drop the upstream stream too.
    let _ = up_tx.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::middleware::tests::{seed_authenticated_user, test_state};
    use axum::{Router, routing::get};
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    #[test]
    fn maps_upstream_frames() {
        let (v, done) = map_upstream(r#"{"type":"partial","text":" hel ","session_id":"s"}"#);
        assert_eq!(v.unwrap(), json!({"type": "partial", "text": "hel"}));
        assert!(!done);
        let (v, _) = map_upstream(r#"{"type":"final","text":"Hi."}"#);
        assert_eq!(v.unwrap()["final_kind"], "summary");
        let (v, done) = map_upstream(r#"{"type":"error","error":"model not loaded"}"#);
        assert_eq!(v.unwrap()["error"], "speech server: model not loaded");
        assert!(done);
        assert_eq!(map_upstream(r#"{"type":"session.started"}"#).0, None);
        assert_eq!(map_upstream("not json").0, None);
        let s = SpeechServer {
            root: "https://gpu.ts.net".into(),
            api_key: None,
            model: "tts-1".into(),
        };
        assert_eq!(
            stream_url(&s),
            "wss://gpu.ts.net/v1/audio/transcriptions/stream?pcm=1&sr=16000"
        );
    }

    /// A stand-in VoiceStudio stream: bearer-gated, counts audio bytes and
    /// answers `input_audio.end` with a partial, an utterance final and
    /// the whole-session summary.
    async fn fake_stream_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let check = |req: &Request, res: Response| {
                        assert_eq!(req.uri().path(), "/v1/audio/transcriptions/stream");
                        assert_eq!(req.uri().query(), Some("pcm=1&sr=16000"));
                        assert_eq!(req.headers().get("authorization").unwrap(), "Bearer k1");
                        Ok(res)
                    };
                    let mut ws = tokio_tungstenite::accept_hdr_async(tcp, check)
                        .await
                        .unwrap();
                    let started =
                        json!({"type": "session.started", "protocol": "voicestudio.speech.v1"});
                    ws.send(UpMessage::Text(started.to_string().into()))
                        .await
                        .unwrap();
                    let mut bytes = 0;
                    while let Some(Ok(m)) = ws.next().await {
                        match m {
                            UpMessage::Binary(b) => bytes += b.len(),
                            UpMessage::Text(t) => {
                                assert_eq!(t.as_str(), r#"{"type":"input_audio.end"}"#);
                                assert_eq!(bytes, 3200);
                                for f in [
                                    json!({"type": "partial", "text": "turn on"}),
                                    json!({"type": "final", "final_kind": "utterance", "text": "Turn on the lights."}),
                                    json!({"type": "final", "final_kind": "summary", "text": "Turn on the lights."}),
                                ] {
                                    ws.send(UpMessage::Text(f.to_string().into()))
                                        .await
                                        .unwrap();
                                }
                                let _ = ws.close(None).await;
                                return;
                            }
                            _ => {}
                        }
                    }
                });
            }
        });
        format!("http://{addr}")
    }

    async fn next_json<S>(ws: &mut S) -> Option<Value>
    where
        S: futures_util::Stream<Item = Result<UpMessage, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
    {
        loop {
            match ws.next().await? {
                Ok(UpMessage::Text(t)) => return serde_json::from_str(&t).ok(),
                Ok(UpMessage::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relays_partials_and_finals_and_reports_unconfigured() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;
        let app = Router::new()
            .route("/api/voice/stt", get(voice_stt_ws_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let connect = || async {
            let (mut ws, _) =
                tokio_tungstenite::connect_async(format!("ws://{addr}/api/voice/stt"))
                    .await
                    .unwrap();
            let auth = json!({"type": "auth", "token": token});
            ws.send(UpMessage::Text(auth.to_string().into()))
                .await
                .unwrap();
            ws
        };

        // No speech server saved: an error frame, then close.
        let mut ws = connect().await;
        let v = next_json(&mut ws).await.unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["code"], "not-configured");
        assert!(next_json(&mut ws).await.is_none());

        let url = fake_stream_server().await;
        speech_server::save(&state.db, Some(&url), Some("k1"), None)
            .await
            .unwrap();
        let mut ws = connect().await;
        assert_eq!(next_json(&mut ws).await.unwrap(), json!({"type": "ready"}));
        for _ in 0..2 {
            ws.send(UpMessage::Binary(vec![0u8; 1600].into()))
                .await
                .unwrap();
        }
        ws.send(UpMessage::Text(r#"{"type":"end"}"#.into()))
            .await
            .unwrap();
        assert_eq!(
            next_json(&mut ws).await.unwrap(),
            json!({"type": "partial", "text": "turn on"})
        );
        assert_eq!(
            next_json(&mut ws).await.unwrap(),
            json!({"type": "final", "text": "Turn on the lights.", "final_kind": "utterance"})
        );
        assert_eq!(next_json(&mut ws).await.unwrap()["final_kind"], "summary");
        // The upstream closed after the end: a clean close, no error.
        assert!(next_json(&mut ws).await.is_none());
    }
}

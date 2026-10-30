//! `/api/voice/tts*` — server-side speech for the voice assistant: Kokoro
//! (see `service::tts`) and an external speech server such as VoiceStudio
//! (see `service::speech_server`). Admin-only like the rest of the voice
//! surface.
//!
//! - `GET  /api/voice/tts/status`  → `{state, progress, error}`
//! - `POST /api/voice/tts/prepare` → start the first-use download/load
//! - `GET  /api/voice/tts/voices`  → `{default, voices, studio_configured,
//!   studio_voices}`
//! - `POST /api/voice/tts {text, voice?, speed?}` → `audio/wav`, or 503
//!   `{status}` until the model is ready. A `studio:<id>` voice goes to
//!   the speech server (502 `{error}` when it fails).
//! - `POST /api/voice/tts {text, phonemes}` → speaks exactly `phonemes`
//!   (Kokoro vocab only, else 400) — the Settings → Voice draft preview.
//! - `GET|PUT /api/voice/speech-server` → `{base_url, model, has_api_key}`
//!   (the key is write-only); `POST /api/voice/speech-server/test` →
//!   `{ok, voices?, version?, device?, error?}`.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;

use crate::auth::middleware::{require_admin, require_auth};
use crate::service::speech_server;
use crate::service::tts::{self, TtsError};
use crate::state::AppState;

#[derive(Deserialize)]
struct TtsRequest {
    text: String,
    #[serde(default)]
    voice: Option<String>,
    #[serde(default)]
    speed: Option<f32>,
    /// Speak exactly these Kokoro phonemes (a lexicon draft preview);
    /// `text` is still required as the label.
    #[serde(default)]
    phonemes: Option<String>,
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/voice/tts", post(synthesize))
        .route("/api/voice/tts/status", get(status))
        .route("/api/voice/tts/prepare", post(prepare))
        .route("/api/voice/tts/voices", get(voices))
        .route(
            "/api/voice/speech-server",
            get(get_speech_server).put(put_speech_server),
        )
        .route("/api/voice/speech-server/test", post(test_speech_server))
        .route_layer(middleware::from_fn(require_admin))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

async fn status(State(state): State<Arc<AppState>>) -> Json<tts::TtsStatus> {
    Json(tts::service_for(&state.config.data_dir).status())
}

async fn prepare(State(state): State<Arc<AppState>>) -> Json<tts::TtsStatus> {
    let svc = tts::service_for(&state.config.data_dir);
    svc.prepare();
    Json(svc.status())
}

/// Kokoro's voices, plus the speech server's under `studio_voices` (ids
/// without the `studio:` prefix; empty when none is configured or it is
/// unreachable).
async fn voices(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let list: Vec<_> = tts::VOICES
        .iter()
        .map(|(id, name)| {
            let lang = if id.starts_with('b') {
                "en-GB"
            } else {
                "en-US"
            };
            serde_json::json!({ "id": id, "name": name, "lang": lang })
        })
        .collect();
    let server = speech_server::load(&state.db).await.ok().flatten();
    let studio = match &server {
        Some(s) => speech_server::pickable(speech_server::voices_cached(s).await),
        None => Vec::new(),
    };
    Json(serde_json::json!({
        "default": tts::DEFAULT_VOICE,
        "voices": list,
        "studio_configured": server.is_some(),
        "studio_voices": studio,
    }))
}

async fn synthesize(State(state): State<Arc<AppState>>, Json(body): Json<TtsRequest>) -> Response {
    if let Some(voice) = body
        .voice
        .as_deref()
        .and_then(|v| v.strip_prefix(speech_server::VOICE_PREFIX))
    {
        return synthesize_remote(&state, &body, voice).await;
    }
    let svc = tts::service_for(&state.config.data_dir);
    let started = std::time::Instant::now();
    let lex = match tts::lexicon::store(&state.db).await {
        Ok(lex) => Some(lex),
        Err(e) => {
            tracing::warn!(error = %e, "tts lexicon unavailable");
            None
        }
    };
    match svc
        .synthesize(
            &body.text,
            body.voice.as_deref(),
            body.speed,
            body.phonemes.as_deref(),
            lex,
        )
        .await
    {
        Ok(wav) => {
            tracing::debug!(
                ms = started.elapsed().as_millis() as u64,
                bytes = wav.len(),
                "kokoro tts"
            );
            ([(header::CONTENT_TYPE, "audio/wav")], wav).into_response()
        }
        Err(TtsError::NotReady(st)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": st.state,
                "progress": st.progress,
                "error": st.error,
            })),
        )
            .into_response(),
        Err(TtsError::BadRequest(msg)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": msg })),
        )
            .into_response(),
        Err(e @ TtsError::Failed(_)) => {
            tracing::warn!(error = %e, "kokoro tts failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }
}
fn json_err(status: StatusCode, msg: &str) -> Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

/// A `studio:<id>` voice: forwarded to the speech server. Pronunciation
/// hints are Kokoro phonemes, so the server gets the plain words.
async fn synthesize_remote(state: &AppState, body: &TtsRequest, voice: &str) -> Response {
    let text = tts::hints::strip(body.text.trim());
    let text = text.trim();
    if text.is_empty() || voice.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "text and voice are required");
    }
    if text.chars().count() > speech_server::MAX_TEXT_CHARS {
        return json_err(
            StatusCode::BAD_REQUEST,
            &format!(
                "text is longer than {} characters",
                speech_server::MAX_TEXT_CHARS
            ),
        );
    }
    let server = match speech_server::load(&state.db).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return json_err(
                StatusCode::SERVICE_UNAVAILABLE,
                "no speech server is configured (Settings → Voice → Speech server)",
            );
        }
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let started = std::time::Instant::now();
    let speed = body.speed.unwrap_or(1.0).clamp(0.5, 2.0);
    match server.speak(text, voice, speed).await {
        Ok(wav) => {
            tracing::debug!(
                ms = started.elapsed().as_millis() as u64,
                bytes = wav.len(),
                "speech server tts"
            );
            ([(header::CONTENT_TYPE, "audio/wav")], wav).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, "speech server tts failed");
            json_err(StatusCode::BAD_GATEWAY, &e.to_string())
        }
    }
}

async fn get_speech_server(State(state): State<Arc<AppState>>) -> Response {
    match speech_server::view(&state.db).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
struct SpeechServerBody {
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

/// `PUT` — absent fields stay as they are; `""` clears one (an empty
/// `base_url` switches the speech server off).
async fn put_speech_server(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SpeechServerBody>,
) -> Response {
    match speech_server::save(
        &state.db,
        body.base_url.as_deref(),
        body.api_key.as_deref(),
        body.model.as_deref(),
    )
    .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => json_err(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}

/// Probe the saved server: `{ok, voices?, version?, device?, error?}`.
async fn test_speech_server(State(state): State<Arc<AppState>>) -> Response {
    match speech_server::load(&state.db).await {
        Ok(Some(s)) => Json(s.test().await).into_response(),
        Ok(None) => Json(serde_json::json!({
            "ok": false,
            "error": "set the speech server URL first",
        }))
        .into_response(),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::middleware::tests::{seed_authenticated_user, test_state};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn call(
        app: &Router,
        token: &str,
        method: &str,
        uri: &str,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or_default())
    }

    #[tokio::test]
    async fn status_voices_and_503_without_model() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;
        let app = router(state.clone()).with_state(state);

        let (s, v) = call(&app, &token, "GET", "/api/voice/tts/status", "").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["state"], "not_started");

        let (s, v) = call(&app, &token, "GET", "/api/voice/tts/voices", "").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["default"], "af_heart");
        assert!(
            v["voices"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x["id"] == "am_michael")
        );

        let (s, v) = call(
            &app,
            &token,
            "POST",
            "/api/voice/tts",
            r#"{"text":"Hello."}"#,
        )
        .await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(v["status"], "not_started");

        let long = "a".repeat(tts::MAX_TEXT_CHARS + 1);
        let (s, _) = call(
            &app,
            &token,
            "POST",
            "/api/voice/tts",
            &format!(r#"{{"text":"{long}"}}"#),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn non_admin_is_forbidden() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "user").await;
        let app = router(state.clone()).with_state(state);
        let (s, _) = call(&app, &token, "GET", "/api/voice/tts/status", "").await;
        assert_eq!(s, StatusCode::FORBIDDEN);
    }

    /// A stand-in VoiceStudio: voices + 24 kHz PCM speech, bearer-gated.
    async fn fake_speech_server() -> String {
        use axum::http::HeaderMap;
        fn authed(h: &HeaderMap) -> bool {
            h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer k1")
        }
        let app = Router::new()
            .route(
                "/v1/audio/voices",
                get(|h: HeaderMap| async move {
                    if !authed(&h) {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(serde_json::json!({"detail": "API key required"})),
                        )
                            .into_response();
                    }
                    Json(serde_json::json!({"voices": [{"voice_id": "v-ana", "name": "Ana"}]}))
                        .into_response()
                }),
            )
            .route(
                "/v1/audio/speech",
                post(
                    |h: HeaderMap, Json(b): Json<serde_json::Value>| async move {
                        assert!(authed(&h));
                        assert_eq!(b["response_format"], "pcm");
                        assert_eq!(b["voice"], "v-ana");
                        // Hint markup is stripped before it leaves Peckboard.
                        assert_eq!(b["input"], "Hello Kubernetes.");
                        vec![0u8; 480].into_response()
                    },
                ),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/v1")
    }

    #[tokio::test]
    async fn studio_voices_are_served_by_the_speech_server() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;
        let app = router(state.clone()).with_state(state);
        let url = fake_speech_server().await;

        // Unconfigured: 503 with a pointer to the setting.
        let (s, v) = call(
            &app,
            &token,
            "POST",
            "/api/voice/tts",
            r#"{"text":"Hi.","voice":"studio:v-ana"}"#,
        )
        .await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
        assert!(v["error"].as_str().unwrap().contains("Speech server"));

        // A wrong key fails the connection test with the server's message.
        let body = format!(r#"{{"base_url":"{url}","api_key":"bad"}}"#);
        let (s, v) = call(&app, &token, "PUT", "/api/voice/speech-server", &body).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["has_api_key"], true);
        assert!(v.get("api_key").is_none());
        assert!(!v["base_url"].as_str().unwrap().ends_with("/v1"));
        let (_, v) = call(&app, &token, "POST", "/api/voice/speech-server/test", "").await;
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("API key required"));

        let (_, v) = call(
            &app,
            &token,
            "PUT",
            "/api/voice/speech-server",
            r#"{"api_key":"k1"}"#,
        )
        .await;
        assert_eq!(v["has_api_key"], true);
        let (_, v) = call(&app, &token, "POST", "/api/voice/speech-server/test", "").await;
        assert_eq!(v["ok"], true);
        assert_eq!(v["voices"], 1);

        let (_, v) = call(&app, &token, "GET", "/api/voice/tts/voices", "").await;
        assert_eq!(v["studio_configured"], true);
        assert_eq!(
            v["studio_voices"][0],
            serde_json::json!({"id": "v-ana", "name": "Ana"})
        );

        let req = Request::builder()
            .method("POST")
            .uri("/api/voice/tts")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"text":"Hello [Kubernetes](/kˌʌbənˈɛɹnətəs/).","voice":"studio:v-ana"}"#,
            ))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["content-type"], "audio/wav");
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(bytes.len(), 44 + 480);

        // A merged reply clip may be long, but not unbounded.
        let long = "a".repeat(speech_server::MAX_TEXT_CHARS + 1);
        let body = format!(r#"{{"text":"{long}","voice":"studio:v-ana"}}"#);
        let (s, v) = call(&app, &token, "POST", "/api/voice/tts", &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"].as_str().unwrap().contains("1500"), "{v}");
    }
}

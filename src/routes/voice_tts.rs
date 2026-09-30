//! `/api/voice/tts*` — server-side Kokoro speech for the voice assistant
//! (see `service::tts`). Admin-only like the rest of the voice surface.
//!
//! - `GET  /api/voice/tts/status`  → `{state, progress, error}`
//! - `POST /api/voice/tts/prepare` → start the first-use download/load
//! - `GET  /api/voice/tts/voices`  → `[{id, name, lang}]`
//! - `POST /api/voice/tts {text, voice?, speed?}` → `audio/wav`, or 503
//!   `{status}` until the model is ready.

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
use crate::service::tts::{self, TtsError};
use crate::state::AppState;

#[derive(Deserialize)]
struct TtsRequest {
    text: String,
    #[serde(default)]
    voice: Option<String>,
    #[serde(default)]
    speed: Option<f32>,
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/voice/tts", post(synthesize))
        .route("/api/voice/tts/status", get(status))
        .route("/api/voice/tts/prepare", post(prepare))
        .route("/api/voice/tts/voices", get(voices))
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

async fn voices() -> Json<serde_json::Value> {
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
    Json(serde_json::json!({ "default": tts::DEFAULT_VOICE, "voices": list }))
}

async fn synthesize(State(state): State<Arc<AppState>>, Json(body): Json<TtsRequest>) -> Response {
    let svc = tts::service_for(&state.config.data_dir);
    let started = std::time::Instant::now();
    match svc
        .synthesize(&body.text, body.voice.as_deref(), body.speed)
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
}

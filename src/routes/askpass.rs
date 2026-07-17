//! Sudo askpass HTTP surface (see `service::askpass` for the full flow).
//!
//! `POST /api/askpass` is called by the generated `askpass.sh` helper from
//! inside a session's `sudo -A` — it is NOT behind the JWT middleware and is
//! instead gated by the per-session secret token dispatch placed in the
//! child's environment. `POST /api/sessions/{id}/askpass-answer` is the
//! JWT-authenticated route the UI's password dialog submits to.

use axum::{
    Form, Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    middleware,
    response::IntoResponse,
    routing::post,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::auth::middleware::require_auth;
use crate::service::askpass::ANSWER_TIMEOUT_SECS;
use crate::state::AppState;
use crate::ws::broadcaster::WsEvent;

const TOKEN_HEADER: &str = "x-peckboard-askpass-token";

/// Keep prompts dialog-sized; sudo's default is short ("[sudo] password
/// for user:") but the helper forwards whatever it was given.
const MAX_PROMPT_LEN: usize = 300;

/// Sanity cap on submitted passwords — far above any real password, far
/// below abuse territory.
const MAX_PASSWORD_LEN: usize = 1024;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let protected = Router::new()
        .route("/api/sessions/{id}/askpass-answer", post(answer))
        .route_layer(middleware::from_fn_with_state(state, require_auth));

    // Helper-facing endpoint: token-gated, no JWT (the CLI child has no
    // user credentials — same model as the MCP route).
    let public = Router::new().route("/api/askpass", post(request_password));

    public.merge(protected)
}

#[derive(Deserialize)]
struct AskpassForm {
    #[serde(default)]
    prompt: String,
}

/// POST /api/askpass — long-polls until the user answers the masked dialog
/// (or the timeout hits). The response body is the raw password, which the
/// helper prints to stdout for sudo; it is never logged or persisted.
async fn request_password(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(body): Form<AskpassForm>,
) -> impl IntoResponse {
    let Some(registry) = state.session_manager.askpass_registry() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "askpass is not enabled on this server".to_string(),
        );
    };
    let registry = registry.clone();
    let token = headers
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let Some(session_id) = registry.session_for_token(token).await else {
        return (
            StatusCode::UNAUTHORIZED,
            "unknown askpass token".to_string(),
        );
    };

    let mut prompt = body.prompt.trim().to_string();
    if prompt.is_empty() {
        prompt = "Password:".into();
    }
    if prompt.len() > MAX_PROMPT_LEN {
        prompt = prompt.chars().take(MAX_PROMPT_LEN).collect();
    }

    let (request_id, rx) = registry.begin_request().await;
    // Global WS event (see ws::handler) — the dialog must surface even on a
    // client that never subscribed to this session's stream.
    state.broadcaster.broadcast(WsEvent {
        event_type: "askpass-request".into(),
        session_id: session_id.clone(),
        data: serde_json::json!({
            "request_id": request_id,
            "session_id": session_id,
            "prompt": prompt,
        }),
    });

    let outcome =
        tokio::time::timeout(std::time::Duration::from_secs(ANSWER_TIMEOUT_SECS), rx).await;

    // Whatever happened, tell every tab the request is over so stale
    // dialogs close (the answering tab already knew).
    let broadcast_resolved = |reason: &str| {
        state.broadcaster.broadcast(WsEvent {
            event_type: "askpass-resolved".into(),
            session_id: session_id.clone(),
            data: serde_json::json!({ "request_id": request_id, "reason": reason }),
        });
    };

    match outcome {
        Ok(Ok(Some(password))) => {
            broadcast_resolved("answered");
            (StatusCode::OK, password)
        }
        Ok(Ok(None)) => {
            broadcast_resolved("cancelled");
            (StatusCode::FORBIDDEN, "askpass cancelled by user".into())
        }
        // Sender dropped without an answer — server shutdown or a bug.
        Ok(Err(_)) => {
            broadcast_resolved("dropped");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "askpass request dropped".into(),
            )
        }
        Err(_) => {
            registry.drop_request(&request_id).await;
            broadcast_resolved("timeout");
            (
                StatusCode::REQUEST_TIMEOUT,
                "askpass timed out waiting for the user".into(),
            )
        }
    }
}

#[derive(Deserialize)]
struct AnswerBody {
    request_id: String,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    cancel: bool,
}

/// POST /api/sessions/{id}/askpass-answer — the password dialog submits
/// here. `{request_id, password}` answers; `{request_id, cancel: true}`
/// rejects (sudo sees a failed askpass and gives up immediately).
async fn answer(
    State(state): State<Arc<AppState>>,
    Path(_session_id): Path<String>,
    Json(body): Json<AnswerBody>,
) -> impl IntoResponse {
    let Some(registry) = state.session_manager.askpass_registry() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "askpass is not enabled on this server" })),
        );
    };
    let answer = if body.cancel {
        None
    } else {
        let pw = body.password.unwrap_or_default();
        if pw.len() > MAX_PASSWORD_LEN {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "password too long" })),
            );
        }
        Some(pw)
    };
    if registry.resolve(&body.request_id, answer).await {
        (StatusCode::OK, Json(serde_json::json!({ "ok": true })))
    } else {
        // Already answered elsewhere, timed out, or bogus id.
        (
            StatusCode::GONE,
            Json(serde_json::json!({ "error": "request no longer pending" })),
        )
    }
}

//! `/api/voice/prompt*` — the voice assistant's editable system prompt (see
//! `service::voice_prompt`). Reads need a login; writes are admin-only, like
//! the voice session itself. A write applies from the voice session's next
//! turn — no restart, and an in-flight turn is never cut short.
//!
//! - `GET  /api/voice/prompt` → `{content, source, is_default, updated_at, default_content}`
//! - `PUT  /api/voice/prompt {content, note?}` → `{id, changed, diff, diff_stats}`, 400 `{error}`
//! - `POST /api/voice/prompt/reset` → same shape as PUT
//! - `GET  /api/voice/prompt/history` → `[{id, source, note, created_at, created_by, diff_stats}]`
//! - `GET  /api/voice/prompt/history/{id}` → `{id, …, content, diff, diff_stats}`, 404

use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;

use crate::auth::middleware::{AuthUser, require_admin, require_auth};
use crate::service::voice_prompt::{self, Change, PromptError, SOURCE_USER};
use crate::state::AppState;

#[derive(Deserialize)]
struct PutBody {
    content: String,
    #[serde(default)]
    note: Option<String>,
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let reads = Router::new()
        .route("/api/voice/prompt", get(current))
        .route("/api/voice/prompt/history", get(history))
        .route("/api/voice/prompt/history/{id}", get(version))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));
    let writes = Router::new()
        .route("/api/voice/prompt", axum::routing::put(save))
        .route("/api/voice/prompt/reset", post(reset))
        .route_layer(middleware::from_fn(require_admin))
        .route_layer(middleware::from_fn_with_state(state, require_auth));
    reads.merge(writes)
}

fn err(status: StatusCode, msg: impl ToString) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": msg.to_string() })),
    )
        .into_response()
}

fn internal(e: impl ToString) -> Response {
    err(StatusCode::INTERNAL_SERVER_ERROR, e)
}

async fn current(State(state): State<Arc<AppState>>) -> Response {
    match voice_prompt::active(&state.db).await {
        Ok(a) => Json(a).into_response(),
        Err(e) => internal(e),
    }
}

async fn history(State(state): State<Arc<AppState>>) -> Response {
    match voice_prompt::history(&state.db).await {
        Ok(h) => Json(h).into_response(),
        Err(e) => internal(e),
    }
}

async fn version(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match voice_prompt::version(&state.db, &id).await {
        Ok(Some(v)) => Json(v).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "no such version"),
        Err(e) => internal(e),
    }
}

/// Push a saved change to the voice session (next turn) and answer.
async fn applied(state: &AppState, result: Result<Change, PromptError>) -> Response {
    let change = match result {
        Ok(c) => c,
        Err(PromptError::Invalid(msg)) => return err(StatusCode::BAD_REQUEST, msg),
        Err(PromptError::Failed(e)) => return internal(e),
    };
    if let Err(e) =
        voice_prompt::apply_to_voice_session(&state.db, Some(&state.provider_registry)).await
    {
        return internal(e);
    }
    Json(change).into_response()
}

async fn save(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<PutBody>,
) -> Response {
    let result = voice_prompt::save(
        &state.db,
        &body.content,
        SOURCE_USER,
        body.note,
        Some(user.user_id.clone()),
    )
    .await;
    applied(&state, result).await
}

async fn reset(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
) -> Response {
    let result = voice_prompt::reset(&state.db, Some(user.user_id.clone())).await;
    applied(&state, result).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::middleware::tests::{
        seed_authenticated_user, seed_authenticated_user_with_suffix, test_state,
    };
    use axum::body::Body;
    use axum::http::{Request, header};
    use tower::ServiceExt;

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        token: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn prompt_routes_save_history_reset() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let admin = seed_authenticated_user(&state, "admin").await;
        let user = seed_authenticated_user_with_suffix(&state, "user", "c").await;
        let app = router(state.clone()).with_state(state.clone());

        let (s, cur) = call(&app, "GET", "/api/voice/prompt", &user, None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(cur["is_default"], true);

        let body = serde_json::json!({"content": "Be terse.", "note": "short"});
        let (s, _) = call(&app, "PUT", "/api/voice/prompt", &user, Some(body.clone())).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _) = call(
            &app,
            "PUT",
            "/api/voice/prompt",
            &admin,
            Some(serde_json::json!({"content": " "})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, saved) = call(&app, "PUT", "/api/voice/prompt", &admin, Some(body)).await;
        assert_eq!(s, StatusCode::OK, "{saved}");
        assert_eq!(saved["changed"], true);

        let (_, cur) = call(&app, "GET", "/api/voice/prompt", &admin, None).await;
        assert_eq!(cur["content"], "Be terse.");
        assert_eq!(cur["source"], "user");

        let (_, hist) = call(&app, "GET", "/api/voice/prompt/history", &admin, None).await;
        let id = hist[0]["id"].as_str().unwrap().to_string();
        assert_eq!(hist[0]["note"], "short");
        let (s, v) = call(
            &app,
            "GET",
            &format!("/api/voice/prompt/history/{id}"),
            &admin,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(v["diff"].as_str().unwrap().contains("+Be terse."));

        let (s, _) = call(&app, "POST", "/api/voice/prompt/reset", &admin, None).await;
        assert_eq!(s, StatusCode::OK);
        let (_, cur) = call(&app, "GET", "/api/voice/prompt", &admin, None).await;
        assert_eq!(cur["is_default"], true);
    }
}

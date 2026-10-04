//! Assistant conversation mirror settings (`service::assistant_mirror`).
//! One instance-global config, admin-only like the voice session itself.
//!
//! - `GET  /api/assistant/mirror` → config + per-channel status + `watched`.
//!   Secrets come back only as `webhook_set` / `password_set`.
//! - `PUT  /api/assistant/mirror` → partial update; secret fields are
//!   write-only (omitted = keep, `""` = clear). Invalid input is a 400 with
//!   `{"errors": {"<field.path>": "<message>"}}`.
//! - `POST /api/assistant/mirror/test` `{"channel": "slack"|"discord"|"email"}`
//!   → `{"ok": bool, "error"?: string}`.

use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::State,
    http::StatusCode,
    middleware,
    routing::{get, post},
};
use serde::Deserialize;

use crate::auth::middleware::{AuthUser, require_auth};
use crate::service::assistant_mirror::settings::MirrorPatch;
use crate::service::assistant_mirror::{AssistantMirror, Channel, UpdateError};
use crate::state::AppState;

type ApiError = (StatusCode, Json<serde_json::Value>);

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/assistant/mirror", get(get_mirror).put(put_mirror))
        .route("/api/assistant/mirror/test", post(test_mirror))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

fn err(status: StatusCode, msg: impl Into<String>) -> ApiError {
    (status, Json(serde_json::json!({ "error": msg.into() })))
}

fn admin_only(user: &AuthUser) -> Result<(), ApiError> {
    if user.is_admin() {
        Ok(())
    } else {
        Err(err(
            StatusCode::FORBIDDEN,
            "the Assistant mirror is admin-only",
        ))
    }
}

async fn get_mirror(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    admin_only(&user)?;
    Ok(Json(AssistantMirror::of(&state).await.wire()))
}

async fn put_mirror(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    admin_only(&user)?;
    let patch: MirrorPatch = serde_json::from_slice(&body)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("invalid body: {e}")))?;
    let mirror = AssistantMirror::of(&state).await;
    match mirror.update(patch).await {
        Ok(_) => Ok(Json(mirror.wire())),
        Err(UpdateError::Invalid(errors)) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "errors": errors })),
        )),
        Err(UpdateError::Storage(e)) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

#[derive(Deserialize)]
struct TestRequest {
    channel: String,
}

async fn test_mirror(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<TestRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    admin_only(&user)?;
    let channel = Channel::parse(&body.channel).ok_or_else(|| {
        err(
            StatusCode::BAD_REQUEST,
            "channel must be slack, discord, or email",
        )
    })?;
    let result = AssistantMirror::of(&state).await.send_test(channel).await;
    Ok(Json(match result {
        Ok(()) => serde_json::json!({ "ok": true }),
        Err(e) => serde_json::json!({ "ok": false, "error": e }),
    }))
}

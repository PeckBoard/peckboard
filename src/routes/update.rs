//! `/api/update/*` — self-update for the core binary (bare-process model).
//!
//! `GET /api/update/check` reports the running version and whether a newer
//! release exists. `POST /api/update/apply` downloads + checksum-verifies the
//! new binary, atomically swaps it in, sends its response, then re-execs into
//! it. Both are behind `require_auth` (same bar as installing a plugin).

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{get, post},
};

use crate::auth::middleware::require_auth;
use crate::service::update;
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/update/check", get(check_update))
        .route("/api/update/apply", post(apply_update))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// A short-lived client for the GitHub API + release CDN.
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap_or_default()
}

async fn check_update(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    match update::check(&http_client()).await {
        Ok(status) => Json(status).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn apply_update(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    let client = http_client();

    // Re-check so we apply exactly the release we'd report — and refuse if
    // there's nothing newer or the platform isn't supported.
    let status = match update::check(&client).await {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    if !status.supported {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "self-update is not supported on this platform" })),
        )
            .into_response();
    }
    let Some(tag) = status.latest_version.clone() else {
        return (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": "could not determine the latest version" })),
        )
            .into_response();
    };
    if !status.update_available {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "already up to date",
                "current_version": status.current_version,
                "latest_version": tag,
            })),
        )
            .into_response();
    }

    // Download, verify, and swap the binary on disk.
    match update::download_and_swap(&client, &tag).await {
        Ok(_sha) => {
            // Re-exec AFTER this response is flushed. exec() replaces the
            // process image, so do it from a detached task on a short delay.
            tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                tracing::warn!("Self-update applied — re-exec into the new binary");
                if let Err(e) = update::restart() {
                    tracing::error!("Self-update re-exec failed: {e}");
                }
            });
            Json(serde_json::json!({ "ok": true, "restarting": true, "version": tag }))
                .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

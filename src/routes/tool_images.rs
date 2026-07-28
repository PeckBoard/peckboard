//! Serves tool-image blobs offloaded from `agent-tool-end` events.
//!
//! `emit_event` writes each tool-returned image (e.g. a Playwright MCP
//! screenshot) to `<data_dir>/tool-images/<session_id>/<image_id>` and the
//! event carries only `{mimeType, id}`; this route hands the bytes back to
//! the chat. Same auth posture as the sessions routes: JWT + per-session
//! ownership check (`require_session_access`).

use crate::auth::middleware::{require_auth, require_session_access};
use crate::service::tool_images;
use crate::state::AppState;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{StatusCode, header},
    middleware,
    response::IntoResponse,
    routing::get,
};
use std::sync::Arc;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/sessions/{id}/tool-images/{image_id}",
            get(download_tool_image),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_session_access,
        ))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// MIME types served as-is so the browser can render them; anything else
/// (a corrupted sidecar, a non-image smuggled through) degrades to a
/// generic byte stream.
fn safe_image_mime(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

/// GET /api/sessions/:id/tool-images/:image_id
async fn download_tool_image(
    State(state): State<Arc<AppState>>,
    Path((session_id, image_id)): Path<(String, String)>,
) -> impl IntoResponse {
    if !tool_images::is_safe_id(&session_id) || !tool_images::is_safe_id(&image_id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid id" })),
        ));
    }

    let Some((mime, bytes)) =
        tool_images::read(&state.config.data_dir, &session_id, &image_id).await
    else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "tool image not found" })),
        ));
    };

    let content_type = if safe_image_mime(&mime) {
        mime
    } else {
        "application/octet-stream".to_string()
    };

    Ok::<_, (StatusCode, Json<serde_json::Value>)>((
        [
            (header::CONTENT_TYPE, content_type),
            // Blobs are immutable — a given id never changes bytes.
            (
                header::CACHE_CONTROL,
                "private, max-age=31536000, immutable".to_string(),
            ),
            (
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff".to_string(),
            ),
            (
                header::HeaderName::from_static("content-security-policy"),
                "default-src 'none'; sandbox".to_string(),
            ),
        ],
        bytes,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_whitelist_gates_renderable_types() {
        assert!(safe_image_mime("image/png"));
        assert!(safe_image_mime("image/webp"));
        assert!(!safe_image_mime("text/html"));
        assert!(!safe_image_mime("image/svg+xml")); // scriptable — never render
    }
}

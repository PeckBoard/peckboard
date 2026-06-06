use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{get, post},
};
use serde::Deserialize;
use std::sync::Arc;

use crate::auth::middleware::{AuthUser, require_auth};
use crate::db::models::{NewAnnouncement, NewPushSubscription, NewQueuedMessage};
use crate::state::AppState;

// ── Request types ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct SubscribeRequest {
    endpoint: String,
    p256dh: String,
    auth_key: String,
}

#[derive(Deserialize)]
struct UnsubscribeRequest {
    endpoint: String,
}

#[derive(Deserialize)]
struct CreateAnnouncementRequest {
    kind: String,
    title: String,
    message: String,
    detail: Option<String>,
}

#[derive(Deserialize)]
struct QueueMessageRequest {
    text: String,
}

// ── Router ─────────────────────────────────────────────────────────

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/push/subscribe",
            post(subscribe).delete(unsubscribe),
        )
        .route(
            "/api/announcements",
            get(list_announcements).post(create_announcement),
        )
        .route("/api/announcements/{id}", axum::routing::delete(delete_announcement))
        .route(
            "/api/sessions/{id}/queue",
            post(upsert_queued_message)
                .get(get_queued_message)
                .delete(delete_queued_message),
        )
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

// ── Push subscribe / unsubscribe ───────────────────────────────────

/// POST /api/push/subscribe
async fn subscribe(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SubscribeRequest>,
) -> impl IntoResponse {
    let now = chrono::Utc::now().to_rfc3339();

    let sub = state
        .db
        .create_push_subscription(NewPushSubscription {
            endpoint: body.endpoint,
            p256dh: body.p256dh,
            auth_key: body.auth_key,
            created_at: now,
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>((
        StatusCode::CREATED,
        Json(serde_json::json!(sub)),
    ))
}

/// DELETE /api/push/subscribe
async fn unsubscribe(
    State(state): State<Arc<AppState>>,
    Json(body): Json<UnsubscribeRequest>,
) -> impl IntoResponse {
    let deleted = state
        .db
        .delete_push_subscription(&body.endpoint)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    if !deleted {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "subscription not found" })),
        ));
    }

    Ok(StatusCode::NO_CONTENT)
}

// ── Announcements ──────────────────────────────────────────────────

/// GET /api/announcements
async fn list_announcements(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let announcements = state.db.list_announcements().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!(announcements)))
}

/// POST /api/announcements (admin only)
async fn create_announcement(
    State(state): State<Arc<AppState>>,
    request: axum::http::Request<axum::body::Body>,
) -> impl IntoResponse {
    let auth_user = request
        .extensions()
        .get::<AuthUser>()
        .expect("auth middleware should inject AuthUser");

    if !auth_user.is_admin() {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "admin access required" })),
        ));
    }

    // We already consumed `request` for the extension check, so we need to
    // extract the body manually.
    let body: CreateAnnouncementRequest =
        serde_json::from_slice(
            &axum::body::to_bytes(request.into_body(), 1024 * 64)
                .await
                .map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({ "error": e.to_string() })),
                    )
                })?,
        )
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    let now = chrono::Utc::now().to_rfc3339();
    let id = uuid::Uuid::new_v4().to_string();

    let announcement = state
        .db
        .create_announcement(NewAnnouncement {
            id,
            kind: body.kind,
            title: body.title,
            message: body.message,
            detail: body.detail,
            created_at: now,
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>((
        StatusCode::CREATED,
        Json(serde_json::json!(announcement)),
    ))
}

/// DELETE /api/announcements/:id
async fn delete_announcement(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let deleted = state.db.delete_announcement(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    if !deleted {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "announcement not found" })),
        ));
    }

    Ok(StatusCode::NO_CONTENT)
}

// ── Queued Messages ────────────────────────────────────────────────

/// POST /api/sessions/:id/queue
async fn upsert_queued_message(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    Json(body): Json<QueueMessageRequest>,
) -> impl IntoResponse {
    let now = chrono::Utc::now().to_rfc3339();

    let msg = state
        .db
        .upsert_queued_message(NewQueuedMessage {
            session_id,
            text: body.text,
            queued_at: now,
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>((
        StatusCode::CREATED,
        Json(serde_json::json!(msg)),
    ))
}

/// GET /api/sessions/:id/queue
async fn get_queued_message(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    let msg = state
        .db
        .get_queued_message(&session_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    match msg {
        Some(m) => Ok(Json(serde_json::json!(m))),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no queued message" })),
        )),
    }
}

/// DELETE /api/sessions/:id/queue
async fn delete_queued_message(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    let deleted = state
        .db
        .delete_queued_message(&session_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    if !deleted {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no queued message" })),
        ));
    }

    Ok(StatusCode::NO_CONTENT)
}

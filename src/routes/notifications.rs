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

use crate::auth::middleware::{AuthUser, require_auth, require_session_access};
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
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
}

// ── Router ─────────────────────────────────────────────────────────

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let public = Router::new().route("/api/push/vapid-key", get(get_vapid_key));

    let protected = Router::new()
        .route("/api/push/subscribe", post(subscribe).delete(unsubscribe))
        .route(
            "/api/announcements",
            get(list_announcements).post(create_announcement),
        )
        .route(
            "/api/announcements/{id}",
            axum::routing::delete(delete_announcement),
        )
        .merge(session_scoped(state.clone()))
        .route_layer(middleware::from_fn_with_state(state, require_auth));

    public.merge(protected)
}

/// The queued-message routes read and write the messages that will be sent
/// into a specific session, so they carry the same owner-or-shared-board
/// gate as the session routes themselves (`require_session_access`) rather
/// than being reachable by any logged-in user.
fn session_scoped(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/sessions/{id}/queue",
            post(enqueue_queued_message)
                .get(list_queued_messages)
                .delete(clear_queued_messages),
        )
        .route(
            "/api/sessions/{id}/queue/{msg_id}",
            axum::routing::delete(delete_queued_message),
        )
        .route(
            "/api/sessions/{id}/queue/{msg_id}/force",
            post(force_queued_message),
        )
        .route_layer(middleware::from_fn_with_state(
            state,
            require_session_access,
        ))
}

// ── VAPID public key ──────────────────────────────────────────────

/// GET /api/push/vapid-key — returns the VAPID public key for push subscriptions.
async fn get_vapid_key(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(serde_json::json!({
        "publicKey": state.push_service.vapid_public_key
    }))
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
async fn list_announcements(State(state): State<Arc<AppState>>) -> impl IntoResponse {
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
    let body: CreateAnnouncementRequest = serde_json::from_slice(
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

    state.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
        event_type: "announcement".into(),
        session_id: String::new(),
        data: serde_json::json!({ "action": "created", "id": announcement.id, "title": announcement.title, "message": announcement.message }),
    });

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

    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "announcement".into(),
            session_id: String::new(),
            data: serde_json::json!({ "action": "dismissed", "id": id }),
        });

    Ok(StatusCode::NO_CONTENT)
}

// ── Queued Messages ──────────────────────────────────

/// POST /api/sessions/:id/queue — append a message to the session's FIFO
/// queue. Machine path (the UI sends through /message, which queues
/// internally when the agent is busy); no `user` event is appended here,
/// so the drain records one at delivery.
async fn enqueue_queued_message(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    Json(body): Json<QueueMessageRequest>,
) -> impl IntoResponse {
    let now = chrono::Utc::now().to_rfc3339();
    let broadcast_session_id = session_id.clone();

    let msg = state
        .db
        .enqueue_message(NewQueuedMessage {
            session_id,
            text: body.text,
            queued_at: now,
            model: body.model,
            effort: body.effort,
            attachment_ids: None,
            user_event_appended: false,
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "queue".into(),
            session_id: broadcast_session_id,
            // No `text` on the wire — subscribers refetch the durable list
            // through the access-checked queue endpoint.
            data: serde_json::json!({ "action": "set", "id": msg.id }),
        });

    Ok::<_, (StatusCode, Json<serde_json::Value>)>((
        StatusCode::CREATED,
        Json(serde_json::json!(msg)),
    ))
}

/// GET /api/sessions/:id/queue — every queued message, oldest (next to
/// deliver) first. Always 200; an empty queue is `{ "messages": [] }`.
async fn list_queued_messages(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    let messages = state
        .db
        .list_queued_messages(&session_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(
        serde_json::json!({ "messages": messages }),
    ))
}

/// DELETE /api/sessions/:id/queue — drop every queued message.
async fn clear_queued_messages(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    let deleted = state
        .db
        .clear_queued_messages(&session_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    if deleted == 0 {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no queued message" })),
        ));
    }

    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "queue".into(),
            session_id: session_id.clone(),
            data: serde_json::json!({ "action": "deleted" }),
        });

    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /api/sessions/:id/queue/:msg_id — remove one queued message
/// (the ✕ on its chip). 404 when the row is already gone — e.g. the
/// drain delivered it between the click and the request.
async fn delete_queued_message(
    State(state): State<Arc<AppState>>,
    Path((session_id, msg_id)): Path<(String, i64)>,
) -> impl IntoResponse {
    let deleted = state
        .db
        .delete_queued_message_by_id(&session_id, msg_id)
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

    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "queue".into(),
            session_id: session_id.clone(),
            data: serde_json::json!({ "action": "deleted", "id": msg_id }),
        });

    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/sessions/:id/queue/:msg_id/force — the per-message "send
/// now" button. Pops the row and delivers it immediately: a mid-stream
/// provider (Claude) gets it injected into the live turn (which steers/
/// interrupts the agent's current work); a per-turn provider has its run
/// cancelled first. Queue order for the remaining rows is untouched.
async fn force_queued_message(
    State(state): State<Arc<AppState>>,
    Path((session_id, msg_id)): Path<(String, i64)>,
) -> impl IntoResponse {
    let err500 = |e: String| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e })),
        )
    };

    let Some(session) = state
        .db
        .get_session(&session_id)
        .await
        .map_err(|e| err500(e.to_string()))?
    else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        ));
    };

    let Some(queued) = state
        .db
        .get_queued_message_by_id(&session_id, msg_id)
        .await
        .map_err(|e| err500(e.to_string()))?
    else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no queued message" })),
        ));
    };

    // Pop the row BEFORE dispatching so the completion listener's drain
    // (or a double-click) can't deliver it a second time.
    if !state
        .db
        .delete_queued_message_by_id(&session_id, msg_id)
        .await
        .map_err(|e| err500(e.to_string()))?
    {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no queued message" })),
        ));
    }

    let config = crate::worker::orchestrator::queued_resume_config(
        &state,
        &session,
        queued.model.clone(),
        queued.effort.clone(),
    )
    .await;

    state
        .session_manager
        .force_queued(
            &session_id,
            queued,
            &state.db,
            &state.broadcaster,
            config,
            &state.config.data_dir,
        )
        .await
        .map_err(|e| err500(e.to_string()))?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(
        serde_json::json!({ "status": "forced", "id": msg_id }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::middleware::tests::{seed_authenticated_user, seed_session, test_state};
    use axum::body::Body;
    use axum::http::{Request, header};
    use tower::ServiceExt;

    fn app(state: Arc<AppState>) -> Router {
        Router::new().merge(router(state.clone())).with_state(state)
    }

    fn queue_request(token: &str, session_id: &str) -> Request<Body> {
        Request::builder()
            .uri(format!("/api/sessions/{session_id}/queue"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    }

    async fn seed_queued_message(state: &Arc<AppState>, session_id: &str) {
        state
            .db
            .enqueue_message(NewQueuedMessage {
                session_id: session_id.into(),
                text: "queued-secret".into(),
                queued_at: chrono::Utc::now().to_rfc3339(),
                model: None,
                effort: None,
                attachment_ids: None,
                user_event_appended: false,
            })
            .await
            .unwrap();
    }

    async fn body_text(response: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).to_string()
    }

    /// The queued message is text that will be sent into the session, so
    /// reading it is reading the session. A non-owner gets the same 404 the
    /// other session routes return — note the message really does exist, so
    /// this is the gate answering, not the "no queued message" 404.
    #[tokio::test]
    async fn non_owner_cannot_read_another_users_queued_message() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "user").await;
        seed_session(&state, "s-theirs", Some("u2"), None).await;
        seed_queued_message(&state, "s-theirs").await;

        let response = app(state.clone())
            .oneshot(queue_request(&token, "s-theirs"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = body_text(response).await;
        assert!(body.contains("session not found"), "got {body}");
        assert!(!body.contains("queued-secret"));
    }

    #[tokio::test]
    async fn non_owner_cannot_queue_a_message_into_another_users_session() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "user").await;
        seed_session(&state, "s-theirs", Some("u2"), None).await;

        let request = Request::builder()
            .method("POST")
            .uri("/api/sessions/s-theirs/queue")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "text": "do something" }).to_string(),
            ))
            .unwrap();
        let response = app(state.clone()).oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            state
                .db
                .list_queued_messages("s-theirs")
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A non-owner must not be able to force or delete a queued message
    /// by id either — those routes dispatch into / mutate the session.
    #[tokio::test]
    async fn non_owner_cannot_force_or_delete_queued_message() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "user").await;
        seed_session(&state, "s-theirs", Some("u2"), None).await;
        seed_queued_message(&state, "s-theirs").await;
        let msg_id = state.db.list_queued_messages("s-theirs").await.unwrap()[0].id;

        for (method, uri) in [
            (
                "POST",
                format!("/api/sessions/s-theirs/queue/{msg_id}/force"),
            ),
            ("DELETE", format!("/api/sessions/s-theirs/queue/{msg_id}")),
        ] {
            let request = Request::builder()
                .method(method)
                .uri(&uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap();
            let response = app(state.clone()).oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {uri}");
        }
        assert_eq!(
            state
                .db
                .list_queued_messages("s-theirs")
                .await
                .unwrap()
                .len(),
            1,
            "gate must not consume the row"
        );
    }

    #[tokio::test]
    async fn owner_can_read_their_own_queued_message() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "user").await;
        seed_session(&state, "s-mine", Some("u1"), None).await;
        seed_queued_message(&state, "s-mine").await;

        let response = app(state.clone())
            .oneshot(queue_request(&token, "s-mine"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_text(response).await.contains("queued-secret"));
    }
}

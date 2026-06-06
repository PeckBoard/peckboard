use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{get, post},
};
use serde::Deserialize;
use std::sync::Arc;

use crate::auth::middleware::require_auth;
use crate::db::models::{NewSession, UpdateSession};
use crate::state::AppState;

#[derive(Deserialize)]
struct CreateSessionRequest {
    name: String,
    folder_id: String,
    model: Option<String>,
    effort: Option<String>,
}

#[derive(Deserialize)]
struct ListSessionsQuery {
    folder_id: Option<String>,
}

#[derive(Deserialize)]
struct UpdateSessionRequest {
    name: Option<String>,
    model: Option<Option<String>>,
    effort: Option<Option<String>>,
    project_id: Option<Option<String>>,
    card_id: Option<Option<String>>,
    conversation_id: Option<Option<String>>,
    last_activity: Option<String>,
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/sessions", post(create_session).get(list_sessions))
        .route(
            "/api/sessions/{id}",
            get(get_session).patch(update_session).delete(delete_session),
        )
        .route("/api/sessions/{id}/events", get(list_events).post(append_event))
        .route("/api/sessions/{id}/read", post(mark_read))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

#[derive(Deserialize)]
struct EventsQuery {
    after_seq: Option<i32>,
    limit: Option<i64>,
}

#[derive(Deserialize)]
struct AppendEventRequest {
    kind: String,
    data: serde_json::Value,
}

/// POST /api/sessions
async fn create_session(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateSessionRequest>,
) -> impl IntoResponse {
    let now = chrono::Utc::now().to_rfc3339();
    let id = uuid::Uuid::new_v4().to_string();

    let session = state
        .db
        .create_session(NewSession {
            id,
            name: body.name,
            folder_id: body.folder_id,
            model: body.model,
            effort: body.effort,
            is_worker: false,
            project_id: None,
            card_id: None,
            conversation_id: None,
            created_at: now.clone(),
            last_activity: now,
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
        Json(serde_json::json!(session)),
    ))
}

/// GET /api/sessions
async fn list_sessions(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListSessionsQuery>,
) -> impl IntoResponse {
    let sessions = if let Some(folder_id) = params.folder_id {
        state.db.list_sessions_by_folder(&folder_id).await
    } else {
        state.db.list_sessions().await
    };

    let sessions = sessions.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!(sessions)))
}

/// GET /api/sessions/:id
async fn get_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let session = state.db.get_session(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    match session {
        Some(s) => Ok(Json(serde_json::json!(s))),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        )),
    }
}

/// PATCH /api/sessions/:id
async fn update_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<UpdateSessionRequest>,
) -> impl IntoResponse {
    let update = UpdateSession {
        name: body.name,
        model: body.model,
        effort: body.effort,
        project_id: body.project_id,
        card_id: body.card_id,
        conversation_id: body.conversation_id,
        last_activity: body.last_activity,
    };

    let session = state.db.update_session(&id, update).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    match session {
        Some(s) => Ok(Json(serde_json::json!(s))),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        )),
    }
}

/// DELETE /api/sessions/:id
async fn delete_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Delete associated events first
    state.db.delete_events_by_session(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    let deleted = state.db.delete_session(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    if !deleted {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        ));
    }

    Ok(StatusCode::NO_CONTENT)
}

/// GET /api/sessions/:id/events — list events with optional afterSeq + limit
async fn list_events(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<EventsQuery>,
) -> impl IntoResponse {
    let events = if let Some(after_seq) = params.after_seq {
        state.db.events_since(&id, after_seq).await
    } else {
        state.db.list_events_by_session(&id, None).await
    };

    let mut events = events.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    // Apply limit if specified
    if let Some(limit) = params.limit {
        events.truncate(limit as usize);
    }

    // Parse data field from string to JSON for each event
    let events_json: Vec<serde_json::Value> = events
        .iter()
        .map(|e| {
            serde_json::json!({
                "id": e.id,
                "session_id": e.session_id,
                "seq": e.seq,
                "ts": e.ts,
                "kind": e.kind,
                "data": serde_json::from_str::<serde_json::Value>(&e.data).unwrap_or_default(),
            })
        })
        .collect();

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!(events_json)))
}

/// POST /api/sessions/:id/events — append an event
async fn append_event(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<AppendEventRequest>,
) -> impl IntoResponse {
    // Verify session exists
    let session = state.db.get_session(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    if session.is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        ));
    }

    let event = state
        .db
        .append_event(&id, &body.kind, body.data)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    // Broadcast the event to WebSocket subscribers
    state.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
        event_type: "event".into(),
        session_id: id,
        data: serde_json::json!({
            "id": event.id,
            "seq": event.seq,
            "ts": event.ts,
            "kind": event.kind,
            "data": serde_json::from_str::<serde_json::Value>(&event.data).unwrap_or_default(),
        }),
    });

    Ok::<_, (StatusCode, Json<serde_json::Value>)>((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "id": event.id,
            "seq": event.seq,
            "ts": event.ts,
            "kind": event.kind,
        })),
    ))
}

/// POST /api/sessions/:id/read — mark session as read
async fn mark_read(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let session = state.db.get_session(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    if session.is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        ));
    }

    state
        .db
        .append_event(&id, "session-read", serde_json::json!({}))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(StatusCode::NO_CONTENT)
}

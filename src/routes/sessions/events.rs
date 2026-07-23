use axum::{
    Extension, Json, extract::Path, extract::Query, extract::State, http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::auth::middleware::AuthUser;
use crate::db::models::UpdateSession;
use crate::state::AppState;

#[derive(Deserialize)]
pub(super) struct EventsQuery {
    /// WS-catch-up mode: return every event with `seq > after_seq`,
    /// oldest-first. Used by the live-update path after a reconnect to
    /// pull anything missed while the socket was down. Returns all
    /// matching rows (no `limit` applied) so the client can be sure it
    /// hasn't lost an event in the gap.
    after_seq: Option<i32>,
    /// Pagination mode: return the latest `limit` events with
    /// `seq < before_seq`, oldest-first. Used by the chat view's
    /// "Load older messages" path. Implies `limit` (capped at
    /// [`MAX_EVENTS_PAGE_SIZE`]).
    before_seq: Option<i32>,
    /// Page size. Applied to `before_seq` and the default "latest N"
    /// mode; ignored for `after_seq` so WS catch-up can't lose events.
    /// Defaults to [`DEFAULT_EVENTS_PAGE_SIZE`], capped at
    /// [`MAX_EVENTS_PAGE_SIZE`].
    limit: Option<i64>,
}

/// Default page size for the chat view's initial fetch. Tuned so the
/// 90% case (recent conversation) loads in one shot without paying the
/// cost of an unbounded event scan, while still showing enough scrollback
/// that the user rarely needs to "Load older".
pub const DEFAULT_EVENTS_PAGE_SIZE: i64 = 200;

/// Hard ceiling on `?limit=` for events. Stops a client from asking for
/// every event in a session (the bug we're fixing) by passing
/// `?limit=99999999`. 1000 is large enough that "Load older" twice
/// covers ~2k events of scrollback before any extra round-trip; past
/// that point the user is digging through history and a few clicks is
/// acceptable.
pub const MAX_EVENTS_PAGE_SIZE: i64 = 1000;

#[derive(Deserialize)]
pub(super) struct AppendEventRequest {
    kind: String,
    data: serde_json::Value,
}

/// GET /api/sessions/:id/events
///
/// Three modes, picked by query param:
/// - **`?after_seq=N`** — WS catch-up. Returns every event with `seq > N`,
///   oldest-first. Returns all matching rows (no limit) so a reconnect
///   never silently drops events.
/// - **`?before_seq=N&limit=K`** — "Load older" page. Returns up to K
///   events with `seq < N`, oldest-first.
/// - **No params (or just `?limit=`)** — Default load. Returns the
///   latest K events (default [`DEFAULT_EVENTS_PAGE_SIZE`]),
///   oldest-first.
///
/// All modes return a flat JSON array. The chat view tracks the lowest
/// `seq` it has and passes it as `before_seq` to page upward.
pub(super) async fn list_events(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<EventsQuery>,
) -> impl IntoResponse {
    tracing::info!(
        session_id = %id,
        after_seq = ?params.after_seq,
        before_seq = ?params.before_seq,
        limit = ?params.limit,
        "Listing events",
    );

    let events = if let Some(after_seq) = params.after_seq {
        // WS catch-up: return all events strictly after `after_seq`.
        // Intentionally NOT limited — losing events in the catch-up
        // path produces silent UI desync.
        state.db.events_since(&id, after_seq).await
    } else {
        let limit = params
            .limit
            .unwrap_or(DEFAULT_EVENTS_PAGE_SIZE)
            .clamp(1, MAX_EVENTS_PAGE_SIZE);
        state
            .db
            .list_events_by_session_before(&id, params.before_seq, limit)
            .await
    };

    let events = events.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

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

/// POST /api/sessions/:id/events -- append an event
pub(super) async fn append_event(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<AppendEventRequest>,
) -> impl IntoResponse {
    tracing::info!(session_id = %id, kind = %body.kind, "Appending event");
    // question-resolved routes through the shared resolver (event append,
    // broadcast, expert feed, conversation resume) so answering from the API
    // and from a plugin surface stays identical. Session existence is checked
    // by the resolver.
    if body.kind == "question-resolved" {
        let event = crate::service::questions::resolve_question(
            state.clone(),
            user.user_id.clone(),
            id.clone(),
            body.data,
        )
        .await
        .map_err(|e| {
            let code = if e == "session not found" {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (code, Json(serde_json::json!({ "error": e })))
        })?;
        return Ok((
            StatusCode::CREATED,
            Json(serde_json::json!({
                "id": event.id,
                "seq": event.seq,
                "ts": event.ts,
                "kind": event.kind,
            })),
        ));
    }
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

    // Update last_activity to now
    let now = chrono::Utc::now().to_rfc3339();
    let _ = state
        .db
        .update_session(
            &id,
            UpdateSession {
                last_activity: Some(now),
                ..Default::default()
            },
        )
        .await;

    // Broadcast the event to WebSocket subscribers
    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "event".into(),
            session_id: id.clone(),
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

/// GET /api/sessions/:id/todos -- current todo snapshot for the session.
///
/// Reads from the dedicated `todos` table, which the `todo` event seam
/// mirrors into on every replace-all snapshot. Returns
/// `{ "todos": [...] }` in `position` order; empty list when the session
/// has never reported any. Live updates still flow over the WebSocket as
/// `todo` events.
pub(super) async fn get_session_todos(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(session_id = %id, "Getting session todos");
    let todos = state.db.list_session_todos(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({ "todos": todos })))
}

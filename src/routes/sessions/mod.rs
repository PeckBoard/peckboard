//! Session HTTP routes. This module holds the simple lifecycle handlers
//! (create/get/list/update/delete/clear/mark_read) plus the
//! `[session:id]` / `[report:folder/file]` reference resolver used by
//! both events and dispatch. Event ingest lives in [`events`] and the
//! send_message + lifecycle controls live in [`dispatch`].

mod dispatch;
mod events;

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
    /// Cursor: `last_activity` of the last row from the previous page.
    /// Paired with `cursor_id` to break ties on rows that share a
    /// `last_activity`. Both must be supplied together to be honoured;
    /// passing just one is treated as "no cursor".
    cursor_la: Option<String>,
    cursor_id: Option<String>,
    /// Page size. Defaults to [`DEFAULT_SESSION_PAGE_SIZE`], capped at
    /// [`MAX_SESSION_PAGE_SIZE`] so a malicious client can't ask for the
    /// entire table by passing `?limit=99999999` and re-introduce the
    /// pre-pagination O(N) behaviour we just removed.
    limit: Option<i64>,
}

/// Default page size for `GET /api/sessions`. Tuned for the sidebar UI:
/// ~100 rows is more than fits on a tall monitor's session list, so the
/// initial fetch covers the visible viewport without a follow-up page
/// fetch for most users.
const DEFAULT_SESSION_PAGE_SIZE: i64 = 100;

/// Hard ceiling on `?limit=`. Stops a buggy or malicious client from
/// asking for the whole table in one go. 500 leaves headroom for power
/// users with denser histories while still bounding the worst-case row
/// scan and JSON serialization cost.
const MAX_SESSION_PAGE_SIZE: i64 = 500;

#[derive(Deserialize)]
struct ListExpertsQuery {
    project_id: Option<String>,
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
        .route("/api/experts", get(list_experts))
        .route(
            "/api/sessions/{id}",
            get(get_session)
                .patch(update_session)
                .delete(delete_session),
        )
        .route(
            "/api/sessions/{id}/events",
            get(events::list_events).post(events::append_event),
        )
        .route("/api/sessions/{id}/todos", get(events::get_session_todos))
        .route("/api/sessions/{id}/read", post(mark_read))
        .route("/api/sessions/{id}/clear", post(clear_session))
        .route("/api/sessions/{id}/message", post(dispatch::send_message))
        .route("/api/sessions/{id}/cancel", post(dispatch::cancel_session))
        .route(
            "/api/sessions/{id}/interrupt",
            post(dispatch::interrupt_session),
        )
        .route(
            "/api/sessions/{id}/terminate",
            post(dispatch::terminate_agent),
        )
        .route(
            "/api/sessions/{id}/status",
            get(dispatch::get_session_status),
        )
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// POST /api/sessions
async fn create_session(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateSessionRequest>,
) -> impl IntoResponse {
    tracing::info!(name = %body.name, folder_id = %body.folder_id, "Creating session");
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
            ..Default::default()
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
///
/// Keyset-paginated. Default page size [`DEFAULT_SESSION_PAGE_SIZE`];
/// the caller can pass `?limit=` up to [`MAX_SESSION_PAGE_SIZE`].
///
/// Response shape:
/// ```json
/// {
///   "items": [Session, ...],
///   "next_cursor": { "last_activity": "...", "id": "..." } | null
/// }
/// ```
///
/// `next_cursor` is `null` when the current page returned fewer rows
/// than the requested limit (i.e. end of list); pass the returned
/// `next_cursor` fields back as `?cursor_la=...&cursor_id=...` to get
/// the next page.
async fn list_sessions(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListSessionsQuery>,
) -> impl IntoResponse {
    tracing::info!(
        folder_id = ?params.folder_id,
        cursor = ?(params.cursor_la.as_deref(), params.cursor_id.as_deref()),
        limit = ?params.limit,
        "Listing sessions",
    );

    let limit = params
        .limit
        .unwrap_or(DEFAULT_SESSION_PAGE_SIZE)
        .clamp(1, MAX_SESSION_PAGE_SIZE);
    // The cursor is meaningless unless BOTH halves are present (they
    // share the keyset comparison). Pass only one and we treat it as
    // "no cursor" rather than half-applying it and producing a
    // surprising page.
    let cursor = match (params.cursor_la, params.cursor_id) {
        (Some(la), Some(id)) => Some((la, id)),
        _ => None,
    };

    let sessions = if let Some(folder_id) = params.folder_id {
        state
            .db
            .list_plain_sessions_by_folder_page(&folder_id, cursor, limit)
            .await
    } else {
        state.db.list_plain_sessions_page(cursor, limit).await
    };

    let sessions = sessions.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    // If we got fewer than `limit` rows, the caller has reached the end
    // — emit `next_cursor: null` so the frontend can stop paginating.
    let next_cursor = if (sessions.len() as i64) < limit {
        serde_json::Value::Null
    } else {
        sessions
            .last()
            .map(|s| {
                serde_json::json!({
                    "last_activity": s.last_activity,
                    "id": s.id,
                })
            })
            .unwrap_or(serde_json::Value::Null)
    };

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({
        "items": sessions,
        "next_cursor": next_cursor,
    })))
}

/// GET /api/experts
///
/// Lists expert sessions for the new experts view. Experts are hidden
/// from the ordinary chat list (`GET /api/sessions`), so this is the
/// only HTTP surface that exposes them. The full `Session` row is
/// returned for each (including `expert_kind`, `knowledge_summary`,
/// `knowledge_area`, `scope_path`, `project_id`, `card_id`,
/// `is_permanent`, `last_activity`); the frontend groups by
/// `project_id` (null = global) and chat session client-side. An
/// optional `?project_id=` narrows to a single project's experts.
async fn list_experts(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListExpertsQuery>,
) -> impl IntoResponse {
    tracing::info!(project_id = ?params.project_id, "Listing experts");
    let experts = if let Some(project_id) = params.project_id {
        state.db.list_expert_sessions_by_project(&project_id).await
    } else {
        state.db.list_expert_sessions().await
    };

    let experts = experts.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!(experts)))
}

/// GET /api/sessions/:id
async fn get_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(session_id = %id, "Getting session");
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
    tracing::info!(session_id = %id, "Updating session");
    let update = UpdateSession {
        name: body.name,
        model: body.model,
        effort: body.effort,
        project_id: body.project_id,
        card_id: body.card_id,
        conversation_id: body.conversation_id,
        last_activity: body.last_activity,
        ..Default::default()
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
    tracing::info!(session_id = %id, "Deleting session");
    // Worker sessions are owned by their card / project — their lifecycle
    // is driven by the orchestrator and they're cleaned up via the
    // card / project cascade. Letting a user delete one directly leaves
    // the parent card pointing at a vanished `worker_session_id` and
    // bypasses the orchestrator's bookkeeping. Refuse, and tell the
    // caller to delete the card or project instead.
    let session = state.db.get_session(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;
    if let Some(ref s) = session
        && s.is_worker
    {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "worker sessions are owned by their card; delete the card or project to remove this session",
            })),
        ));
    }

    // Delete associated events first
    state.db.delete_events_by_session(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    // Remove attachments directory for this session
    let attachments_dir = state.config.data_dir.join("attachments").join(&id);
    if attachments_dir.exists() {
        let _ = std::fs::remove_dir_all(&attachments_dir);
    }

    // Clean up MCP config and tokens
    crate::service::mcp_server::delete_mcp_config(&state.config.data_dir, &id);
    state.mcp_tokens.revoke_by_session(&id).await;

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

    // Tell every connected client the session is gone so other devices
    // (or other tabs on the same device) drop their tab strip entry,
    // wipe any cached events, and switch the body off the now-deleted
    // session if it was the active one. Without this the only cleanup
    // path is the focus-driven `/api/me/tabs` refetch, which closes the
    // tab but leaves ChatView mounted against a 404'd session id.
    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "session-deleted".into(),
            session_id: id.clone(),
            data: serde_json::json!({ "session_id": id }),
        });

    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/sessions/:id/read -- mark session as read
async fn mark_read(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(session_id = %id, "Marking session as read");
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

/// POST /api/sessions/:id/clear -- kill process (placeholder), delete events,
/// delete attachments directory, reset conversation_id. Returns 204.
async fn clear_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(session_id = %id, "Clearing session");
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

    // Kill any running process for this session AND wait for it to fully
    // wind down. The streaming task emits a synthetic `agent-end` (Crashed
    // reason "interrupted") on every cancel path; if we don't wait, that
    // event lands AFTER `delete_events_by_session` below and resurrects
    // a stale "Agent crashed (interrupted)" line on the cleared session.
    //
    // The pre-cancel `interrupt` event is for live UI consumers: ChatView
    // suppresses any subsequent agent-end-with-reason-"interrupted" when
    // it follows an `interrupt`, so the brief window between the Crashed
    // broadcast and the `session-cleared` wipe doesn't flash a crash
    // banner. Both events will themselves be wiped by `delete_events_by_session`
    // and never re-appear after a reload.
    if state.session_manager.is_running(&id).await {
        if let Ok(event) = state
            .db
            .append_event(
                &id,
                "interrupt",
                serde_json::json!({ "reason": "session-clear" }),
            )
            .await
        {
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
        }
        state.session_manager.cancel_and_wait(&id).await;
    }

    // Delete all events for this session
    state.db.delete_events_by_session(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    // Wipe the dedicated todos table too. The chat view's local memo
    // falls back to empty once events load empty, but the load-time
    // /todos endpoint and the dedicated SessionTodosView read straight
    // from this table — without this, todos reappear after a reload or
    // when the user opens the standalone tasks view.
    state
        .db
        .replace_session_todos(&id, crate::todo::TodoSnapshot::default())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    // Tell any live subscribers (chat view, session-todos view) to drop
    // their cached snapshot. Sent as a typed transient frame rather than
    // a persisted `todo` event so the cleared events list stays empty.
    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "session-cleared".into(),
            session_id: id.clone(),
            data: serde_json::json!({ "session_id": id }),
        });

    // Delete attachments directory
    let attachments_dir = state.config.data_dir.join("attachments").join(&id);
    if attachments_dir.exists() {
        let _ = tokio::fs::remove_dir_all(&attachments_dir).await;
    }

    // Reset conversation_id to None
    state
        .db
        .update_session(
            &id,
            UpdateSession {
                conversation_id: Some(None),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(StatusCode::NO_CONTENT)
}

/// Resolve `[session:id]` and `[report:folder/file]` references in text.
pub(super) async fn resolve_references(text: &str, state: &Arc<AppState>) -> String {
    let mut result = text.to_string();

    // Resolve session references
    let session_re = regex::Regex::new(r"\[session:([a-f0-9\-]+)\]").unwrap();
    let mut session_replacements = Vec::new();
    for cap in session_re.captures_iter(&result) {
        let full_match = cap[0].to_string();
        let ref_session_id = cap[1].to_string();

        // Hook: session.reference.resolve
        let hook_result = state
            .plugins
            .dispatch(
                "session.reference.resolve",
                serde_json::json!({
                    "referencedSessionId": &ref_session_id,
                }),
            )
            .await;

        if let crate::plugin::hooks::HookResult::Allowed(modified) = &hook_result {
            if let Some(custom) = modified.get("replacement").and_then(|v| v.as_str()) {
                session_replacements.push((full_match, custom.to_string()));
                continue;
            }
        }

        if let Ok(Some(ref_session)) = state.db.get_session(&ref_session_id).await {
            let session_name = &ref_session.name;
            let conv_id = ref_session.conversation_id.as_deref().unwrap_or("unknown");
            let card_info = if let Some(ref card_id) = ref_session.card_id {
                state
                    .db
                    .get_card(card_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|c| format!(" (card: \"{}\")", c.title))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let replacement = format!(
                "[Referenced session \"{}\"{} — conversation_id: {}. \
                 To read this session's full history, you can resume it with \
                 conversation_id \"{}\". The session contains the work and \
                 context from that conversation.]",
                session_name, card_info, conv_id, conv_id
            );
            session_replacements.push((full_match, replacement));
        } else {
            session_replacements.push((
                full_match,
                format!("[session {} not found]", ref_session_id),
            ));
        }
    }
    for (from, to) in session_replacements {
        result = result.replace(&from, &to);
    }

    // Resolve report references. The folder/file segments come from
    // user-supplied chat text, so they must pass the same strict
    // alphanumeric/dash/underscore filter the REST report routes use.
    // Without this an attacker who can send chat messages can drop
    // `[report:../../etc/passwd]` to slurp arbitrary files into the
    // session transcript.
    fn safe_report_segment(s: &str) -> bool {
        if s.is_empty() || s.len() > 128 {
            return false;
        }
        let core = s.strip_suffix(".md").unwrap_or(s);
        if core.is_empty() {
            return false;
        }
        core.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    }

    let report_re = regex::Regex::new(r"\[report:([^\]]+)\]").unwrap();
    let mut report_replacements = Vec::new();
    for cap in report_re.captures_iter(&result) {
        let full_match = cap[0].to_string();
        let report_path = cap[1].to_string();
        let parts: Vec<&str> = report_path.splitn(2, '/').collect();
        if parts.len() == 2 {
            let folder = parts[0];
            let file = parts[1];
            if !safe_report_segment(folder) || !safe_report_segment(file) {
                report_replacements.push((
                    full_match,
                    format!("[report {}/{} rejected: invalid path]", folder, file),
                ));
                continue;
            }
            let report_file = state
                .config
                .data_dir
                .join("reports")
                .join(folder)
                .join(file);
            if let Ok(content) = tokio::fs::read_to_string(&report_file).await {
                let body = if content.starts_with("---") {
                    content.splitn(3, "---").nth(2).unwrap_or(&content).trim()
                } else {
                    content.trim()
                };
                let truncated = if body.len() > 2000 {
                    format!("{}... (truncated)", &body[..2000])
                } else {
                    body.to_string()
                };
                report_replacements.push((
                    full_match,
                    format!(
                        "[Report: {}/{}]\n{}\n[End of report]",
                        folder, file, truncated
                    ),
                ));
            } else {
                report_replacements.push((
                    full_match,
                    format!("[report {}/{} not found]", folder, file),
                ));
            }
        }
    }
    for (from, to) in report_replacements {
        result = result.replace(&from, &to);
    }

    result
}

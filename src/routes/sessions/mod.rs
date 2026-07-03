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
        .route("/api/sessions/{id}/compact", post(compact_session))
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
        Some(s) => {
            // Ride the latest context-window occupancy along so the chat
            // toolbar can seed its context badge without a second request;
            // live updates then come from the streamed `agent-usage` events.
            let occupancy = state
                .db
                .latest_context_tokens(&id)
                .await
                .unwrap_or(None)
                .unwrap_or(0);
            let mut v = serde_json::json!(s);
            v["context_tokens"] = serde_json::json!(occupancy);
            Ok(Json(v))
        }
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

    // A model change that crosses a provider/account boundary needs a
    // handover: the outgoing model writes a context doc the incoming model
    // reads (see `crate::handover`). Decide that here, before applying the
    // patch, because a handover defers the actual `model` write until the
    // doc-generation turn completes.
    let requested_model = body.model.clone().flatten();
    let handover_target = maybe_handover_target(&state, &id, requested_model.as_deref()).await?;

    // When handing over, don't write the new `model` yet — the outgoing
    // provider must stay selected so its doc-generation turn routes to it.
    // `begin_handover` parks the target in `handover_to_model` and flips
    // `model` on completion.
    let model_update = if handover_target.is_some() {
        None
    } else {
        body.model
    };

    let update = UpdateSession {
        name: body.name,
        model: model_update,
        effort: body.effort,
        project_id: body.project_id,
        card_id: body.card_id,
        conversation_id: body.conversation_id,
        last_activity: body.last_activity,
        ..Default::default()
    };

    // Stripping the model for a handover can leave an all-`None` changeset
    // (the common "switch model" PATCH carries only `model`). Diesel rejects
    // an empty changeset, so skip the write and just read the row back — the
    // handover branch below does the meaningful state change.
    let has_updates = update.name.is_some()
        || update.model.is_some()
        || update.effort.is_some()
        || update.project_id.is_some()
        || update.card_id.is_some()
        || update.conversation_id.is_some()
        || update.last_activity.is_some();

    let session = if has_updates {
        state.db.update_session(&id, update).await
    } else {
        state.db.get_session(&id).await
    }
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    let mut session = match session {
        Some(s) => s,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "session not found" })),
            ));
        }
    };

    if let Some(target) = handover_target {
        let from = session.model.clone().unwrap_or_default();
        if let Err(e) = crate::handover::begin_handover(&state, &id, &from, &target, None).await {
            tracing::error!(session_id = %id, "Failed to begin handover: {e}");
            // Fall back to a plain switch so the user isn't stuck: apply the
            // model directly and clear the parked target.
            let _ = state
                .db
                .update_session(
                    &id,
                    UpdateSession {
                        model: Some(Some(target.clone())),
                        handover_to_model: Some(None),
                        ..Default::default()
                    },
                )
                .await;
        }
        // Re-read so the response reflects the parked target (or the
        // fallback switch).
        if let Ok(Some(s)) = state.db.get_session(&id).await {
            session = s;
        }
    }

    Ok(Json(serde_json::json!(session)))
}

/// Decide whether a requested model change should trigger a handover, and
/// return the target model id if so. `Ok(None)` means a plain switch: no
/// model change requested, the change stays within the same
/// provider+account, the session is a worker (orchestrator-driven), or the
/// session has no conversation yet to hand over.
///
/// Rejects with 409 when the change can't be honoured right now:
/// - a handover is already in flight (a second switch would race the
///   finalize step's deferred `model` write and silently lose one of them);
/// - a cross-boundary switch while a turn is actively streaming (the
///   handover shuts the outgoing provider down after the NEXT result — see
///   `begin_handover` — which mid-turn would be the user's turn, not the
///   doc turn, so the doc would never be generated).
async fn maybe_handover_target(
    state: &Arc<AppState>,
    session_id: &str,
    requested_model: Option<&str>,
) -> Result<Option<String>, (StatusCode, Json<serde_json::Value>)> {
    let Some(new_model) = requested_model else {
        return Ok(None);
    };
    let Some(session) = state.db.get_session(session_id).await.ok().flatten() else {
        return Ok(None); // route 404s on the main update path
    };

    if session.handover_to_model.is_some() {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "model handover in progress; wait for it to finish before switching again",
            })),
        ));
    }
    // Workers are driven by the orchestrator/card, not interactive switches.
    if session.is_worker {
        return Ok(None);
    }
    let current = session.model.as_deref().unwrap_or("default");
    if !crate::handover::needs_handover(current, new_model) {
        return Ok(None);
    }

    let events = state
        .db
        .events_tail(session_id, 100)
        .await
        .unwrap_or_default();

    // Nothing to hand over if the outgoing model never actually spoke.
    let has_history = events
        .iter()
        .any(|e| e.kind == "agent-text" || e.kind == "agent-start");
    if !has_history {
        return Ok(None);
    }

    if turn_in_flight(&events) {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "agent is mid-turn; wait for it to finish before switching provider or account",
            })),
        ));
    }

    Ok(Some(new_model.to_string()))
}

/// Is a turn actively streaming? True when the latest `agent-start` has no
/// `agent-end` after it — the same signal `derive_status` uses for the
/// toolbar indicator.
fn turn_in_flight(events: &[crate::db::models::Event]) -> bool {
    let last_start = events.iter().rposition(|e| e.kind == "agent-start");
    let last_end = events.iter().rposition(|e| e.kind == "agent-end");
    match (last_start, last_end) {
        (Some(s), Some(e)) => s > e,
        (Some(_), None) => true,
        _ => false,
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
    if attachments_dir.exists()
        && let Err(e) = std::fs::remove_dir_all(&attachments_dir)
    {
        tracing::warn!(
            session_id = %id,
            dir = %attachments_dir.display(),
            "Failed to remove attachments dir during session delete: {e}"
        );
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

/// POST /api/sessions/:id/compact — manual context compaction, valid at any
/// occupancy (threshold-crossing compaction is dispatched automatically by
/// the completion listener). Dispatches the same-model handover doc turn and
/// returns 202; the conversation restarts fresh once the doc lands. 409 with
/// a reason when the session is ineligible (worker, handover already in
/// flight, nothing to compact).
async fn compact_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    match crate::handover::begin_compaction(&state, &id).await {
        Ok(()) => Ok(StatusCode::ACCEPTED),
        Err(e) => Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
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

    let session = match session {
        Some(s) => s,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "session not found" })),
            ));
        }
    };

    // Worker sessions are owned by their card. Wiping their events would
    // strand the orchestrator (no transcript to resume against) and let a
    // user destroy the card's audit trail behind the worker's back.
    // Refuse — mirrors the DELETE guard above.
    if session.is_worker {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "worker sessions are owned by their card; their transcript cannot be cleared",
            })),
        ));
    }

    // Sessions kicked off by a repeating task are the task's run history.
    // Clearing one wipes the audit trail of that scheduled run without
    // removing the run itself, which is never what the user wants — the
    // schedule keeps firing and the cleared row becomes a confusing
    // empty stub. The card-owner asked that Clear simply not be offered
    // for these sessions ([[deleting-sessions]]); the route enforces it
    // so the UI menu hiding is defence-in-depth, not the load-bearing
    // guard.
    if session.repeating_task_id.is_some() {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "this session is a repeating task run; delete it instead of clearing",
            })),
        ));
    }

    // Do the actual wipe via the shared core (also used by the
    // session-control plugin's clear_session host function).
    clear_session_core(&state, &id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(StatusCode::NO_CONTENT)
}

/// Wipe a session's transcript and reset it — the shared body behind both the
/// `/clear` route (after its worker/repeating-task guards) and the
/// session-control plugin's `clear_session`. No guards, no HTTP framing:
/// cancels any in-flight run (waiting for it to wind down so a stale
/// `agent-end` can't resurrect a cleared line), deletes events + todos,
/// drops the attachments dir, and resets `conversation_id`.
pub(crate) async fn clear_session_core(state: &AppState, id: &str) -> anyhow::Result<()> {
    if state.session_manager.is_running(id).await {
        if let Ok(event) = state
            .db
            .append_event(
                id,
                "interrupt",
                serde_json::json!({ "reason": "session-clear" }),
            )
            .await
        {
            state.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
                event_type: "event".into(),
                session_id: id.to_string(),
                data: serde_json::json!({
                    "id": event.id,
                    "seq": event.seq,
                    "ts": event.ts,
                    "kind": event.kind,
                    "data": serde_json::from_str::<serde_json::Value>(&event.data).unwrap_or_default(),
                }),
            });
        }
        state.session_manager.cancel_and_wait(id).await;
    }

    state.db.delete_events_by_session(id).await?;
    state
        .db
        .replace_session_todos(id, crate::todo::TodoSnapshot::default())
        .await?;

    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "session-cleared".into(),
            session_id: id.to_string(),
            data: serde_json::json!({ "session_id": id }),
        });

    let attachments_dir = state.config.data_dir.join("attachments").join(id);
    if attachments_dir.exists()
        && let Err(e) = tokio::fs::remove_dir_all(&attachments_dir).await
    {
        tracing::warn!(
            session_id = %id,
            dir = %attachments_dir.display(),
            "Failed to remove attachments dir during session clear: {e}"
        );
    }

    state
        .db
        .update_session(
            id,
            UpdateSession {
                conversation_id: Some(None),
                ..Default::default()
            },
        )
        .await?;
    Ok(())
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

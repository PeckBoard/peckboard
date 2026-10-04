//! Pending-action routes (`service::voice_actions`): the human side of a
//! gated voice tool call.
//!
//! - `GET  /api/voice/actions` — the caller's open actions.
//! - `POST /api/voice/actions/{id}/confirm` — run the STORED call, once.
//! - `POST /api/voice/actions/{id}/cancel` — the call can never run.
//!
//! Admin-only like the rest of the voice surface, and owner-only on top: an
//! action resolves only for the user it was parked for. Channel-agnostic —
//! the web panel's buttons and its spoken-yes recognizer call these today;
//! IM / phone adapters resolve the same rows through the same service calls.

use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    routing::{get, post},
};
use serde_json::{Value, json};

use crate::auth::middleware::{AuthUser, require_auth};
use crate::db::models::PendingAction;
use crate::service::voice_actions::{self, ActionError};
use crate::state::AppState;

type ApiError = (StatusCode, Json<Value>);

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/voice/actions", get(list_actions))
        .route("/api/voice/actions/{id}/confirm", post(confirm_action))
        .route("/api/voice/actions/{id}/cancel", post(cancel_action))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

fn err(status: StatusCode, msg: impl Into<String>) -> ApiError {
    (status, Json(json!({ "error": msg.into() })))
}

fn admin(user: &AuthUser) -> Result<(), ApiError> {
    if user.is_admin() {
        Ok(())
    } else {
        Err(err(StatusCode::FORBIDDEN, "the Assistant is admin-only"))
    }
}

fn action_err(e: ActionError) -> ApiError {
    let status = match &e {
        ActionError::NotFound => StatusCode::NOT_FOUND,
        ActionError::Resolved(_) | ActionError::Expired => StatusCode::CONFLICT,
        ActionError::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    err(status, e.to_string())
}

fn view(a: &PendingAction) -> Value {
    json!({
        "id": a.id,
        "session_id": a.session_id,
        "channel": a.channel,
        "tool": a.tool,
        "summary": a.summary,
        "status": a.status,
        "created_at": a.created_at,
        "expires_at": a.expires_at,
    })
}

/// GET /api/voice/actions → `{"actions": [...]}`, oldest first.
async fn list_actions(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
) -> Result<Json<Value>, ApiError> {
    admin(&user)?;
    let rows = voice_actions::list_open(&state.db, &user.user_id)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(
        json!({ "actions": rows.iter().map(view).collect::<Vec<_>>() }),
    ))
}

/// POST /api/voice/actions/{id}/confirm — consume the action and run its
/// stored call on the session that parked it. The outcome is recorded on the
/// row and told to that session as a `[relay] action …` note.
async fn confirm_action(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    admin(&user)?;
    let action = voice_actions::confirm(&state.db, &id, &user.user_id)
        .await
        .map_err(action_err)?;
    let row = action.row().clone();
    let session = state
        .db
        .get_session(&row.session_id)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let result = match session {
        Some(session) => {
            let ctx = crate::service::mcp_server::ToolCallContext {
                session_id: session.id.clone(),
                project_id: None,
                card_id: session.card_id.clone(),
                folder_id: session.folder_id.clone(),
                db: Arc::new(state.db.clone()),
                broadcaster: state.broadcaster.clone(),
                provider_registry: Some(state.provider_registry.clone()),
                data_dir: Some(state.config.data_dir.clone()),
                device_registry: Some(state.device_registry.clone()),
                background: Some(state.background.clone()),
            };
            let registry = crate::service::mcp_server::McpToolRegistry::new();
            crate::service::mcp_server::run_confirmed_action(
                &state.plugins,
                &registry,
                action,
                &ctx,
            )
            .await
        }
        None => Err(anyhow::anyhow!("the session that asked for this is gone")),
    };
    // Internal `_marker` keys are route plumbing for other tools; a gated
    // tool never needs one, so they are dropped rather than acted on.
    let result = result.map(|mut v| {
        if let Some(o) = v.as_object_mut() {
            o.retain(|k, _| !k.starts_with('_'));
        }
        v
    });
    let outcome = match &result {
        Ok(v) => Ok(Some(v.clone())),
        Err(e) => Err(e.to_string()),
    };
    tracing::info!(
        action_id = %row.id, user_id = %user.user_id, tool = %row.tool,
        ok = result.is_ok(), "pending action executed"
    );
    finish(&state, &row, "confirmed", &outcome).await;
    Ok(Json(match result {
        Ok(v) => json!({ "status": "confirmed", "ok": true, "result": v }),
        Err(e) => json!({ "status": "confirmed", "ok": false, "error": e.to_string() }),
    }))
}

/// POST /api/voice/actions/{id}/cancel — the action can never run.
async fn cancel_action(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    admin(&user)?;
    let row = voice_actions::cancel(&state.db, &id, &user.user_id)
        .await
        .map_err(action_err)?;
    finish(&state, &row, "cancelled", &Ok(None)).await;
    Ok(Json(json!({ "status": "cancelled" })))
}

/// Refresh panels and tell the asking session what the user decided.
async fn finish(
    state: &Arc<AppState>,
    row: &PendingAction,
    status: &str,
    outcome: &Result<Option<Value>, String>,
) {
    let mut resolved = row.clone();
    resolved.status = status.to_string();
    voice_actions::broadcast(&state.broadcaster, &resolved);
    let text = voice_actions::outcome_note(&row.summary, outcome);
    let dispatcher = crate::service::mcp_server::AppExpertDispatcher::new(state.clone());
    if let Err(e) = crate::service::session_notify::notify_session(
        &state.db,
        &state.broadcaster,
        Some(&dispatcher),
        &row.session_id,
        &text,
        json!({ "source": "voice-action" }),
    )
    .await
    {
        tracing::warn!(session_id = %row.session_id, "pending action note failed: {e}");
    }
}

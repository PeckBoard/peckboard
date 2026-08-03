//! Shared delete guard for provider-account routes (claude/grok/kimi).
//!
//! Deleting an account whose id is still pinned into a `model@account_id`
//! reference (session/card/project/repeating-task/queued-message) or the
//! app-wide default model would otherwise strand that reference from the
//! next dispatch onward ("account not found",
//! `provider/claude/provider.rs`). Without `?force=true` the delete is
//! refused (409) with the reference counts; with it, every reference is
//! rewritten to the bare model id first.
//!
//! The isolated CLI config dir is only removed once nothing referencing the
//! account is still a live/running session — a child process reading that
//! dir mid-turn must not have it rug-pulled out from under it. The account
//! row itself is always deleted; an orphaned config dir left behind for a
//! live child is harmless (best-effort cleanup only, same as before).

use std::sync::Arc;

use axum::Json;
use axum::http::StatusCode;

use crate::state::AppState;

pub type ApiError = (StatusCode, Json<serde_json::Value>);

fn server_error(e: anyhow::Error) -> ApiError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": e.to_string() })),
    )
}

/// Query param shared by the three `DELETE .../:id` routes.
#[derive(Debug, serde::Deserialize, Default)]
pub struct DeleteAccountQuery {
    #[serde(default)]
    pub force: bool,
}

/// Runs the shared reference check/rewrite, then deletes the row via
/// `delete_row` (same shape as the existing `Db::delete_*_account`
/// methods: `Ok(Some(config_dir))` on success, `Ok(None)` if the id wasn't
/// found, `Err` on failure). Returns the config dir to remove — `None`
/// either when the account had none, or when a live session still
/// references it and removal must be deferred.
pub async fn guarded_delete<F, Fut>(
    state: &Arc<AppState>,
    account_id: &str,
    force: bool,
    delete_row: F,
) -> Result<Option<String>, ApiError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<Option<Option<String>>>>,
{
    let refs = state
        .db
        .account_model_refs(account_id)
        .await
        .map_err(server_error)?;

    let default_model = crate::routes::settings::default_model_setting(state).await;
    let default_pinned = default_model
        .as_deref()
        .map(|m| {
            let (_, acct) = crate::provider::registry::split_model_account(m);
            acct == Some(account_id)
        })
        .unwrap_or(false);

    if !force && (!refs.is_empty() || default_pinned) {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "account is still referenced; retry with ?force=true to delete anyway \
                          (references are rewritten to the bare model)",
                "sessions": refs.sessions,
                "cards": refs.cards,
                "projects": refs.projects,
                "repeating_tasks": refs.repeating_tasks,
                "queued_messages": refs.queued_messages,
                "default_model": default_pinned,
            })),
        ));
    }

    // Live check before the row disappears: once deleted, `is_running`
    // still works (it's a session-manager lookup, not account-gated), but
    // doing it up front keeps the check unambiguous either way.
    let mut live = false;
    for session_id in &refs.sessions {
        if state.session_manager.is_running(session_id).await {
            live = true;
            break;
        }
    }

    if force {
        state
            .db
            .strip_account_model_refs(account_id)
            .await
            .map_err(server_error)?;
        if default_pinned {
            crate::routes::settings::set_default_model_value(state, "").await?;
        }
    }

    let config_dir = delete_row().await.map_err(server_error)?.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "account not found" })),
        )
    })?;

    if live {
        return Ok(None);
    }
    Ok(config_dir)
}

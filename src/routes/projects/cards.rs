use axum::{Json, extract::Path, extract::State, http::StatusCode, response::IntoResponse};
use serde::Deserialize;
use std::sync::Arc;

use crate::db::models::{Card, NewCard, UpdateCard};
use crate::routes::misc::explicit_null;
use crate::state::AppState;

use super::{apply_dependencies, card_json_with_deps};

/// Load a card and enforce that it really belongs to the project named in
/// the URL. Every `/api/projects/:id/cards/:card_id/*` route addresses a
/// card *through* a project, so a card living in a different project has to
/// be indistinguishable from one that doesn't exist -- otherwise a handler
/// runs against the URL project's folder/state while carrying a foreign
/// card's id. That is how retry-merge came to clear the merge state of a
/// card in a project it was never asked about.
async fn load_scoped_card(
    state: &AppState,
    project_id: &str,
    card_id: &str,
) -> Result<Card, (StatusCode, Json<serde_json::Value>)> {
    let not_found = || {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "card not found" })),
        )
    };
    let card = state
        .db
        .get_card(card_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?
        .ok_or_else(not_found)?;
    if card.project_id != project_id {
        return Err(not_found());
    }
    Ok(card)
}

#[derive(Deserialize)]
pub(super) struct CreateCardRequest {
    title: String,
    description: String,
    step: String,
    priority: i32,
    workflow: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    /// Ids of cards this card depends on (must be `done` before a worker
    /// will pick this card up).
    depends_on: Option<Vec<String>>,
    /// File the card as already blocked so no worker picks it up until
    /// a human (or another caller) unblocks it. A non-empty
    /// `block_reason` implies `blocked = true` when this is omitted.
    #[serde(default)]
    blocked: Option<bool>,
    #[serde(default)]
    block_reason: Option<String>,
    /// Cost-aware model auto-switch opt-in for workers on this card.
    /// Omitted / `null` = inherit the default (ON — cards spawn workers).
    #[serde(default)]
    model_autoswitch: Option<bool>,
    /// Name of a library system prompt to attach to this card. Non-empty is
    /// validated to exist; empty string / omitted = none. Applied to the
    /// worker session's system prompt at spawn.
    #[serde(default)]
    system_prompt_name: Option<String>,
}

#[derive(Deserialize, serde::Serialize)]
pub(super) struct UpdateCardRequest {
    title: Option<String>,
    description: Option<String>,
    step: Option<String>,
    priority: Option<i32>,
    workflow: Option<String>,
    /// Explicit `null` clears the pin (see `explicit_null`), so the card
    /// falls back to the inherit chain (step → project → app default); an
    /// absent key leaves the pin alone. Without this, a caller could never
    /// un-pin a model.
    #[serde(default, deserialize_with = "explicit_null")]
    model: Option<Option<String>>,
    /// Explicit `null` clears the effort override (see `explicit_null`);
    /// absent leaves it alone. Without this the effort picker's "Default"
    /// choice was silently dropped on update.
    #[serde(default, deserialize_with = "explicit_null")]
    effort: Option<Option<String>>,
    worker_session_id: Option<Option<String>>,
    last_worker_session_id: Option<Option<String>>,
    handoff_context: Option<Option<String>>,
    blocked: Option<bool>,
    /// Explicit `null` clears the reason (see `explicit_null`) — that is how
    /// the kanban's Unblock action wipes the stale text.
    #[serde(default, deserialize_with = "explicit_null")]
    block_reason: Option<Option<String>>,
    /// Name of a library system prompt to attach. Non-empty is validated to
    /// exist; empty string clears it.
    system_prompt_name: Option<String>,
    model_autoswitch: Option<Option<bool>>,
    /// When present, replaces the card's full dependency set.
    depends_on: Option<Vec<String>>,
}

/// POST /api/projects/:id/cards
pub(super) async fn create_card(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<String>,
    Json(body): Json<CreateCardRequest>,
) -> impl IntoResponse {
    tracing::info!(project_id = %project_id, title = %body.title, "Creating card");

    // Validate priority
    if !crate::routes::misc::is_valid_priority(body.priority) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(
                serde_json::json!({ "error": format!("invalid priority: {}. Use GET /api/priorities for valid values.", body.priority) }),
            ),
        ));
    }

    // Hook: card.create.before — plugins can validate or modify
    let hook_result = state
        .plugins
        .dispatch(
            "card.create.before",
            serde_json::json!({
                "projectId": project_id,
                "title": body.title,
                "priority": body.priority,
            }),
        )
        .await;
    if let crate::plugin::hooks::HookResult::Cancelled { plugin, reason, .. } = &hook_result {
        tracing::info!(plugin = %plugin, reason = %reason, "card.create.before cancelled");
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": format!("blocked by plugin {plugin}: {reason}") })),
        ));
    }

    // Verify project exists. We also need its workflow as the bake-in
    // default when the request doesn't name one explicitly: cards now
    // store a concrete workflow id at create time rather than deferring
    // resolution to read time, so a project workflow change later won't
    // silently re-route an existing card's step order.
    let project = state.db.get_project(&project_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    let project = match project {
        Some(p) => p,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "project not found" })),
            ));
        }
    };

    // Resolve the card's workflow once, at create time. If the client
    // sent a non-empty id we validate it against the registry; otherwise
    // we copy the project's workflow into the card.
    let workflow = match body.workflow.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => {
            if crate::workflow::workflow_by_id(id).is_none() {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": format!("unknown workflow id '{id}'") })),
                ));
            }
            id.to_string()
        }
        _ => project.workflow.clone(),
    };

    let now = chrono::Utc::now().to_rfc3339();
    let id = uuid::Uuid::new_v4().to_string();

    // Treat a non-empty `block_reason` as implicitly setting blocked, so
    // a caller can file a blocked card with one field. Empty/whitespace
    // strings are dropped to keep stored reasons meaningful.
    let block_reason = body
        .block_reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let blocked = body.blocked.unwrap_or(block_reason.is_some());

    // Validate and canonicalize a selected library prompt name. Cards store
    // only the name (resolved to a body at worker spawn); empty/omitted =
    // none, an unknown name is a 400.
    let system_prompt_name = state
        .db
        .resolve_system_prompt(body.system_prompt_name.as_deref())
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?
        .map(|(name, _body)| name);

    let card = state
        .db
        .create_card(NewCard {
            id,
            project_id: project_id.clone(),
            title: body.title,
            description: body.description,
            step: body.step,
            priority: body.priority,
            workflow,
            model: body.model,
            effort: body.effort,
            blocked,
            block_reason,
            created_at: now.clone(),
            updated_at: now,
            system_prompt_name,
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    // An explicit auto-switch choice is applied as a follow-up update so the
    // create path keeps its single `NewCard` shape; omitted leaves the
    // column NULL (inherit the worker default).
    let card = match body.model_autoswitch {
        Some(v) => state
            .db
            .update_card(
                &card.id,
                UpdateCard {
                    model_autoswitch: Some(Some(v)),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": e.to_string() })),
                )
            })?
            .unwrap_or(card),
        None => card,
    };

    // Persist dependencies if requested. On validation failure roll the
    // card back so we don't leave a half-created card behind.
    if let Some(depends_on) = body.depends_on {
        if let Err(err) = apply_dependencies(&state, &project_id, &card.id, depends_on).await {
            let _ = state.db.delete_card(&card.id).await;
            return Err(err);
        }
    }

    let card_value = card_json_with_deps(&state, &card).await;

    // Broadcast card creation for live kanban
    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "card-update".into(),
            session_id: card.project_id.clone(),
            data: serde_json::json!({ "card": card_value }),
        });

    Ok::<_, (StatusCode, Json<serde_json::Value>)>((StatusCode::CREATED, Json(card_value)))
}

/// GET /api/projects/:id/cards
pub(super) async fn list_cards(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(project_id = %project_id, "Listing cards");
    let cards = state
        .db
        .list_cards_by_project(&project_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    // Attach each card's dependency ids from a single project-wide query.
    let edges = state
        .db
        .list_dependencies_by_project(&project_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;
    let mut deps_by_card: std::collections::HashMap<&str, Vec<&str>> =
        std::collections::HashMap::new();
    for (card_id, dep_id) in &edges {
        deps_by_card
            .entry(card_id.as_str())
            .or_default()
            .push(dep_id.as_str());
    }

    let mut items: Vec<serde_json::Value> = Vec::with_capacity(cards.len());
    for c in &cards {
        let deps = deps_by_card.get(c.id.as_str()).cloned().unwrap_or_default();
        let mut value = serde_json::to_value(c).unwrap_or_else(|_| serde_json::json!({}));
        if let Some(obj) = value.as_object_mut() {
            obj.insert("depends_on".into(), serde_json::json!(deps));
            // Seed the board's per-card context badge: latest context-window
            // occupancy of the card's (current or resumable) worker session.
            // Live updates then ride the streamed `agent-usage` events, same
            // as the chat toolbar. Terminal cards skip the lookup — no badge.
            if c.step != "done"
                && c.step != "wont_do"
                && let Some(sid) = c
                    .worker_session_id
                    .as_deref()
                    .or(c.last_worker_session_id.as_deref())
            {
                let ctx = state
                    .db
                    .latest_context_tokens(sid)
                    .await
                    .unwrap_or(None)
                    .unwrap_or(0);
                if ctx > 0 {
                    obj.insert("context_tokens".into(), serde_json::json!(ctx));
                }
                // Seed the board's per-card error chip: the session's most
                // recent failed agent turn (crash, or a Completed turn
                // carrying an `error`, e.g. an expired login). Live updates
                // then ride the streamed `agent-end` / `agent-start` events.
                if let Ok(Some((err, kind))) = state.db.latest_worker_error(sid).await {
                    obj.insert("last_worker_error".into(), serde_json::json!(err));
                    if let Some(kind) = kind {
                        obj.insert("last_worker_error_kind".into(), serde_json::json!(kind));
                    }
                }
            }
        }
        items.push(value);
    }

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!(items)))
}

/// PUT /api/projects/:id/cards/:card_id
pub(super) async fn update_card(
    State(state): State<Arc<AppState>>,
    Path((project_id, card_id)): Path<(String, String)>,
    Json(mut body): Json<UpdateCardRequest>,
) -> impl IntoResponse {
    tracing::info!(card_id = %card_id, "Updating card");
    load_scoped_card(&state, &project_id, &card_id).await?;

    // Validate priority if being updated
    if let Some(priority) = body.priority {
        if !crate::routes::misc::is_valid_priority(priority) {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(
                    serde_json::json!({ "error": format!("invalid priority: {priority}. Use GET /api/priorities for valid values.") }),
                ),
            ));
        }
    }

    // Validate workflow id up front if the client is changing it. A
    // card's workflow is NOT NULL at the schema level, so an explicit
    // clear or an unknown id is rejected.
    if let Some(ref wf) = body.workflow {
        let trimmed = wf.trim();
        if trimmed.is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "workflow is required" })),
            ));
        }
        if crate::workflow::workflow_by_id(trimmed).is_none() {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("unknown workflow id '{trimmed}'") })),
            ));
        }
        body.workflow = Some(trimmed.to_string());
    }

    // Hook: card.update.before
    let hook_result = state
        .plugins
        .dispatch(
            "card.update.before",
            serde_json::json!({
                "cardId": card_id,
                "updates": serde_json::to_value(&body).unwrap_or_default(),
            }),
        )
        .await;
    if let crate::plugin::hooks::HookResult::Cancelled { plugin, reason, .. } = &hook_result {
        tracing::info!(plugin = %plugin, reason = %reason, "card.update.before cancelled");
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": format!("blocked by plugin {plugin}: {reason}") })),
        ));
    }

    // Pull `depends_on` out before the atomic update closure so the
    // body fields it captures don't include the dep set. Replacing
    // dependencies is a separate table write that needs to validate
    // (unknown dep / cycle) against the project's full card graph; we
    // do it after the atomic card update succeeds so a failed card
    // write doesn't leave dependencies in an inconsistent state.
    let depends_on = body.depends_on.take();

    // Atomic validate + update under the DB connection mutex. Holding
    // the mutex across the read-validate-write closure prevents two
    // concurrent transitions from both seeing the same pre-state and
    // both applying their write (e.g. two `complete_step` calls racing
    // and producing inconsistent step values).
    //
    // `stale_worker` carries the worker_session_id that was assigned to
    // the card BEFORE this update applied a step change — captured
    // inside the closure (so it's atomic against parallel writers) and
    // cancelled after the response is shipped. Without this, a user
    // dragging an in-flight card to a different column would leave the
    // worker running on the old step, and the worker could then call
    // `complete_step` against a now-incorrect base step.
    let depends_on_present = depends_on.is_some();
    // Resolve a selected library prompt name before the atomic closure (the
    // closure is sync, and resolution is an async DB lookup). Present in the
    // request => touch the column: Some(name) validates+canonicalizes and
    // sets it, Some("") clears it; absent leaves it untouched. Unknown => 400.
    let system_prompt_name_update: Option<Option<String>> = match &body.system_prompt_name {
        None => None,
        Some(name) => Some(
            state
                .db
                .resolve_system_prompt(Some(name.as_str()))
                .await
                .map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({ "error": e.to_string() })),
                    )
                })?
                .map(|(n, _body)| n),
        ),
    };

    let stale_worker_cell = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let stale_worker_writer = stale_worker_cell.clone();
    let prev_step_cell = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let prev_step_writer = prev_step_cell.clone();
    // Set when this update flips `blocked` true -> false, so a fresh
    // attempt budget can be granted after the atomic update succeeds (see
    // `mark_card_unblocked`) -- same treatment as the restart-worker route.
    let unblocked_cell = std::sync::Arc::new(std::sync::Mutex::new(false));
    let unblocked_writer = unblocked_cell.clone();
    let card = state
        .db
        .update_card_atomic(&card_id, move |existing| {
            // Terminal-state + backlog-freeze policy, shared with the MCP
            // `update_card` handler so both writers enforce one policy.
            crate::card_policy::enforce_card_update_policy(
                &existing.step,
                &crate::card_policy::CardUpdateIntent {
                    step: body.step.is_some(),
                    title: body.title.is_some(),
                    description: body.description.is_some(),
                    priority: body.priority.is_some(),
                    workflow: body.workflow.is_some(),
                    model: body.model.is_some(),
                    effort: body.effort.is_some(),
                    blocked: body.blocked.is_some(),
                    block_reason: body.block_reason.is_some(),
                    depends_on: depends_on_present,
                },
            )?;

            // If this update changes the step and the caller did NOT
            // explicitly touch worker_session_id, force-clear the
            // assignment (and stamp last_worker_session_id) so the
            // worker we're about to cancel can't keep advancing a stale
            // base step. The stash drives the post-update cancel.
            let mut worker_session_id = body.worker_session_id.clone();
            let mut last_worker_session_id = body.last_worker_session_id.clone();
            let step_changing = body.step.as_deref().is_some_and(|s| s != existing.step);
            if step_changing {
                *prev_step_writer.lock().unwrap() = Some(existing.step.clone());
            }
            if step_changing
                && worker_session_id.is_none()
                && let Some(sid) = existing.worker_session_id.clone()
            {
                *stale_worker_writer.lock().unwrap() = Some(sid.clone());
                worker_session_id = Some(None);
                last_worker_session_id = Some(Some(sid));
            }
            if existing.blocked && body.blocked == Some(false) {
                *unblocked_writer.lock().unwrap() = true;
            }

            Ok(UpdateCard {
                title: body.title,
                description: body.description,
                step: body.step,
                priority: body.priority,
                workflow: body.workflow,
                model: body.model,
                effort: body.effort,
                worker_session_id,
                last_worker_session_id,
                handoff_context: body.handoff_context,
                blocked: body.blocked,
                block_reason: body.block_reason,
                model_autoswitch: body.model_autoswitch,
                system_prompt_name: system_prompt_name_update,
                updated_at: Some(chrono::Utc::now().to_rfc3339()),
                // Leave to update_card_atomic's stamper — it knows the
                worktree_unmerged_reason: None,
                worktree_unmerged_detail: None,
                // prev_step from the read it already did.
                completed_at: None,
            })
        })
        .await;

    let card = match card {
        Ok(c) => c,
        Err(e) => {
            let msg = e.to_string();
            // Validation rejections from the closure are user-correctable
            // (terminal-state or backlog-locked policy); everything else
            // is a server-side error.
            let status = if msg.starts_with("card-update-policy:") {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            return Err((
                status,
                Json(serde_json::json!({
                    "error": msg.trim_start_matches("card-update-policy: ").to_string()
                })),
            ));
        }
    };

    let Some(c) = card else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "card not found" })),
        ));
    };

    if *unblocked_cell.lock().unwrap() {
        if let Err(e) = crate::worker::orchestrator::mark_card_unblocked(&state.db, &card_id).await
        {
            tracing::warn!(card_id = %card_id, "Failed to reset crash/no-progress counters on unblock: {e}");
        }
    }

    // Apply dependency replacements after the card row update has
    // succeeded. `apply_dependencies` validates unknown ids / cycles
    // and returns a 4xx via the response type if rejected.
    if let Some(deps) = depends_on {
        apply_dependencies(&state, &c.project_id, &card_id, deps).await?;
    }

    let card_value = card_json_with_deps(&state, &c).await;
    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "card-update".into(),
            session_id: c.project_id.clone(),
            data: serde_json::json!({ "card": card_value }),
        });

    let prev_step = prev_step_cell.lock().unwrap().take();
    if let Some(old_step) = prev_step {
        crate::plugin::notify::fire_card_step_after(
            &state.db,
            &card_id,
            &c.title,
            &c.project_id,
            &old_step,
            &c.step,
        )
        .await;
    }
    // If the user dragged the card to a terminal step, clear the prior
    // worker's todos so the chat session view, the standalone session
    // todos view, and the project todos panel all stop showing what is
    // now stale scratchpad. Falls back to `last_worker_session_id` when
    // the card was already idle on a non-terminal step before this
    // update (the just-cleared `stale_worker_cell` is empty in that case).
    let stale_sid = stale_worker_cell.lock().unwrap().take();
    if c.step == "done" || c.step == "wont_do" {
        let cleanup_sid = stale_sid
            .clone()
            .or_else(|| c.last_worker_session_id.clone());
        if let Some(sid) = cleanup_sid {
            crate::worker::orchestrator::clear_session_todos(&state.db, &state.broadcaster, &sid)
                .await;
        }
    }

    // Cancel the worker that was running against the pre-update step.
    // Done after the broadcast so the UI sees the new state immediately
    // and the (slower) `cancel_and_wait` doesn't gate the HTTP response.
    if let Some(sid) = stale_sid {
        tracing::info!(
            card_id = %card_id,
            session_id = %sid,
            "Cancelling worker after user moved card to a different step"
        );
        let state_for_cancel = state.clone();
        tokio::spawn(async move {
            crate::worker::orchestrator::cancel_worker_for_card_move(&state_for_cancel, &sid).await;
        });
    }
    Ok(Json(card_value))
}

/// DELETE /api/projects/:id/cards/:card_id
pub(super) async fn delete_card(
    State(state): State<Arc<AppState>>,
    Path((project_id, card_id)): Path<(String, String)>,
) -> impl IntoResponse {
    tracing::info!(card_id = %card_id, "Deleting card");
    load_scoped_card(&state, &project_id, &card_id).await?;

    // Atomic cascade. Replaces a sequence of separate awaits with
    // `let _ = …` that silently swallowed errors and could leave
    // orphaned events/sessions when a step failed.
    let report = state.db.delete_card_cascade(&card_id).await.map_err(|e| {
        let msg = e.to_string();
        let status = if msg.contains("not found") {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        (status, Json(serde_json::json!({ "error": msg })))
    })?;
    tracing::info!(
        card_id = %card_id,
        sessions = report.sessions_deleted,
        events = report.events_deleted,
        "Card cascade-deleted"
    );

    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "card-delete".into(),
            session_id: project_id.clone(),
            data: serde_json::json!({ "cardId": card_id, "projectId": project_id }),
        });

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(StatusCode::NO_CONTENT)
}

/// POST /api/projects/:id/cards/:card_id/stop -- stop the card's active worker
pub(super) async fn stop_card_worker(
    State(state): State<Arc<AppState>>,
    Path((project_id, card_id)): Path<(String, String)>,
) -> impl IntoResponse {
    tracing::info!(card_id = %card_id, "Stopping card worker");
    let card = load_scoped_card(&state, &project_id, &card_id).await?;

    if let Some(session_id) = &card.worker_session_id {
        state.session_manager.cancel(session_id).await;
        state
            .db
            .update_card(
                &card_id,
                crate::db::models::UpdateCard {
                    worker_session_id: Some(None),
                    last_worker_session_id: Some(Some(session_id.clone())),
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
    }

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({ "ok": true })))
}

/// POST /api/projects/:id/cards/:card_id/restart -- restart the card's worker
pub(super) async fn restart_card_worker(
    State(state): State<Arc<AppState>>,
    Path((project_id, card_id)): Path<(String, String)>,
) -> impl IntoResponse {
    tracing::info!(card_id = %card_id, "Restarting card worker");
    let card = load_scoped_card(&state, &project_id, &card_id).await?;

    // Stop existing worker if running
    if let Some(session_id) = &card.worker_session_id {
        state.session_manager.cancel(session_id).await;
        state
            .db
            .update_card(
                &card_id,
                crate::db::models::UpdateCard {
                    worker_session_id: Some(None),
                    last_worker_session_id: Some(Some(session_id.clone())),
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
    }

    // Unblock if blocked. Also resets the crash / no-progress counters
    // (`mark_card_unblocked`) so this manual retry gets a fresh attempt
    // budget instead of immediately re-tripping on the next crash or
    // no-progress completion.
    if card.blocked {
        state
            .db
            .update_card(
                &card_id,
                crate::db::models::UpdateCard {
                    blocked: Some(false),
                    block_reason: Some(None),
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
        if let Err(e) = crate::worker::orchestrator::mark_card_unblocked(&state.db, &card_id).await
        {
            tracing::warn!(card_id = %card_id, "Failed to reset crash/no-progress counters on unblock: {e}");
        }
    }

    // The watchdog/orchestrator will pick up the unassigned card on next cycle
    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({ "ok": true })))
}

/// POST /api/projects/:id/cards/:card_id/cancel-wont-do -- cancel worker and mark card as wont_do
pub(super) async fn cancel_card_wont_do(
    State(state): State<Arc<AppState>>,
    Path((project_id, card_id)): Path<(String, String)>,
) -> impl IntoResponse {
    tracing::info!(card_id = %card_id, "Cancelling card as wont_do");
    let card = load_scoped_card(&state, &project_id, &card_id).await?;

    // Stop existing worker
    if let Some(session_id) = &card.worker_session_id {
        state.session_manager.cancel(session_id).await;
    }

    // Clear the prior worker's todos so the in-progress scratchpad
    // disappears with the card. Use the current `worker_session_id`
    // when present, or `last_worker_session_id` to catch the case
    // where the user cancels an already-idle card whose previous run
    // left a snapshot.
    let cleanup_sid = card
        .worker_session_id
        .clone()
        .or_else(|| card.last_worker_session_id.clone());
    if let Some(sid) = cleanup_sid {
        crate::worker::orchestrator::clear_session_todos(&state.db, &state.broadcaster, &sid).await;
    }

    // Move card to wont_do
    state
        .db
        .update_card(
            &card_id,
            crate::db::models::UpdateCard {
                step: Some("wont_do".into()),
                worker_session_id: Some(None),
                last_worker_session_id: card.worker_session_id.map(Some),
                blocked: Some(false),
                block_reason: Some(None),
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

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({ "ok": true })))
}

/// POST /api/projects/:id/cards/:card_id/retry-merge -- re-run the merge +
/// cleanup of the card's git worktree after the user resolved the conflict.
/// Returns the git error text when it still can't be merged.
pub(super) async fn retry_card_merge(
    State(state): State<Arc<AppState>>,
    Path((project_id, card_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let err500 = |e: anyhow::Error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    };
    let not_found = |what: &str| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("{what} not found") })),
        )
    };

    let card = load_scoped_card(&state, &project_id, &card_id).await?;
    let project = state
        .db
        .get_project(&project_id)
        .await
        .map_err(err500)?
        .ok_or_else(|| not_found("project"))?;
    let folder = state
        .db
        .get_folder(&project.folder_id)
        .await
        .map_err(err500)?
        .ok_or_else(|| not_found("folder"))?;

    // Narrate into the worker's transcript when there is one to narrate to.
    let session_id = card
        .worker_session_id
        .clone()
        .or_else(|| card.last_worker_session_id.clone());

    tracing::info!(card_id = %card_id, "Retrying worktree merge");
    let outcome = crate::worker::worktree::merge_worktree(
        &folder.path,
        &card_id,
        session_id.as_deref(),
        &state.db,
    )
    .await;

    if outcome.is_clean() {
        return Ok(Json(serde_json::json!({ "ok": true, "merged": true })));
    }
    Err((
        StatusCode::CONFLICT,
        Json(serde_json::json!({
            "error": outcome.detail.unwrap_or_else(|| "merge failed".into()),
            "reason": outcome.reason,
            "merged": outcome.merged,
        })),
    ))
}

/// GET /api/projects/:id/cards/:card_id/reports -- list reports written by this card's worker
pub(super) async fn list_card_reports(
    State(state): State<Arc<AppState>>,
    Path((project_id, card_id)): Path<(String, String)>,
) -> impl IntoResponse {
    load_scoped_card(&state, &project_id, &card_id).await?;
    let reports_dir = state.config.data_dir.join("reports");

    let mut reports = Vec::new();
    if reports_dir.exists() {
        if let Ok(folders) = std::fs::read_dir(&reports_dir) {
            for folder_entry in folders.flatten() {
                let folder_name = folder_entry.file_name().to_string_lossy().to_string();
                if let Ok(files) = std::fs::read_dir(folder_entry.path()) {
                    for file_entry in files.flatten() {
                        let file_name = file_entry.file_name().to_string_lossy().to_string();
                        if !file_name.ends_with(".md") {
                            continue;
                        }

                        if let Ok(content) = std::fs::read_to_string(file_entry.path()) {
                            if !content.starts_with("---") {
                                continue;
                            }
                            let fm = content.splitn(3, "---").nth(1).unwrap_or("");
                            let mut title = file_name.clone();
                            let mut report_card_id = None;
                            let mut date = String::new();

                            for line in fm.lines() {
                                if let Some(v) = line.strip_prefix("title: ") {
                                    title = v.trim_matches('"').to_string();
                                }
                                if let Some(v) = line.strip_prefix("cardId: ") {
                                    report_card_id = Some(v.trim_matches('"').to_string());
                                }
                                if let Some(v) = line.strip_prefix("date: ") {
                                    date = v.trim_matches('"').to_string();
                                }
                            }

                            if report_card_id.as_deref() == Some(&card_id) {
                                reports.push(serde_json::json!({
                                    "folder": folder_name,
                                    "file": file_name,
                                    "title": title,
                                    "date": date,
                                }));
                            }
                        }
                    }
                }
            }
        }
    }

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({ "reports": reports })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three states of a model pin have to survive deserialization
    /// distinctly: absent = leave alone, `null` = un-pin (inherit chain),
    /// a string = pin. Without `explicit_null`, serde collapses `null` into
    /// the outer `None` and a pinned card can never be un-pinned.
    #[test]
    fn model_pin_distinguishes_absent_null_and_value() {
        let absent: UpdateCardRequest = serde_json::from_str(r#"{"title":"x"}"#).unwrap();
        assert_eq!(absent.model, None);

        let cleared: UpdateCardRequest = serde_json::from_str(r#"{"model":null}"#).unwrap();
        assert_eq!(cleared.model, Some(None));

        let pinned: UpdateCardRequest = serde_json::from_str(r#"{"model":"mock:echo"}"#).unwrap();
        assert_eq!(pinned.model, Some(Some("mock:echo".to_string())));
    }
}

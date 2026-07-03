use axum::{Json, extract::Path, extract::State, http::StatusCode, response::IntoResponse};
use serde::Deserialize;
use std::sync::Arc;

use crate::db::models::{NewCard, UpdateCard};
use crate::state::AppState;

use super::{apply_dependencies, card_json_with_deps};

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
}

#[derive(Deserialize, serde::Serialize)]
pub(super) struct UpdateCardRequest {
    title: Option<String>,
    description: Option<String>,
    step: Option<String>,
    priority: Option<i32>,
    workflow: Option<String>,
    model: Option<Option<String>>,
    effort: Option<Option<String>>,
    worker_session_id: Option<Option<String>>,
    last_worker_session_id: Option<Option<String>>,
    handoff_context: Option<Option<String>>,
    blocked: Option<bool>,
    block_reason: Option<Option<String>>,
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
    if let crate::plugin::hooks::HookResult::Cancelled { plugin, reason } = &hook_result {
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
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

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
        .unwrap_or_default();
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
            }
        }
        items.push(value);
    }

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!(items)))
}

/// PUT /api/projects/:id/cards/:card_id
pub(super) async fn update_card(
    State(state): State<Arc<AppState>>,
    Path((_project_id, card_id)): Path<(String, String)>,
    Json(mut body): Json<UpdateCardRequest>,
) -> impl IntoResponse {
    tracing::info!(card_id = %card_id, "Updating card");

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
    if let crate::plugin::hooks::HookResult::Cancelled { plugin, reason } = &hook_result {
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
    let stale_worker_cell = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let stale_worker_writer = stale_worker_cell.clone();
    let card = state
        .db
        .update_card_atomic(&card_id, move |existing| {
            let is_terminal = existing.step == "done" || existing.step == "wont_do";

            // Terminal cards: only step changes allowed (to reopen / move).
            // depends_on edits are also blocked in terminal states.
            if is_terminal {
                let only_step = body.step.is_some()
                    && body.title.is_none()
                    && body.description.is_none()
                    && body.priority.is_none()
                    && body.workflow.is_none()
                    && body.model.is_none()
                    && body.effort.is_none()
                    && body.blocked.is_none()
                    && body.block_reason.is_none()
                    && !depends_on_present;
                if !only_step {
                    anyhow::bail!(
                        "card-update-policy: card is in terminal state — only step changes allowed"
                    );
                }
            }

            // description/workflow are locked once a card leaves backlog.
            // model, effort, title, priority, blocked, block_reason stay
            // editable in any non-terminal state.
            if existing.step != "backlog"
                && !is_terminal
                && (body.workflow.is_some() || body.description.is_some())
            {
                anyhow::bail!(
                    "card-update-policy: description and workflow are locked after leaving backlog"
                );
            }

            // If this update changes the step and the caller did NOT
            // explicitly touch worker_session_id, force-clear the
            // assignment (and stamp last_worker_session_id) so the
            // worker we're about to cancel can't keep advancing a stale
            // base step. The stash drives the post-update cancel.
            let mut worker_session_id = body.worker_session_id.clone();
            let mut last_worker_session_id = body.last_worker_session_id.clone();
            let step_changing = body.step.as_deref().is_some_and(|s| s != existing.step);
            if step_changing
                && worker_session_id.is_none()
                && let Some(sid) = existing.worker_session_id.clone()
            {
                *stale_worker_writer.lock().unwrap() = Some(sid.clone());
                worker_session_id = Some(None);
                last_worker_session_id = Some(Some(sid));
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
                updated_at: Some(chrono::Utc::now().to_rfc3339()),
                // Leave to update_card_atomic's stamper — it knows the
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
    Path((_project_id, card_id)): Path<(String, String)>,
) -> impl IntoResponse {
    tracing::info!(card_id = %card_id, "Deleting card");
    // Grab the project_id before we delete so we can still broadcast a
    // card-delete event with it.
    let project_id = state
        .db
        .get_card(&card_id)
        .await
        .ok()
        .flatten()
        .map(|c| c.project_id);

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

    if let Some(pid) = project_id {
        state
            .broadcaster
            .broadcast(crate::ws::broadcaster::WsEvent {
                event_type: "card-delete".into(),
                session_id: pid.clone(),
                data: serde_json::json!({ "cardId": card_id, "projectId": pid }),
            });
    }

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(StatusCode::NO_CONTENT)
}

/// POST /api/projects/:id/cards/:card_id/stop -- stop the card's active worker
pub(super) async fn stop_card_worker(
    State(state): State<Arc<AppState>>,
    Path((_project_id, card_id)): Path<(String, String)>,
) -> impl IntoResponse {
    tracing::info!(card_id = %card_id, "Stopping card worker");
    let card = state.db.get_card(&card_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;
    let card = card.ok_or((
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "card not found" })),
    ))?;

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
    Path((_project_id, card_id)): Path<(String, String)>,
) -> impl IntoResponse {
    tracing::info!(card_id = %card_id, "Restarting card worker");
    let card = state.db.get_card(&card_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;
    let card = card.ok_or((
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "card not found" })),
    ))?;

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

    // Unblock if blocked
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
    }

    // The watchdog/orchestrator will pick up the unassigned card on next cycle
    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({ "ok": true })))
}

/// POST /api/projects/:id/cards/:card_id/cancel-wont-do -- cancel worker and mark card as wont_do
pub(super) async fn cancel_card_wont_do(
    State(state): State<Arc<AppState>>,
    Path((_project_id, card_id)): Path<(String, String)>,
) -> impl IntoResponse {
    tracing::info!(card_id = %card_id, "Cancelling card as wont_do");
    let card = state.db.get_card(&card_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;
    let card = card.ok_or((
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "card not found" })),
    ))?;

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

/// GET /api/projects/:id/cards/:card_id/reports -- list reports written by this card's worker
pub(super) async fn list_card_reports(
    State(state): State<Arc<AppState>>,
    Path((_project_id, card_id)): Path<(String, String)>,
) -> impl IntoResponse {
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

    Json(serde_json::json!({ "reports": reports }))
}

use std::sync::Arc;

use crate::db::models::{Card, NewSession, Project, UpdateCard};
use crate::provider::stream::SpawnConfig;
use crate::service::mcp_server;
use crate::state::AppState;
use crate::worker::pipeline;
use crate::worker::scheduler::{self, WorkerIntent};
use crate::ws::broadcaster::WsEvent;

/// Broadcast a card update via WebSocket so the project page gets live updates.
fn broadcast_card_update(state: &AppState, card_id: &str, project_id: &str) {
    let db = state.db.clone();
    let broadcaster = state.broadcaster.clone();
    let card_id = card_id.to_string();
    let project_id = project_id.to_string();
    tokio::spawn(async move {
        if let Ok(Some(card)) = db.get_card(&card_id).await {
            broadcaster.broadcast(WsEvent {
                event_type: "card-update".into(),
                session_id: project_id,
                data: serde_json::json!({
                    "card": {
                        "id": card.id,
                        "project_id": card.project_id,
                        "title": card.title,
                        "description": card.description,
                        "step": card.step,
                        "priority": card.priority,
                        "workflow": card.workflow,
                        "model": card.model,
                        "effort": card.effort,
                        "worker_session_id": card.worker_session_id,
                        "last_worker_session_id": card.last_worker_session_id,
                        "handoff_context": card.handoff_context,
                        "blocked": card.blocked,
                        "block_reason": card.block_reason,
                        "created_at": card.created_at,
                        "updated_at": card.updated_at,
                    }
                }),
            });
        }
    });
}

/// Scan all active projects, find cards that need workers, and spawn them.
///
/// For each active project:
/// 1. Count currently active workers (cards with a `worker_session_id` not in
///    a terminal state).
/// 2. If active workers < project.worker_count, find unassigned, unblocked
///    cards not in terminal states.
/// 3. Spawn a worker for each available slot.
pub async fn check_and_spawn_workers(state: &Arc<AppState>) {
    let projects = state.db.list_projects().await.unwrap_or_default();
    for project in &projects {
        if project.status != "active" {
            continue;
        }

        let cards = state
            .db
            .list_cards_by_project(&project.id)
            .await
            .unwrap_or_default();

        // Count currently active workers
        let active_workers = cards
            .iter()
            .filter(|c| c.worker_session_id.is_some() && c.step != "done" && c.step != "wont_do")
            .count();

        if active_workers >= project.worker_count as usize {
            continue;
        }

        // Dependency gate: a card can only be picked up once every card it
        // depends on has reached `done`. `wont_do` does NOT satisfy a
        // dependency — a dropped prerequisite keeps the dependent waiting
        // until the user edits the dependency list.
        let dep_edges = state
            .db
            .list_dependencies_by_project(&project.id)
            .await
            .unwrap_or_default();
        let mut deps_by_card: std::collections::HashMap<&str, Vec<&str>> =
            std::collections::HashMap::new();
        for (card_id, dep_id) in &dep_edges {
            deps_by_card
                .entry(card_id.as_str())
                .or_default()
                .push(dep_id.as_str());
        }
        let step_by_id: std::collections::HashMap<&str, &str> = cards
            .iter()
            .map(|c| (c.id.as_str(), c.step.as_str()))
            .collect();
        let deps_satisfied = |card_id: &str| -> bool {
            deps_by_card.get(card_id).is_none_or(|deps| {
                deps.iter()
                    .all(|dep| step_by_id.get(dep).copied() == Some("done"))
            })
        };

        // Find unassigned, unblocked cards not in terminal states whose
        // dependencies are all satisfied.
        let available: Vec<&Card> = cards
            .iter()
            .filter(|c| {
                c.worker_session_id.is_none()
                    && !c.blocked
                    && c.step != "done"
                    && c.step != "wont_do"
                    && deps_satisfied(&c.id)
            })
            .collect();

        let slots = (project.worker_count as usize) - active_workers;
        tracing::info!(
            project_id = %project.id,
            project_name = %project.name,
            active_workers = active_workers,
            available_cards = available.len(),
            slots = slots,
            "Orchestrator check: project \"{}\"",
            project.name
        );
        for card in available.iter().take(slots) {
            if let Err(e) = spawn_worker_for_card(state, project, card).await {
                tracing::error!(
                    project_id = %project.id,
                    card_id = %card.id,
                    "Failed to spawn worker for card: {e}"
                );
            }
        }

        // Check for worker sessions with pending inter-worker messages
        // that finished but weren't re-spawned. The per-session lock makes
        // this idempotent: if a parallel tick or completion handler is
        // already mid-respawn, our check-and-act sees is_running == true
        // and skips. This is the SINGLE respawn path for inter-worker
        // messages — handle_worker_done deliberately does not duplicate it.
        if project.worker_communication {
            let worker_sessions = state
                .db
                .list_worker_sessions_by_project(&project.id)
                .await
                .unwrap_or_default();
            for ws in &worker_sessions {
                let lock = state.session_manager.lock_session(&ws.id).await;
                if state.session_manager.is_running(&ws.id).await {
                    continue;
                }
                let events = state.db.events_tail(&ws.id, 20).await.unwrap_or_default();
                if events.is_empty() {
                    continue;
                }

                let last = &events[events.len() - 1];
                // If last event is a worker-communication user message, the agent
                // never got a chance to respond — re-spawn it
                if last.kind != "user" {
                    continue;
                }
                let data = match serde_json::from_str::<serde_json::Value>(&last.data) {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                let source = data.get("source").and_then(|v| v.as_str()).unwrap_or("");
                if !matches!(
                    source,
                    "worker-communication"
                        | "worker-finding"
                        | "worker-message"
                        | "worker-notification"
                ) {
                    continue;
                }
                let text = data
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                tracing::info!(
                    session_id = %ws.id,
                    "Orchestrator: found pending worker message, resuming session"
                );

                let session_project_id = ws.project_id.clone();
                let mcp_token = state
                    .mcp_tokens
                    .issue_token(ws.id.clone(), session_project_id)
                    .await;
                let working_dir = state
                    .db
                    .get_folder(&ws.folder_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|f| f.path)
                    .unwrap_or_default();
                let mcp_config_path = mcp_server::write_mcp_config(
                    &state.config.data_dir,
                    &ws.id,
                    state.config.port,
                    &mcp_token,
                )
                .ok()
                .map(|p| p.to_string_lossy().to_string());

                let prompt = format!(
                    "IMPORTANT: You have a message from another worker that requires your response. \
                     You MUST acknowledge and respond:\n\n{}",
                    text
                );
                let _ = state
                    .db
                    .append_event(&ws.id, "user", serde_json::json!({ "text": prompt }))
                    .await;

                let config = SpawnConfig {
                    model: "default".into(),
                    effort: None,
                    working_dir,
                    mcp_config_path,
                    env: Default::default(),
                    permission_mode: Some("bypass".into()),
                    timeout_ms: None,
                    metadata: serde_json::json!({ "worker": true, "inter_worker_followup": true }),
                    system_prompt_suffix: None,
                    restrict_to_qa: false,
                };

                if let Err(e) = state
                    .session_manager
                    .send_message_locked(&lock, &prompt, &state.db, &state.broadcaster, config)
                    .await
                {
                    tracing::error!(session_id = %ws.id, "Failed to resume for pending message: {e}");
                }
            }
        }
    }
}

/// Spawn a worker agent for a specific card.
///
/// 1. Resolve the project folder.
/// 2. Create a worker session (is_worker=true, project_id, card_id).
/// 3. Issue an MCP bearer token scoped to the session/project.
/// 4. Write the per-session MCP config file.
/// 5. Build the worker prompt from `pipeline::build_worker_prompt`.
/// 6. Acquire the per-session lock and call `send_message_locked` with
///    the prompt. The lock is uncontested for a fresh uuid but routes us
///    through the single, proof-token-protected dispatch entry point.
/// 7. Update `card.worker_session_id` to point at the new session.
async fn spawn_worker_for_card(
    state: &Arc<AppState>,
    project: &Project,
    card: &Card,
) -> anyhow::Result<()> {
    // 1. Get folder
    let folder = state
        .db
        .get_folder(&project.folder_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("folder not found: {}", project.folder_id))?;

    // 2. Create worker session
    let now = chrono::Utc::now().to_rfc3339();
    let session_id = uuid::Uuid::new_v4().to_string();

    let session = state
        .db
        .create_session(NewSession {
            id: session_id.clone(),
            name: format!("worker: {}", card.title),
            folder_id: project.folder_id.clone(),
            model: card.model.clone().or_else(|| project.model.clone()),
            effort: card.effort.clone().or_else(|| project.effort.clone()),
            is_worker: true,
            project_id: Some(project.id.clone()),
            card_id: Some(card.id.clone()),
            conversation_id: None,
            created_at: now.clone(),
            last_activity: now.clone(),
            ..Default::default()
        })
        .await?;

    tracing::info!(
        session_id = %session.id,
        card_id = %card.id,
        project_id = %project.id,
        "Created worker session for card \"{}\"",
        card.title
    );

    // Check money-loop defense
    let events = state
        .db
        .events_tail(&session.id, 64)
        .await
        .unwrap_or_default();
    let (crash_count, should_block) = pipeline::detect_retry_loop(&events);
    if should_block {
        // Block the card
        let _ = state
            .db
            .update_card(
                &card.id,
                UpdateCard {
                    blocked: Some(true),
                    block_reason: Some(Some(format!(
                        "Money-loop defense: {} consecutive crashes",
                        crash_count
                    ))),
                    ..Default::default()
                },
            )
            .await;
        tracing::warn!(card_id = %card.id, crash_count, "Card blocked by money-loop defense");
        return Ok(());
    }

    // 3. Hook: mcp.token.issue.before
    let token_hook = state.plugins.dispatch(
        "mcp.token.issue.before",
        serde_json::json!({ "sessionId": &session_id, "projectId": &project.id, "role": "worker" }),
    ).await;
    if token_hook.is_cancelled() {
        tracing::info!(session_id = %session_id, "mcp.token.issue.before cancelled by plugin");
        return Ok(());
    }

    let mcp_token = state
        .mcp_tokens
        .issue_token(session_id.clone(), Some(project.id.clone()))
        .await;

    // Hook: mcp.token.issue.after
    state
        .plugins
        .dispatch(
            "mcp.token.issue.after",
            serde_json::json!({ "sessionId": &session_id, "projectId": &project.id }),
        )
        .await;

    // 4. Hook: mcp.config.write.before
    let config_hook = state
        .plugins
        .dispatch(
            "mcp.config.write.before",
            serde_json::json!({ "sessionId": &session_id, "port": state.config.port }),
        )
        .await;
    if config_hook.is_cancelled() {
        tracing::info!(session_id = %session_id, "mcp.config.write.before cancelled by plugin");
        return Ok(());
    }

    let mcp_config_path = mcp_server::write_mcp_config(
        &state.config.data_dir,
        &session_id,
        state.config.port,
        &mcp_token,
    )?;

    // Hook: mcp.config.write.after
    state
        .plugins
        .dispatch(
            "mcp.config.write.after",
            serde_json::json!({ "sessionId": &session_id }),
        )
        .await;

    // 5. Build worker prompt. The card's workflow is baked in at create
    // time, so it always carries a concrete id and the per-step prompt
    // doesn't need to consult the project as a fallback.
    let workflow_steps = crate::workflow::steps_for(Some(&card.workflow));
    // In-scope experts (project + global) so the worker can consult them.
    // A lookup failure here must not block the spawn — degrade to none.
    let experts = match state.db.list_expert_sessions_by_scope(&project.id).await {
        Ok(experts) => experts,
        Err(err) => {
            tracing::warn!(error = %err, "failed to load in-scope experts for worker prompt");
            Vec::new()
        }
    };
    let prompt = pipeline::build_worker_prompt(
        project,
        card,
        &card.step,
        &workflow_steps,
        card.handoff_context.as_deref(),
        &experts,
    );

    // 6. Build spawn config and send message
    let config = SpawnConfig {
        model: session.model.clone().unwrap_or_else(|| "default".into()),
        effort: session.effort.clone(),
        working_dir: folder.path.clone(),
        mcp_config_path: Some(mcp_config_path.to_string_lossy().to_string()),
        env: Default::default(),
        permission_mode: Some("bypass".into()),
        timeout_ms: None,
        metadata: serde_json::json!({
            "worker": true,
            "project_id": project.id,
            "card_id": card.id,
        }),
        system_prompt_suffix: None,
        restrict_to_qa: false,
    };

    // The lock is uncontested for a brand-new uuid; we acquire it anyway
    // so `send_message_locked` is the single dispatch entry point.
    let lock = state.session_manager.lock_session(&session_id).await;
    state
        .session_manager
        .send_message_locked(&lock, &prompt, &state.db, &state.broadcaster, config)
        .await?;
    drop(lock);

    // 7. Update card: assign worker and, if it's still in the intake step,
    // advance it to the workflow's second step (the first one a worker
    // actually performs). Hardcoding `in_progress` here was wrong for any
    // non-default workflow (e.g. research's second step is `research`).
    let new_step = if card.step == "backlog" || card.step == "todo" {
        workflow_steps.get(1).cloned()
    } else {
        None
    };
    state
        .db
        .update_card(
            &card.id,
            UpdateCard {
                worker_session_id: Some(Some(session_id.clone())),
                last_worker_session_id: Some(Some(session_id.clone())),
                step: new_step.clone(),
                updated_at: Some(now),
                ..Default::default()
            },
        )
        .await?;

    tracing::info!(
        session_id = %session_id,
        card_id = %card.id,
        "Worker spawned and card assigned"
    );

    // Broadcast card update to project page
    broadcast_card_update(state, &card.id, &project.id);

    Ok(())
}

/// Handle a worker session completing (called after `stream_events` finishes
/// with a Completed status).
///
/// Derives the worker intent from the event log and takes the appropriate
/// action: advance the step, finish the card, mark it won't-do, or leave it
/// as-is for user questions / continuation.
pub async fn handle_worker_done(state: &Arc<AppState>, session_id: &str) {
    // 1. Get session, find card_id
    let session = match state.db.get_session(session_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            tracing::warn!(session_id = %session_id, "handle_worker_done: session not found");
            return;
        }
        Err(e) => {
            tracing::error!(session_id = %session_id, "handle_worker_done: db error: {e}");
            return;
        }
    };

    let card_id = match &session.card_id {
        Some(id) => id.clone(),
        None => {
            tracing::warn!(session_id = %session_id, "handle_worker_done: no card_id on session");
            return;
        }
    };

    let card = match state.db.get_card(&card_id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            tracing::warn!(card_id = %card_id, "handle_worker_done: card not found");
            return;
        }
        Err(e) => {
            tracing::error!(card_id = %card_id, "handle_worker_done: db error: {e}");
            return;
        }
    };

    let project_id = match &session.project_id {
        Some(id) => id.clone(),
        None => {
            tracing::warn!(session_id = %session_id, "handle_worker_done: no project_id");
            return;
        }
    };

    // 2. Get events, derive intent
    let events = match state.db.list_events_by_session(session_id, None).await {
        Ok(evts) => evts,
        Err(e) => {
            tracing::error!(session_id = %session_id, "handle_worker_done: failed to list events: {e}");
            return;
        }
    };

    let intent = scheduler::derive_worker_intent(&events);
    let now = chrono::Utc::now().to_rfc3339();

    // 3. Act on intent
    match intent {
        Some(WorkerIntent::CompleteStep { handoff_context }) => {
            // `complete_step` is a true partial handoff: advance EXACTLY ONE
            // step. We deliberately do NOT auto-jump to `done` when a worker
            // signals "step complete" — doing so would silently skip a genuine
            // intermediate stage (e.g. `review`). A worker that has finished
            // the whole card is instead steered to call `finish_card` (which
            // lands on `done` from any step) via the prompt + tool-description
            // disambiguation in build_worker_prompt / schemas.rs.
            //
            // The card's workflow is baked in at create time, so we read
            // it directly — no project lookup needed — to walk the
            // configured steps. Without this, `complete_step` would skip
            // e.g. research's `research`/`summarize` stages.
            let workflow_steps = crate::workflow::steps_for(Some(&card.workflow));

            if let Some(next_step) = pipeline::find_next_step(&card.step, &workflow_steps) {
                // Advance step
                let _ = state
                    .db
                    .update_card(
                        &card_id,
                        UpdateCard {
                            step: Some(next_step.clone()),
                            handoff_context: Some(handoff_context.clone()),
                            worker_session_id: Some(None), // Clear to allow re-spawn
                            updated_at: Some(now.clone()),
                            ..Default::default()
                        },
                    )
                    .await;

                // Append step-change event
                let _ = state
                    .db
                    .append_event(
                        session_id,
                        "step-change",
                        serde_json::json!({
                            "from": card.step,
                            "to": next_step,
                            "cardId": card_id,
                        }),
                    )
                    .await;

                tracing::info!(
                    card_id = %card_id,
                    from = %card.step,
                    to = %next_step,
                    "Worker completed step, advancing"
                );
            } else {
                // No next step means card is done
                let _ = state
                    .db
                    .update_card(
                        &card_id,
                        UpdateCard {
                            step: Some("done".into()),
                            handoff_context: Some(handoff_context),
                            worker_session_id: Some(None),
                            updated_at: Some(now),
                            ..Default::default()
                        },
                    )
                    .await;

                tracing::info!(card_id = %card_id, "Worker completed final step, card done");
            }
        }

        Some(WorkerIntent::Finish { summary }) => {
            let _ = state
                .db
                .update_card(
                    &card_id,
                    UpdateCard {
                        step: Some("done".into()),
                        handoff_context: Some(summary.or_else(|| Some(String::new()))),
                        worker_session_id: Some(None),
                        updated_at: Some(now),
                        ..Default::default()
                    },
                )
                .await;

            tracing::info!(card_id = %card_id, "Worker finished card");
        }

        Some(WorkerIntent::WontDo { reason }) => {
            let _ = state
                .db
                .update_card(
                    &card_id,
                    UpdateCard {
                        step: Some("wont_do".into()),
                        block_reason: Some(Some(reason.clone())),
                        worker_session_id: Some(None),
                        updated_at: Some(now),
                        ..Default::default()
                    },
                )
                .await;

            tracing::info!(card_id = %card_id, reason = %reason, "Worker marked card as won't-do");
        }

        Some(WorkerIntent::AskUser { question }) => {
            // Leave as-is; the ask-user-requested event is already in the log
            // and will surface to the user through the normal event stream.
            tracing::info!(
                card_id = %card_id,
                "Worker asked user a question, leaving card assigned"
            );
            let _ = question; // already recorded via MCP tool
        }

        Some(WorkerIntent::Continue) | None => {
            // No special intent detected. Clear worker_session_id so the
            // orchestrator can re-spawn if needed.
            let _ = state
                .db
                .update_card(
                    &card_id,
                    UpdateCard {
                        worker_session_id: Some(None),
                        updated_at: Some(now),
                        ..Default::default()
                    },
                )
                .await;

            tracing::debug!(
                card_id = %card_id,
                "Worker completed with no special intent, clearing assignment"
            );
        }
    }

    // Broadcast card update to project page
    broadcast_card_update(state, &card_id, &project_id);

    // Clean up MCP config and revoke tokens
    mcp_server::delete_mcp_config(&state.config.data_dir, session_id);
    state
        .plugins
        .dispatch(
            "mcp.config.delete.after",
            serde_json::json!({ "sessionId": session_id }),
        )
        .await;

    state.mcp_tokens.revoke_by_session(session_id).await;
    state
        .plugins
        .dispatch(
            "mcp.token.revoke.after",
            serde_json::json!({ "sessionId": session_id }),
        )
        .await;

    // Inter-worker message resumption is handled by `check_and_spawn_workers`
    // (called from the completion listener after we return). Keeping it in
    // one place ensures the is_running + per-session-lock gate is the
    // single source of truth for "should this session be respawned?" —
    // otherwise the 5s orchestrator tick and this handler could both fire
    // and double-spawn.

    let _ = project_id;
}

/// Drain any persistent queued message for `session_id` and dispatch it as
/// a fresh agent run. Idempotent and lock-protected:
///
/// * If no message is queued, this is a no-op.
/// * If an agent is currently running on this session, this is a no-op —
///   the next completion will drain instead.
/// * If draining fails (e.g. spawn error), the queued message is already
///   consumed; we log and let the user retry.
///
/// Called by the completion listener for every session on every completion
/// path (success, crash, interrupt) so a `send while busy` reliably
/// delivers regardless of how the in-flight run ended.
pub async fn drain_queue_for_session(
    state: &Arc<AppState>,
    session_id: &str,
) -> anyhow::Result<()> {
    let session = match state.db.get_session(session_id).await? {
        Some(s) => s,
        None => return Ok(()),
    };

    // Peek at the queued message so we can use the model/effort the user
    // picked when they enqueued, if any. Falls back to the session →
    // card → project chain. The drain helper itself re-checks the queue
    // under the per-session lock, so this peek does NOT consume.
    let queued_peek = state.db.get_queued_message(session_id).await.ok().flatten();

    let mut model: Option<String> = queued_peek
        .as_ref()
        .and_then(|q| q.model.clone())
        .or_else(|| session.model.clone());
    let mut effort: Option<String> = queued_peek
        .as_ref()
        .and_then(|q| q.effort.clone())
        .or_else(|| session.effort.clone());
    if model.is_none() || effort.is_none() {
        if let Some(ref card_id) = session.card_id {
            if let Ok(Some(card)) = state.db.get_card(card_id).await {
                if model.is_none() {
                    model = card.model.clone();
                }
                if effort.is_none() {
                    effort = card.effort.clone();
                }
                if model.is_none() || effort.is_none() {
                    if let Ok(Some(project)) = state.db.get_project(&card.project_id).await {
                        if model.is_none() {
                            model = project.model.clone();
                        }
                        if effort.is_none() {
                            effort = project.effort.clone();
                        }
                    }
                }
            }
        }
    }

    let mcp_token = state
        .mcp_tokens
        .issue_token(session_id.to_string(), session.project_id.clone())
        .await;
    let mcp_config_path = mcp_server::write_mcp_config(
        &state.config.data_dir,
        session_id,
        state.config.port,
        &mcp_token,
    )
    .ok()
    .map(|p| p.to_string_lossy().to_string());

    let config = SpawnConfig {
        model: model.unwrap_or_else(|| "default".into()),
        effort,
        working_dir: String::new(),
        mcp_config_path,
        env: Default::default(),
        permission_mode: Some("bypass".into()),
        timeout_ms: None,
        metadata: serde_json::Value::Null,
        system_prompt_suffix: None,
        restrict_to_qa: false,
    };

    state
        .session_manager
        .drain_queued(session_id, &state.db, &state.broadcaster, config)
        .await?;
    Ok(())
}

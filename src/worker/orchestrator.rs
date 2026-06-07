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

        // Find unassigned, unblocked cards not in terminal states
        let available: Vec<&Card> = cards
            .iter()
            .filter(|c| {
                c.worker_session_id.is_none()
                    && !c.blocked
                    && c.step != "done"
                    && c.step != "wont_do"
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
    }
}

/// Spawn a worker agent for a specific card.
///
/// 1. Resolve the project folder.
/// 2. Create a worker session (is_worker=true, project_id, card_id).
/// 3. Issue an MCP bearer token scoped to the session/project.
/// 4. Write the per-session MCP config file.
/// 5. Build the worker prompt from `pipeline::build_worker_prompt`.
/// 6. Call `session_manager.send_message()` with the prompt.
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

    // 5. Build worker prompt
    let prompt =
        pipeline::build_worker_prompt(project, card, &card.step, card.handoff_context.as_deref());

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
    };

    state
        .session_manager
        .send_message(&session_id, &prompt, &state.db, &state.broadcaster, config)
        .await?;

    // 7. Update card: assign worker and move to in_progress if in backlog
    let new_step = if card.step == "backlog" || card.step == "todo" {
        Some("in_progress".to_string())
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
            // Determine workflow steps
            let workflow_steps = default_workflow_steps();

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

    // Check if there are unprocessed inter-worker messages that arrived
    // during this worker's turn. Resume even if the card is done — the agent
    // should still acknowledge messages from peers.
    let card = state.db.get_card(&card_id).await.ok().flatten();
    if let Some(ref _card) = card {
        {
            let events = state
                .db
                .events_tail(session_id, 20)
                .await
                .unwrap_or_default();
            // Find the last agent-end, then check for worker messages after it
            let last_agent_end = events.iter().rposition(|e| e.kind == "agent-end");
            let has_pending_worker_msgs = if let Some(end_idx) = last_agent_end {
                events[end_idx + 1..].iter().any(|e| {
                    if e.kind != "user" {
                        return false;
                    }
                    if let Ok(data) = serde_json::from_str::<serde_json::Value>(&e.data) {
                        let source = data.get("source").and_then(|v| v.as_str()).unwrap_or("");
                        matches!(
                            source,
                            "worker-communication"
                                | "worker-finding"
                                | "worker-message"
                                | "worker-auto-notify"
                                | "worker-notification"
                        )
                    } else {
                        false
                    }
                })
            } else {
                false
            };

            if has_pending_worker_msgs {
                tracing::info!(
                    session_id = %session_id,
                    card_id = %card_id,
                    "Pending inter-worker messages detected, resuming to process them"
                );

                // Collect the pending messages for the prompt
                let pending_msgs: Vec<String> = if let Some(end_idx) = last_agent_end {
                    events[end_idx + 1..]
                        .iter()
                        .filter_map(|e| {
                            if e.kind != "user" {
                                return None;
                            }
                            serde_json::from_str::<serde_json::Value>(&e.data)
                                .ok()
                                .and_then(|d| {
                                    let source =
                                        d.get("source").and_then(|v| v.as_str()).unwrap_or("");
                                    if matches!(
                                        source,
                                        "worker-communication"
                                            | "worker-finding"
                                            | "worker-message"
                                            | "worker-auto-notify"
                                            | "worker-notification"
                                    ) {
                                        d.get("text")
                                            .and_then(|v| v.as_str())
                                            .map(|s| s.to_string())
                                    } else {
                                        None
                                    }
                                })
                        })
                        .collect()
                } else {
                    Vec::new()
                };

                // Send a follow-up message that explicitly asks the agent to respond
                let follow_up = format!(
                    "IMPORTANT: You have {} message(s) from other workers that require your \
                     response. For EACH message below:\n\
                     1. Read and evaluate the message\n\
                     2. If it's a finding, acknowledge it and note how it affects your work\n\
                     3. If it's a question, respond using mcp__peckboard__send_worker_message\n\
                     4. If it's a file change notification, re-read affected files before editing\n\n\
                     You MUST acknowledge each message — do not ignore them.\n\n{}",
                    pending_msgs.len(),
                    pending_msgs
                        .iter()
                        .enumerate()
                        .map(|(i, m)| { format!("--- Message {} ---\n{}", i + 1, m) })
                        .collect::<Vec<_>>()
                        .join("\n\n")
                );

                // Append as user event
                let _ = state
                    .db
                    .append_event(session_id, "user", serde_json::json!({ "text": follow_up }))
                    .await;

                // Issue MCP token and resume the session
                let session_project_id = state
                    .db
                    .get_session(session_id)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|s| s.project_id);
                let mcp_token = state
                    .mcp_tokens
                    .issue_token(session_id.to_string(), session_project_id)
                    .await;
                let folder = state
                    .db
                    .get_session(session_id)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|s| Some(s.folder_id));
                let working_dir = if let Some(ref fid) = folder {
                    state
                        .db
                        .get_folder(fid)
                        .await
                        .ok()
                        .flatten()
                        .map(|f| f.path)
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let mcp_config_path = mcp_server::write_mcp_config(
                    &state.config.data_dir,
                    session_id,
                    state.config.port,
                    &mcp_token,
                )
                .ok()
                .map(|p| p.to_string_lossy().to_string());

                let config = SpawnConfig {
                    model: "default".into(),
                    effort: None,
                    working_dir,
                    mcp_config_path,
                    env: Default::default(),
                    permission_mode: Some("bypass".into()),
                    timeout_ms: None,
                    metadata: serde_json::json!({ "worker": true, "inter_worker_followup": true }),
                };

                if let Err(e) = state
                    .session_manager
                    .send_message(
                        session_id,
                        &follow_up,
                        &state.db,
                        &state.broadcaster,
                        config,
                    )
                    .await
                {
                    tracing::error!(session_id = %session_id, "Failed to resume for inter-worker messages: {e}");
                }
            }
        }
    }

    let _ = project_id;
}

/// Return the default workflow step order.
fn default_workflow_steps() -> Vec<String> {
    vec![
        "backlog".into(),
        "in_progress".into(),
        "review".into(),
        "done".into(),
    ]
}

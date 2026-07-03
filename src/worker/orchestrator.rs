use std::sync::Arc;

use crate::db::models::{Card, NewSession, Project, UpdateCard, UpdateProject};
use crate::provider::stream::SpawnConfig;
use crate::service::mcp_server;
use crate::state::AppState;
use crate::worker::pipeline;
use crate::worker::scheduler::{self, WorkerIntent};
use crate::ws::broadcaster::WsEvent;

/// Broadcast a project update so the project page can re-render fields
/// like `status` and `pause_reason` without a full reload.
fn broadcast_project_update(state: &AppState, project_id: &str) {
    let db = state.db.clone();
    let broadcaster = state.broadcaster.clone();
    let project_id = project_id.to_string();
    tokio::spawn(async move {
        if let Ok(Some(project)) = db.get_project(&project_id).await {
            broadcaster.broadcast(WsEvent {
                event_type: "project-update".into(),
                session_id: project_id,
                data: serde_json::json!({ "project": project }),
            });
        }
    });
}

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

/// Clear the persisted todo snapshot for a worker session whose card
/// just landed on a terminal step. The todo panel is the agent's
/// in-flight scratchpad — once the card is `done` / `wont_do` the items
/// no longer represent live work, and leaving them showing in the chat
/// session view, the dedicated session-todos view, and the project
/// todos roll-up makes the board look like there's outstanding work
/// when there isn't.
///
/// Three writes, in order, so every read path agrees on "no todos":
///   1. Wipe the dedicated `todos` table for this session — the source
///      of truth the `/api/sessions/:id/todos` and
///      `/api/projects/:id/todos` endpoints read.
///   2. Append an empty `todo` event so any client live-subscribed via
///      the WS drops its in-memory snapshot through the normal
///      replace-all path (`latestTodoSnapshot` returns `[]` ⇒
///      `TodoPanel` hides itself).
///   3. Broadcast that event so the drop is immediate.
///
/// No-op when the session never reported any todos — we don't want to
/// pollute every just-completed worker's event log with an empty `todo`
/// event for sessions that wouldn't have shown anything anyway.
pub async fn clear_session_todos(
    db: &crate::db::Db,
    broadcaster: &crate::ws::broadcaster::Broadcaster,
    session_id: &str,
) {
    let existing = match db.list_session_todos(session_id).await {
        Ok(items) => items,
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                "clear_session_todos: list_session_todos failed: {e}"
            );
            return;
        }
    };
    if existing.is_empty() {
        return;
    }

    if let Err(e) = db
        .replace_session_todos(session_id, crate::todo::TodoSnapshot::default())
        .await
    {
        tracing::warn!(
            session_id = %session_id,
            "clear_session_todos: replace_session_todos failed: {e}"
        );
        return;
    }

    match db
        .append_event(session_id, "todo", serde_json::json!({ "todos": [] }))
        .await
    {
        Ok(event) => {
            broadcaster.broadcast(WsEvent {
                event_type: "event".into(),
                session_id: session_id.to_string(),
                data: serde_json::json!({
                    "id": event.id,
                    "seq": event.seq,
                    "ts": event.ts,
                    "kind": "todo",
                    "data": { "todos": [] },
                }),
            });
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                "clear_session_todos: append_event failed: {e}"
            );
        }
    }
}

/// Scan all active projects, find cards that need workers, and spawn them.
///
/// For each active project:
/// 1. Count currently active workers (cards with a `worker_session_id` not in
///    a terminal state).
/// 2. If active workers < project.worker_count, find unassigned, unblocked
///    cards not in terminal states.
/// 3. Spawn a worker for each available slot.
/// Serializes orchestrator passes. The 5s interval loop and the
/// worker-completion listener both call `check_and_spawn_workers`; two
/// concurrent passes would read the same card/slot snapshot and spawn
/// past the project's worker_count. The per-card `claim_card_for_worker`
/// guard below prevents double-assigning a single card, but only this
/// gate keeps the slot arithmetic itself accurate.
static SPAWN_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub async fn check_and_spawn_workers(state: &Arc<AppState>) {
    let _gate = SPAWN_GATE.lock().await;
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
                    system_prompt_override: None,
                    // Populated in SessionManager::final_config from the plugin registry.
                    extra_allowed_tools: Vec::new(),
                };

                if let Err(e) = state
                    .session_manager
                    .send_message_locked(
                        &lock,
                        crate::provider::message::UserMessage::from_text(prompt),
                        &state.db,
                        &state.broadcaster,
                        config,
                    )
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

    let now = chrono::Utc::now().to_rfc3339();

    // The step the worker will actually run. Cards picked up from the
    // intake step are advanced to the workflow's second step (the first
    // one a worker performs) as part of the claim below.
    let workflow_steps = crate::workflow::steps_for(Some(&card.workflow));
    let new_step = if card.step == "backlog" || card.step == "todo" {
        workflow_steps.get(1).cloned()
    } else {
        None
    };
    let effective_step = new_step.clone().unwrap_or_else(|| card.step.clone());

    // 2. Reuse or create the worker session. If the card's previous worker
    // session was working THIS step and its resume link is intact (the
    // card never moved to a different real step since — detours through
    // backlog / wont_do / the blocked flag don't sever it, see
    // `sever_worker_resume_link` in db::crud::cards), resume that session:
    // the provider restores the old conversation via its conversation_id,
    // so the agent keeps its context instead of rediscovering the card
    // from scratch.
    let resume_session = match card.last_worker_session_id.as_deref() {
        Some(prev_sid) => state
            .db
            .get_session(prev_sid)
            .await
            .ok()
            .flatten()
            .filter(|prev| {
                prev.is_worker
                    && prev.card_id.as_deref() == Some(card.id.as_str())
                    && prev.worker_step.as_deref() == Some(effective_step.as_str())
                    && prev.conversation_id.is_some()
            }),
        None => None,
    };

    let (session, is_resume) = match resume_session {
        Some(prev) => {
            tracing::info!(
                session_id = %prev.id,
                card_id = %card.id,
                project_id = %project.id,
                step = %effective_step,
                "Resuming previous worker session for card \"{}\"",
                card.title
            );
            (prev, true)
        }
        None => {
            let session = state
                .db
                .create_session(NewSession {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: format!("worker: {}", card.title),
                    folder_id: project.folder_id.clone(),
                    model: card.model.clone().or_else(|| project.model.clone()),
                    // Default to medium effort when neither the card nor the
                    // project sets one — unset effort otherwise falls through
                    // to the provider's own default (high thinking on capable
                    // models), which measurably doubles worker output tokens.
                    effort: card
                        .effort
                        .clone()
                        .or_else(|| project.effort.clone())
                        .or_else(|| Some("medium".into())),
                    is_worker: true,
                    project_id: Some(project.id.clone()),
                    card_id: Some(card.id.clone()),
                    conversation_id: None,
                    created_at: now.clone(),
                    last_activity: now.clone(),
                    worker_step: Some(effective_step.clone()),
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
            (session, false)
        }
    };
    let session_id = session.id.clone();

    // Claim the card BEFORE dispatching the agent. The conditional claim
    // (WHERE worker_session_id IS NULL) is what makes concurrent spawn
    // paths safe: whoever loses the claim must not start an agent. If it's
    // the card's intake step, advance it to the workflow's second step (the
    // first one a worker actually performs).
    let claimed = state
        .db
        .claim_card_for_worker(&card.id, &session_id, new_step, &now)
        .await?;
    if !claimed {
        tracing::info!(
            card_id = %card.id,
            session_id = %session_id,
            "Card already claimed by another worker; skipping spawn"
        );
        // Only a freshly-minted session is ours to discard; a resumed one
        // predates this spawn attempt and keeps its history.
        if !is_resume {
            let _ = state.db.delete_session(&session_id).await;
        }
        return Ok(());
    }
    // Any exit between here and a successful dispatch must release the
    // claim, or the card points at a session that never ran an agent and
    // is skipped by every future tick.
    let release_claim = |reason: &'static str| {
        let db = state.db.clone();
        let card_id = card.id.to_string();
        async move {
            let released = db
                .update_card(
                    &card_id,
                    UpdateCard {
                        worker_session_id: Some(None),
                        ..Default::default()
                    },
                )
                .await;
            if let Err(e) = released {
                tracing::error!(card_id = %card_id, "Failed to release claim after {reason}: {e}");
            }
        }
    };

    // Repeated worker crashes are caught at completion time, not here:
    // `maybe_auto_pause_after_crash` in main.rs's completion listener
    // pauses the owning project once a card's lifecycle events show
    // `PAUSE_AFTER_CRASHES` consecutive non-excluded crashes. The
    // `project.status != "active"` filter at the top of
    // `check_and_spawn_workers` then skips the spawn next tick.

    // 3. Hook: mcp.token.issue.before
    let token_hook = state.plugins.dispatch(
        "mcp.token.issue.before",
        serde_json::json!({ "sessionId": &session_id, "projectId": &project.id, "role": "worker" }),
    ).await;
    if token_hook.is_cancelled() {
        tracing::info!(session_id = %session_id, "mcp.token.issue.before cancelled by plugin");
        release_claim("token hook cancel").await;
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
        release_claim("config hook cancel").await;
        return Ok(());
    }

    let mcp_config_path = match mcp_server::write_mcp_config(
        &state.config.data_dir,
        &session_id,
        state.config.port,
        &mcp_token,
    ) {
        Ok(path) => path,
        Err(e) => {
            release_claim("mcp config write failure").await;
            return Err(e);
        }
    };

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
    // doesn't need to consult the project as a fallback. `workflow_steps`
    // was computed above, before the claim.
    //
    // A resumed session gets a short "pick up where you left off" prompt
    // instead — the full assignment prompt is already in its restored
    // conversation, so we skip the instruction lookup + codebase scan too.
    let prompt = if is_resume {
        pipeline::build_worker_resume_prompt(card, &effective_step)
    } else {
        // Per-project additional step instructions, if the user set any in
        // the edit-project modal. A lookup failure must not block the spawn;
        // we fall back to no extras.
        let extra_step_instructions = state
            .db
            .get_project_workflow_instruction(&project.id, &card.workflow, &card.step)
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(error = %err, "failed to load project workflow instructions");
                None
            });
        // Scan the project folder once to give the worker a compact codebase map
        // (top-level layout + likely-relevant files + their symbol outlines) up
        // front, so it doesn't burn its opening turns — each of which re-reads
        // the whole context — re-discovering the repo. Best-effort: an
        // unreadable folder just yields no map.
        let codebase_context = {
            let root = std::path::Path::new(&folder.path);
            let files = pipeline::scan_project_files(root);
            let mut ctx = pipeline::build_codebase_context(&files, card);
            if let Some(outlines) = pipeline::build_relevant_outlines(root, &files, card) {
                ctx = Some(match ctx {
                    Some(map) => format!("{map}\n{outlines}"),
                    None => outlines,
                });
            }
            ctx
        };
        pipeline::build_worker_prompt(
            project,
            card,
            &card.step,
            &workflow_steps,
            card.handoff_context.as_deref(),
            extra_step_instructions.as_deref(),
            codebase_context.as_deref(),
        )
    };

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
        system_prompt_override: None,
        // Populated in SessionManager::final_config from the plugin registry.
        extra_allowed_tools: Vec::new(),
    };

    // The lock is uncontested for a brand-new uuid; we acquire it anyway
    // so `send_message_locked` is the single dispatch entry point. For a
    // RESUMED session the lock is real: the session may be mid-run on an
    // inter-worker follow-up, in which case we must not inject a resume
    // turn into the live agent — release the claim and let a later tick
    // retry once it winds down.
    let lock = state.session_manager.lock_session(&session_id).await;
    if is_resume && state.session_manager.is_running(&session_id).await {
        drop(lock);
        tracing::info!(
            session_id = %session_id,
            card_id = %card.id,
            "Resume target is mid-run; releasing claim until it finishes"
        );
        release_claim("resume target still running").await;
        return Ok(());
    }
    let dispatched = state
        .session_manager
        .send_message_locked(
            &lock,
            crate::provider::message::UserMessage::from_text(prompt),
            &state.db,
            &state.broadcaster,
            config,
        )
        .await;
    drop(lock);
    if let Err(e) = dispatched {
        // No agent is running — release the claim so the next tick can
        // retry the card instead of treating it as actively worked.
        release_claim("dispatch failure").await;
        return Err(e);
    }

    tracing::info!(
        session_id = %session_id,
        card_id = %card.id,
        "Worker spawned and card assigned"
    );

    // Broadcast card update to project page
    broadcast_card_update(state, &card.id, &project.id);

    Ok(())
}

/// Hard-cancel the worker assigned to `card_id`, if any, because an
/// external action (drag-drop, MCP `move_card_to_*`, etc.) made its
/// current task obsolete.
///
/// The atomic card update is the caller's job; this helper only:
/// 1. fires a cancel through every registered provider, then
/// 2. waits for the streaming task to actually wind down so the
///    synthetic `Crashed { reason: "interrupted" }` event lands before
///    we return.
///
/// Why wait: without it, the cancel's completion listener fires later
/// and runs `clear_card_worker_if_matches(card_id, session_id)`. With
/// the caller having already cleared the ref in the same atomic update,
/// the conditional-clear becomes a no-op (the values no longer match),
/// so the orchestrator's next 5s tick is free to spawn a replacement
/// for the new step without the listener clobbering its assignment.
///
/// Auto-pause is also skipped: the synthetic Crashed event carries
/// `reason: "interrupted"`, which `pipeline::count_consecutive_crashes`
/// explicitly excludes.
pub async fn cancel_worker_for_card_move(state: &Arc<AppState>, session_id: &str) {
    state.session_manager.cancel_and_wait(session_id).await;
    // Also drop any persisted queued message — the worker is going away
    // because the card moved, not because the user wants their last input
    // delivered to a fresh run.
    let _ = state.db.delete_queued_message(session_id).await;
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

    // Stale-completion guard: if the card no longer references THIS
    // session (the user moved the card, called move_card_to_done, or the
    // orchestrator already reassigned after a cancel), our intent is
    // obsolete. Acting on it would clobber a fresh assignment or
    // re-advance from a now-incorrect step. Skip the entire intent +
    // step-update block; the MCP-config / token cleanup below still
    // runs so we don't leak per-session resources.
    if card.worker_session_id.as_deref() != Some(session_id) {
        tracing::info!(
            session_id = %session_id,
            card_id = %card_id,
            current_worker = ?card.worker_session_id,
            "handle_worker_done: card reassigned, skipping intent + step update"
        );
        // Still run the resource-cleanup tail block below.
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
        return;
    }

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
                clear_session_todos(&state.db, &state.broadcaster, session_id).await;

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
            clear_session_todos(&state.db, &state.broadcaster, session_id).await;

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
            clear_session_todos(&state.db, &state.broadcaster, session_id).await;

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
            // orchestrator can re-spawn if needed, but stamp
            // last_worker_session_id: the card is still on the step this
            // session was working, so the re-spawn resumes this session's
            // conversation instead of starting a fresh agent.
            let _ = state
                .db
                .update_card(
                    &card_id,
                    UpdateCard {
                        worker_session_id: Some(None),
                        last_worker_session_id: Some(Some(session_id.to_string())),
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

    // Pause-aware: if this is a worker session whose owning project is
    // paused, do NOT drain. Pause means "stop the work"; draining here
    // would spawn a fresh agent run for a paused project the moment the
    // cancel's completion listener fires. We also drop the row so a
    // later resume doesn't re-trigger this branch with a stale message.
    if session.is_worker
        && let Some(project_id) = session.project_id.as_deref()
        && let Ok(Some(project)) = state.db.get_project(project_id).await
        && project.status != "active"
    {
        if let Ok(Some(_)) = state.db.get_queued_message(session_id).await {
            let _ = state.db.delete_queued_message(session_id).await;
            tracing::info!(
                session_id = %session_id,
                project_id = %project_id,
                "drain_queue_for_session: dropping queued message for paused project"
            );
        }
        return Ok(());
    }

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
        system_prompt_override: None,
        // Populated in SessionManager::final_config from the plugin registry.
        extra_allowed_tools: Vec::new(),
    };

    state
        .session_manager
        .drain_queued(session_id, &state.db, &state.broadcaster, config)
        .await?;
    Ok(())
}

/// After a worker crash, count consecutive crashes for the owning card
/// (across every session it has ever had) and pause the project once we
/// hit [`pipeline::PAUSE_AFTER_CRASHES`]. Skipped if the project is
/// already paused — re-pausing would clobber a more specific reason that
/// a different card hit first.
///
/// This is the load-bearing defense against the 5-second
/// spawn-respawn-crash loop when something durable is broken (rate limit,
/// invalid credentials, malformed system prompt). We never count crashes
/// whose `reason` is excluded by `pipeline::count_consecutive_crashes`
/// (e.g. `"interrupted"`, `"server-shutdown"`) — those aren't the agent's
/// fault and shouldn't poison the counter.
pub async fn maybe_auto_pause_after_crash(
    state: &Arc<AppState>,
    card_id: &str,
    last_stderr: Option<&str>,
) {
    if auto_pause_after_crash(&state.db, card_id, last_stderr).await {
        // Only broadcast on the transition (active → paused). If the
        // helper returned false (already paused, threshold not reached,
        // card/project missing) there's nothing for the UI to render.
        if let Ok(Some(card)) = state.db.get_card(card_id).await {
            broadcast_project_update(state, &card.project_id);
        }
    }
}

/// Db-only auto-pause kernel. Returns true iff the project was just
/// flipped from "active" to "paused" by this call; the wrapper above
/// broadcasts on that transition. Factored out so the integration test
/// can drive it without standing up an `AppState`.
async fn auto_pause_after_crash(
    db: &crate::db::Db,
    card_id: &str,
    last_stderr: Option<&str>,
) -> bool {
    let card = match db.get_card(card_id).await {
        Ok(Some(c)) => c,
        _ => return false,
    };
    let project = match db.get_project(&card.project_id).await {
        Ok(Some(p)) => p,
        _ => return false,
    };
    if project.status != "active" {
        // Already paused (manually or by an earlier auto-pause). Don't
        // overwrite a reason that was set first — leave it as-is so the
        // user sees the originating failure.
        return false;
    }

    let events = match db.card_lifecycle_events(card_id, 256).await {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!(card_id = %card_id, error = %err, "auto-pause: card_lifecycle_events failed");
            return false;
        }
    };
    let crash_count = pipeline::count_consecutive_crashes(&events);
    if crash_count < pipeline::PAUSE_AFTER_CRASHES {
        return false;
    }

    let reason = format_pause_reason(&card.title, crash_count, last_stderr);
    tracing::warn!(
        project_id = %project.id,
        card_id = %card.id,
        crash_count,
        "Auto-pausing project after repeated worker crashes"
    );
    let _ = db
        .update_project(
            &project.id,
            UpdateProject {
                status: Some("paused".into()),
                pause_reason: Some(Some(reason)),
                last_accessed_at: Some(chrono::Utc::now().to_rfc3339()),
                ..Default::default()
            },
        )
        .await;
    true
}

/// Append a [`pipeline::PAUSE_CLEARED_KIND`] sentinel event to the most
/// recent worker session of every card in the project that has one. The
/// auto-pause counter treats this kind as a reset marker, so the user's
/// next retry-on-resume gets a fresh [`pipeline::PAUSE_AFTER_CRASHES`]
/// attempt budget rather than failing on the first crash. Cards that
/// have never had a worker assigned have no session to anchor against
/// and are skipped — they had no crashes to count anyway.
pub async fn mark_project_resumed(db: &crate::db::Db, project_id: &str) -> anyhow::Result<()> {
    let cards = db.list_cards_by_project(project_id).await?;
    for card in cards {
        let Some(session_id) = card.last_worker_session_id.as_ref() else {
            continue;
        };
        db.append_event(
            session_id,
            pipeline::PAUSE_CLEARED_KIND,
            serde_json::json!({ "card_id": card.id }),
        )
        .await?;
    }
    Ok(())
}

/// Human-readable pause reason shown on the project page banner. Includes
/// the card title and a short snippet of the last crash's stderr so the
/// user has a starting point for what went wrong.
fn format_pause_reason(card_title: &str, crash_count: u32, stderr: Option<&str>) -> String {
    let stderr_snippet = stderr.map(str::trim).filter(|s| !s.is_empty()).map(|s| {
        // Cap at ~240 chars to keep the banner readable; rate-limit
        // notices and panic backtraces can be much longer.
        const MAX: usize = 240;
        if s.len() <= MAX {
            s.to_string()
        } else {
            let mut cut = MAX;
            while !s.is_char_boundary(cut) {
                cut -= 1;
            }
            format!("{}…", &s[..cut])
        }
    });
    match stderr_snippet {
        Some(snippet) => format!(
            "Worker for \"{card_title}\" crashed {crash_count} times in a row. Last error: {snippet}"
        ),
        None => format!("Worker for \"{card_title}\" crashed {crash_count} times in a row."),
    }
}

#[cfg(test)]
mod auto_pause_tests {
    use super::*;
    use crate::db::Db;
    use crate::db::models::{NewCard, NewFolder, NewProject, NewSession, UpdateCard};

    #[test]
    fn format_pause_reason_includes_title_and_count() {
        let msg = format_pause_reason("Ship the thing", 2, None);
        assert!(msg.contains("Ship the thing"));
        assert!(msg.contains("2 times"));
    }

    #[test]
    fn format_pause_reason_truncates_long_stderr() {
        let long = "x".repeat(1000);
        let msg = format_pause_reason("Card", 2, Some(&long));
        // The "…" marker indicates truncation occurred.
        assert!(msg.contains('…'));
        assert!(msg.len() < long.len());
    }

    #[test]
    fn format_pause_reason_omits_blank_stderr() {
        let msg = format_pause_reason("Card", 2, Some("   "));
        assert!(!msg.contains("Last error"));
    }

    /// Build the minimal DB state the auto-pause kernel walks: a folder,
    /// a project ("active"), a card, and `crash_count` distinct worker
    /// sessions each with an `agent-end` crash event whose `reason` is
    /// supplied by the caller. Sessions are timestamped so the
    /// `card_lifecycle_events` ordering is deterministic.
    async fn setup_card_with_crashes(crashes: &[(&str, &str)]) -> (Db, String, String) {
        let db = Db::in_memory().unwrap();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_project(NewProject {
            id: "p1".into(),
            name: "P".into(),
            context: "".into(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "task".into(),
            model: Some("mock:crash".into()),
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: true,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_card(NewCard {
            id: "c1".into(),
            project_id: "p1".into(),
            title: "Add auth".into(),
            description: "".into(),
            step: "in_progress".into(),
            priority: 1,
            workflow: "task".into(),
            model: None,
            effort: None,
            blocked: false,
            block_reason: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();

        // Each crash gets its own session so the test mirrors production
        // (every spawn allocates a fresh UUID). card_id links them all to
        // c1 so `card_lifecycle_events` can join them back.
        for (i, (reason, stderr)) in crashes.iter().enumerate() {
            let sid = format!("ws{i}");
            db.create_session(NewSession {
                id: sid.clone(),
                name: format!("worker-{i}"),
                folder_id: "f1".into(),
                model: Some("mock:crash".into()),
                effort: None,
                is_worker: true,
                project_id: Some("p1".into()),
                card_id: Some("c1".into()),
                conversation_id: None,
                created_at: ts.clone(),
                last_activity: ts.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
            db.update_card(
                "c1",
                UpdateCard {
                    last_worker_session_id: Some(Some(sid.clone())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            db.append_event(
                &sid,
                "agent-end",
                serde_json::json!({
                    "status": "crashed",
                    "reason": reason,
                    "stderr": stderr,
                }),
            )
            .await
            .unwrap();
        }

        (db, "c1".into(), "p1".into())
    }

    #[tokio::test]
    async fn pauses_project_after_two_process_crashes() {
        let (db, card_id, project_id) = setup_card_with_crashes(&[
            ("process exited mid-turn (code 1)", "rate limit"),
            ("process exited mid-turn (code 1)", "rate limit"),
        ])
        .await;

        let paused = auto_pause_after_crash(&db, &card_id, Some("rate limit")).await;
        assert!(paused, "kernel should report it flipped the project");

        let project = db.get_project(&project_id).await.unwrap().unwrap();
        assert_eq!(project.status, "paused");
        let reason = project.pause_reason.expect("pause_reason set");
        assert!(
            reason.contains("Add auth"),
            "card title in reason: {reason}"
        );
        assert!(
            reason.contains("2 times"),
            "crash count in reason: {reason}"
        );
        assert!(
            reason.contains("rate limit"),
            "stderr snippet in reason: {reason}"
        );
    }

    #[tokio::test]
    async fn does_not_pause_after_one_crash() {
        let (db, card_id, project_id) =
            setup_card_with_crashes(&[("process exited mid-turn (code 1)", "boom")]).await;
        let paused = auto_pause_after_crash(&db, &card_id, None).await;
        assert!(!paused);
        let project = db.get_project(&project_id).await.unwrap().unwrap();
        assert_eq!(project.status, "active");
        assert!(project.pause_reason.is_none());
    }

    /// User cancellation and the startup repair both surface as crash
    /// events but MUST NOT count — they aren't agent failures. Two
    /// `interrupted` crashes followed by one real crash must NOT pause,
    /// because the consecutive count is 1.
    #[tokio::test]
    async fn does_not_pause_on_excluded_reasons() {
        let (db, card_id, project_id) = setup_card_with_crashes(&[
            ("interrupted", ""),
            ("server-shutdown", ""),
            ("process exited mid-turn (code 1)", "boom"),
        ])
        .await;
        let paused = auto_pause_after_crash(&db, &card_id, None).await;
        assert!(!paused);
        let project = db.get_project(&project_id).await.unwrap().unwrap();
        assert_eq!(project.status, "active");
    }

    /// Two real crashes mixed with an "interrupted" between them still
    /// hits the threshold — the interrupted one is skipped, leaving two
    /// genuine consecutive crashes.
    #[tokio::test]
    async fn pauses_when_excluded_reason_is_interleaved() {
        let (db, card_id, _) = setup_card_with_crashes(&[
            ("process exited mid-turn (code 1)", "boom"),
            ("interrupted", ""),
            ("process exited mid-turn (code 1)", "boom"),
        ])
        .await;
        let paused = auto_pause_after_crash(&db, &card_id, Some("boom")).await;
        assert!(paused);
    }

    /// If the project was already paused (e.g. user paused manually, or
    /// a different card auto-paused first), don't overwrite the
    /// pre-existing reason.
    #[tokio::test]
    async fn does_not_overwrite_existing_pause_reason() {
        let (db, card_id, project_id) = setup_card_with_crashes(&[
            ("process exited mid-turn (code 1)", "boom"),
            ("process exited mid-turn (code 1)", "boom"),
        ])
        .await;
        // Manually pause first with a distinct reason.
        db.update_project(
            &project_id,
            UpdateProject {
                status: Some("paused".into()),
                pause_reason: Some(Some("manual".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let paused = auto_pause_after_crash(&db, &card_id, Some("boom")).await;
        assert!(!paused, "should not re-pause an already-paused project");
        let project = db.get_project(&project_id).await.unwrap().unwrap();
        assert_eq!(project.pause_reason.as_deref(), Some("manual"));
    }
}

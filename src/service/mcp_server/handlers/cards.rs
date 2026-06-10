use serde_json::Value;

use super::super::{McpToolRegistry, build_dependency_tree, collect_transitive_deps};
use crate::db::models::{Card, NewCard, UpdateCard};
use crate::service::mcp_server::context::ToolCallContext;

/// Append a sentinel `step-change` event for `session_id` so any later
/// `handle_worker_done` invocation reads it as the upper boundary in
/// `derive_worker_intent` and ignores the `*-requested` event we already
/// acted on. Without this, a process that eventually exits gracefully
/// would have its intent re-derived and the card would advance (or be
/// re-finished) a second time.
async fn append_step_change(
    ctx: &ToolCallContext,
    card_id: &str,
    from: &str,
    to: &str,
) -> anyhow::Result<()> {
    ctx.db
        .append_event(
            &ctx.session_id,
            "step-change",
            serde_json::json!({
                "from": from,
                "to": to,
                "cardId": card_id,
            }),
        )
        .await?;
    Ok(())
}

/// Broadcast a card-update event for the live kanban. Mirrors the shape
/// produced elsewhere in this module (e.g. `handle_update_card`).
fn broadcast_card_update(ctx: &ToolCallContext, card: &Card) {
    ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
        event_type: "card-update".into(),
        session_id: card.project_id.clone(),
        data: serde_json::json!({ "card": card }),
    });
}

/// Fire-and-forget cancel of the current worker's underlying process so
/// the next orchestrator tick can spawn a fresh worker (with a fresh
/// prompt + clean context window) for the new step. For `finish` /
/// `wont_do` the slot is already freed by the terminal step transition;
/// cancelling there too just stops a now-pointless process. We don't
/// wait on termination — `wait_for_termination` can take up to 10s and
/// the agent has already gotten its tool response. The synthetic
/// `Crashed { reason: "interrupted" }` event the cancel path emits is
/// explicitly excluded from the auto-pause counter (see
/// `pipeline::crash_reason_counts`), so cancelling a worker that just
/// finished its card cleanly will not pause the project.
async fn cancel_worker_process(ctx: &ToolCallContext) {
    if let Some(registry) = ctx.provider_registry.as_ref() {
        crate::provider::manager::cancel_via_registry(registry, &ctx.session_id).await;
    }
}

impl McpToolRegistry {
    pub(crate) async fn handle_complete_step(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let card_id = ctx
            .card_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("complete_step requires card context"))?;

        tracing::info!(session_id = %ctx.session_id, card_id = %card_id, "MCP tool: complete_step");

        let handoff_context = args
            .get("handoff_context")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Audit/replay event. Bounded by the step-change appended below
        // so a later `handle_worker_done` doesn't re-fire the intent.
        ctx.db
            .append_event(
                &ctx.session_id,
                "complete-step-requested",
                serde_json::json!({
                    "cardId": card_id,
                    "handoffContext": handoff_context,
                }),
            )
            .await?;

        // Atomic read-then-advance: the closure runs while the DB
        // connection mutex is held, so two concurrent `complete_step`
        // calls on the same card can't both see the same pre-state and
        // both advance. The pre-step is captured for the step-change
        // event we append after the write.
        let session_id = ctx.session_id.clone();
        let handoff_for_update = handoff_context.clone();
        let prev_step_cell = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let prev_step_writer = prev_step_cell.clone();
        let updated = ctx
            .db
            .update_card_atomic(card_id, move |card| {
                // Refuse if the card is already terminal — the agent
                // raced a manual user action or another worker. Idempotent
                // re-calls on `done` / `wont_do` are intentionally a
                // no-op rather than a re-advance.
                if card.step == "done" || card.step == "wont_do" {
                    anyhow::bail!(
                        "complete-step-policy: card already terminal ({})",
                        card.step
                    );
                }
                let workflow_steps = crate::workflow::steps_for(Some(&card.workflow));
                let new_step = crate::worker::pipeline::find_next_step(&card.step, &workflow_steps)
                    .unwrap_or_else(|| "done".to_string());
                *prev_step_writer.lock().unwrap() = Some(card.step.clone());

                Ok(UpdateCard {
                    step: Some(new_step),
                    handoff_context: Some(handoff_for_update.clone()),
                    // Free the slot so the orchestrator can spawn the next
                    // step's worker with a fresh prompt + clean context.
                    worker_session_id: Some(None),
                    last_worker_session_id: Some(Some(session_id.clone())),
                    updated_at: Some(chrono::Utc::now().to_rfc3339()),
                    ..Default::default()
                })
            })
            .await?;

        let card = updated.ok_or_else(|| anyhow::anyhow!("card not found: {card_id}"))?;
        let prev_step = prev_step_cell.lock().unwrap().clone().unwrap_or_default();

        append_step_change(ctx, card_id, &prev_step, &card.step).await?;
        broadcast_card_update(ctx, &card);
        cancel_worker_process(ctx).await;

        Ok(serde_json::json!({
            "status": "ok",
            "message": "Step advanced",
            "from": prev_step,
            "to": card.step,
        }))
    }

    pub(crate) async fn handle_finish_card(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let card_id = ctx
            .card_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("finish_card requires card context"))?;

        tracing::info!(session_id = %ctx.session_id, card_id = %card_id, "MCP tool: finish_card");

        let summary = args
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        ctx.db
            .append_event(
                &ctx.session_id,
                "finish-requested",
                serde_json::json!({
                    "cardId": card_id,
                    "summary": summary,
                }),
            )
            .await?;

        let session_id = ctx.session_id.clone();
        let summary_for_update = summary.clone();
        let prev_step_cell = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let prev_step_writer = prev_step_cell.clone();
        let updated = ctx
            .db
            .update_card_atomic(card_id, move |card| {
                *prev_step_writer.lock().unwrap() = Some(card.step.clone());
                // Already `done` → leave handoff context alone but treat
                // as idempotent. Already `wont_do` → the agent is
                // contradicting an explicit terminal state; refuse rather
                // than silently flipping to `done`.
                if card.step == "wont_do" {
                    anyhow::bail!("finish-card-policy: card is wont_do, cannot finish");
                }
                Ok(UpdateCard {
                    step: Some("done".into()),
                    handoff_context: Some(if summary_for_update.is_empty() {
                        None
                    } else {
                        Some(summary_for_update.clone())
                    }),
                    worker_session_id: Some(None),
                    last_worker_session_id: Some(Some(session_id.clone())),
                    updated_at: Some(chrono::Utc::now().to_rfc3339()),
                    ..Default::default()
                })
            })
            .await?;

        let card = updated.ok_or_else(|| anyhow::anyhow!("card not found: {card_id}"))?;
        let prev_step = prev_step_cell.lock().unwrap().clone().unwrap_or_default();

        append_step_change(ctx, card_id, &prev_step, "done").await?;
        broadcast_card_update(ctx, &card);
        cancel_worker_process(ctx).await;

        Ok(serde_json::json!({
            "status": "ok",
            "message": "Card finished",
            "from": prev_step,
            "to": "done",
        }))
    }

    pub(crate) async fn handle_wont_do_card(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let card_id = ctx
            .card_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("wont_do_card requires card context"))?;

        tracing::info!(session_id = %ctx.session_id, card_id = %card_id, "MCP tool: wont_do_card");

        let reason = args
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("no reason given")
            .to_string();

        ctx.db
            .append_event(
                &ctx.session_id,
                "wont-do-requested",
                serde_json::json!({
                    "cardId": card_id,
                    "reason": reason,
                }),
            )
            .await?;

        let session_id = ctx.session_id.clone();
        let reason_for_update = reason.clone();
        let prev_step_cell = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let prev_step_writer = prev_step_cell.clone();
        let updated = ctx
            .db
            .update_card_atomic(card_id, move |card| {
                *prev_step_writer.lock().unwrap() = Some(card.step.clone());
                if card.step == "done" {
                    anyhow::bail!("wont-do-card-policy: card is already done, cannot mark wont_do");
                }
                Ok(UpdateCard {
                    step: Some("wont_do".into()),
                    block_reason: Some(Some(reason_for_update.clone())),
                    worker_session_id: Some(None),
                    last_worker_session_id: Some(Some(session_id.clone())),
                    updated_at: Some(chrono::Utc::now().to_rfc3339()),
                    ..Default::default()
                })
            })
            .await?;

        let card = updated.ok_or_else(|| anyhow::anyhow!("card not found: {card_id}"))?;
        let prev_step = prev_step_cell.lock().unwrap().clone().unwrap_or_default();

        append_step_change(ctx, card_id, &prev_step, "wont_do").await?;
        broadcast_card_update(ctx, &card);
        cancel_worker_process(ctx).await;

        Ok(serde_json::json!({
            "status": "ok",
            "message": "Card marked won't-do",
            "from": prev_step,
            "to": "wont_do",
        }))
    }

    pub(crate) async fn handle_create_card(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let scope = ctx.scope_project(args.get("project_id").and_then(|v| v.as_str()))?;
        let project_id = scope.as_str();

        tracing::info!(session_id = %ctx.session_id, project_id = %project_id, "MCP tool: create_card");

        let title = args
            .get("title")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("create_card requires 'title'"))?;

        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("create_card requires 'description'"))?;

        let priority = args.get("priority").and_then(|v| v.as_i64()).unwrap_or(3) as i32;

        // A card's workflow is baked in at create time. If the caller
        // names one we validate it; otherwise we copy the project's
        // workflow into the card so the schema's NOT NULL constraint is
        // always satisfied and the card doesn't shift if the project's
        // workflow is changed later.
        let project = ctx
            .db
            .get_project(project_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("project not found: {project_id}"))?;
        let workflow = match args.get("workflow").and_then(|v| v.as_str()).map(str::trim) {
            Some(w) if !w.is_empty() => {
                if crate::workflow::workflow_by_id(w).is_none() {
                    anyhow::bail!("unknown workflow id '{w}'");
                }
                w.to_string()
            }
            _ => project.workflow.clone(),
        };
        let model = args
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let effort = args
            .get("effort")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Optional dependency ids: only keep those that are real cards in
        // the same project (a new card can't form a cycle since nothing
        // points back to it yet).
        let depends_on: Vec<String> = args
            .get("depends_on")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let now = chrono::Utc::now().to_rfc3339();
        let card_id = uuid::Uuid::new_v4().to_string();
        let card = ctx
            .db
            .create_card(NewCard {
                id: card_id.clone(),
                project_id: project_id.to_string(),
                title: title.to_string(),
                description: description.to_string(),
                step: "backlog".to_string(),
                priority,
                workflow,
                model,
                effort,
                created_at: now.clone(),
                updated_at: now,
            })
            .await?;

        if !depends_on.is_empty() {
            let project_cards = ctx.db.list_cards_by_project(project_id).await?;
            let valid: std::collections::HashSet<&str> =
                project_cards.iter().map(|c| c.id.as_str()).collect();
            let deps: Vec<String> = depends_on
                .into_iter()
                .filter(|d| d != &card_id && valid.contains(d.as_str()))
                .collect();
            ctx.db.set_card_dependencies(&card_id, deps).await?;
        }

        let deps = ctx
            .db
            .list_card_dependencies(&card_id)
            .await
            .unwrap_or_default();
        let mut card_value = serde_json::to_value(&card).unwrap_or_else(|_| serde_json::json!({}));
        if let Some(obj) = card_value.as_object_mut() {
            obj.insert("depends_on".into(), serde_json::json!(deps));
        }

        // Broadcast for live kanban
        ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "card-update".into(),
            session_id: project_id.to_string(),
            data: serde_json::json!({ "card": card_value }),
        });

        Ok(serde_json::json!({
            "status": "ok",
            "card": {
                "id": card.id,
                "title": card.title,
                "step": card.step,
                "priority": card.priority,
                "depends_on": deps,
            }
        }))
    }

    pub(crate) async fn handle_list_cards(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, "MCP tool: list_cards");

        let scope = ctx.scope_project(args.get("project_id").and_then(|v| v.as_str()))?;
        let project_id = scope.as_str();

        let cards = ctx.db.list_cards_by_project(project_id).await?;

        // Dependency edges + per-card step, so each card can report what it
        // depends on and whether those dependencies are all `done`.
        let edges = ctx
            .db
            .list_dependencies_by_project(project_id)
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
        let step_by_id: std::collections::HashMap<&str, &str> = cards
            .iter()
            .map(|c| (c.id.as_str(), c.step.as_str()))
            .collect();

        let items: Vec<Value> = cards
            .iter()
            .map(|c| {
                let deps = deps_by_card.get(c.id.as_str()).cloned().unwrap_or_default();
                let dependencies_met = deps
                    .iter()
                    .all(|dep| step_by_id.get(dep).copied() == Some("done"));
                serde_json::json!({
                    "id": c.id,
                    "title": c.title,
                    "description": c.description,
                    "step": c.step,
                    "priority": c.priority,
                    "blocked": c.blocked,
                    "block_reason": c.block_reason,
                    "workflow": c.workflow,
                    "model": c.model,
                    "effort": c.effort,
                    "worker_session_id": c.worker_session_id,
                    "has_worker": c.worker_session_id.is_some(),
                    "depends_on": deps,
                    "dependencies_met": dependencies_met,
                })
            })
            .collect();

        Ok(serde_json::json!({ "cards": items, "count": items.len(), "project_id": project_id }))
    }

    pub(crate) async fn handle_list_card_dependencies(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let card_id = args
            .get("card_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("list_card_dependencies requires 'card_id'"))?;

        tracing::info!(session_id = %ctx.session_id, card_id = %card_id, "MCP tool: list_card_dependencies");

        let card = ctx
            .db
            .get_card(card_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("card not found: {card_id}"))?;

        // Resolve dependency ids against the card's own project so we can
        // report each prerequisite's title and step.
        let project_cards = ctx.db.list_cards_by_project(&card.project_id).await?;
        let info_by_id: std::collections::HashMap<&str, &Card> =
            project_cards.iter().map(|c| (c.id.as_str(), c)).collect();

        let dep_ids = ctx.db.list_card_dependencies(card_id).await?;
        let dependencies: Vec<Value> = dep_ids
            .iter()
            .map(|id| match info_by_id.get(id.as_str()) {
                Some(c) => serde_json::json!({
                    "id": c.id,
                    "title": c.title,
                    "step": c.step,
                    "met": c.step == "done",
                }),
                None => serde_json::json!({ "id": id, "met": false }),
            })
            .collect();

        let all_met = dep_ids
            .iter()
            .all(|id| info_by_id.get(id.as_str()).map(|c| c.step.as_str()) == Some("done"));

        Ok(serde_json::json!({
            "card_id": card_id,
            "dependencies": dependencies,
            "count": dependencies.len(),
            "all_met": all_met,
        }))
    }

    pub(crate) async fn handle_get_card_dependency_tree(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let card_id = args
            .get("card_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("get_card_dependency_tree requires 'card_id'"))?;

        tracing::info!(session_id = %ctx.session_id, card_id = %card_id, "MCP tool: get_card_dependency_tree");

        let card = ctx
            .db
            .get_card(card_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("card not found: {card_id}"))?;

        let cards = ctx.db.list_cards_by_project(&card.project_id).await?;
        let edges = ctx
            .db
            .list_dependencies_by_project(&card.project_id)
            .await
            .unwrap_or_default();

        let mut deps_by_card: std::collections::HashMap<&str, Vec<&str>> =
            std::collections::HashMap::new();
        for (cid, dep_id) in &edges {
            deps_by_card
                .entry(cid.as_str())
                .or_default()
                .push(dep_id.as_str());
        }
        let info_by_id: std::collections::HashMap<&str, &Card> =
            cards.iter().map(|c| (c.id.as_str(), c)).collect();

        let mut path = std::collections::HashSet::new();
        let tree = build_dependency_tree(card_id, &deps_by_card, &info_by_id, &mut path);

        // Every transitive prerequisite (excluding the card itself) must be
        // `done` for the card to be dispatchable.
        let mut transitive = std::collections::HashSet::new();
        collect_transitive_deps(card_id, &deps_by_card, &mut transitive);
        let all_dependencies_met = transitive
            .iter()
            .all(|id| info_by_id.get(id.as_str()).map(|c| c.step.as_str()) == Some("done"));

        Ok(serde_json::json!({
            "card_id": card_id,
            "tree": tree,
            "dependency_count": transitive.len(),
            "all_dependencies_met": all_dependencies_met,
        }))
    }

    pub(crate) async fn handle_update_card(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let card_id = args
            .get("card_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("update_card requires 'card_id'"))?;
        let _scope = ctx.scope_card(card_id).await?;

        tracing::info!(session_id = %ctx.session_id, card_id = %card_id, "MCP tool: update_card");

        let update = UpdateCard {
            title: args
                .get("title")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            description: args
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            priority: args
                .get("priority")
                .and_then(|v| v.as_i64())
                .map(|n| n as i32),
            step: args
                .get("step")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            blocked: args.get("blocked").and_then(|v| v.as_bool()),
            block_reason: args
                .get("block_reason")
                .map(|v| v.as_str().map(|s| s.to_string())),
            updated_at: Some(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };

        let card = ctx
            .db
            .update_card(card_id, update)
            .await?
            .ok_or_else(|| anyhow::anyhow!("card not found: {card_id}"))?;

        ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "card-update".into(),
            session_id: card.project_id.clone(),
            data: serde_json::json!({ "card": card }),
        });

        Ok(serde_json::json!({
            "status": "ok",
            "card": {
                "id": card.id,
                "title": card.title,
                "step": card.step,
                "priority": card.priority,
                "blocked": card.blocked,
            }
        }))
    }

    pub(crate) async fn handle_delete_card(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let card_id = args
            .get("card_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("delete_card requires 'card_id'"))?;
        let _scope = ctx.scope_card(card_id).await?;

        tracing::info!(session_id = %ctx.session_id, card_id = %card_id, "MCP tool: delete_card");

        // Look up the project_id before cascade so we can broadcast a
        // card-delete event with it after the row is gone.
        let project_id = ctx
            .db
            .get_card(card_id)
            .await?
            .map(|c| c.project_id)
            .ok_or_else(|| anyhow::anyhow!("card not found: {card_id}"))?;

        // Atomic cascade.
        let _ = ctx.db.delete_card_cascade(card_id).await?;

        ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "card-delete".into(),
            session_id: project_id.clone(),
            data: serde_json::json!({ "cardId": card_id, "projectId": project_id }),
        });

        Ok(serde_json::json!({
            "status": "ok",
            "message": format!("Card {card_id} deleted"),
        }))
    }

    pub(crate) async fn handle_move_card_to_done(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let card_id = args
            .get("card_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("move_card_to_done requires 'card_id'"))?;
        let _scope = ctx.scope_card(card_id).await?;

        tracing::info!(session_id = %ctx.session_id, card_id = %card_id, "MCP tool: move_card_to_done");

        let update = UpdateCard {
            step: Some("done".to_string()),
            updated_at: Some(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };

        let card = ctx
            .db
            .update_card(card_id, update)
            .await?
            .ok_or_else(|| anyhow::anyhow!("card not found: {card_id}"))?;

        ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "card-update".into(),
            session_id: card.project_id.clone(),
            data: serde_json::json!({ "card": card }),
        });

        Ok(serde_json::json!({
            "status": "ok",
            "card": {
                "id": card.id,
                "title": card.title,
                "step": card.step,
            }
        }))
    }

    pub(crate) async fn handle_move_card_to_wont_do(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let card_id = args
            .get("card_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("move_card_to_wont_do requires 'card_id'"))?;
        let _scope = ctx.scope_card(card_id).await?;

        tracing::info!(session_id = %ctx.session_id, card_id = %card_id, "MCP tool: move_card_to_wont_do");

        let reason = args
            .get("reason")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let update = UpdateCard {
            step: Some("wont_do".to_string()),
            block_reason: Some(reason),
            updated_at: Some(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };

        let card = ctx
            .db
            .update_card(card_id, update)
            .await?
            .ok_or_else(|| anyhow::anyhow!("card not found: {card_id}"))?;

        ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "card-update".into(),
            session_id: card.project_id.clone(),
            data: serde_json::json!({ "card": card }),
        });

        Ok(serde_json::json!({
            "status": "ok",
            "card": {
                "id": card.id,
                "title": card.title,
                "step": card.step,
            }
        }))
    }
}

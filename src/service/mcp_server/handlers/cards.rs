use serde_json::Value;

use super::super::{McpToolRegistry, build_dependency_tree, collect_transitive_deps};
use crate::db::models::{Card, NewCard, UpdateCard};
use crate::service::mcp_server::context::ToolCallContext;

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

        // Record the event so the scheduler can pick it up.
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

        Ok(serde_json::json!({
            "status": "ok",
            "message": "Step completion requested"
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

        let summary = args.get("summary").and_then(|v| v.as_str()).unwrap_or("");

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

        Ok(serde_json::json!({
            "status": "ok",
            "message": "Card finish requested"
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
            .unwrap_or("no reason given");

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

        Ok(serde_json::json!({
            "status": "ok",
            "message": "Won't-do requested"
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

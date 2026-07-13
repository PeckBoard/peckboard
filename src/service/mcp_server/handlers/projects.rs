use serde_json::Value;

use super::super::McpToolRegistry;
use crate::db::models::{NewProject, UpdateProject};
use crate::service::mcp_server::context::ToolCallContext;

impl McpToolRegistry {
    pub(crate) async fn handle_list_projects(
        &self,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, folder_id = %ctx.folder_id, "MCP tool: list_projects");

        // Folder-scoped: a caller in folder F sees only F's projects.
        // Sibling folders never appear, even by id.
        let projects = ctx.db.list_projects_by_folder(&ctx.folder_id).await?;

        let items: Vec<Value> = projects
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "name": p.name,
                    "context": p.context,
                    "folder_id": p.folder_id,
                    "status": p.status,
                    "worker_count": p.worker_count,
                    "workflow": p.workflow,
                    "model": p.model,
                    "effort": p.effort,
                })
            })
            .collect();

        Ok(serde_json::json!({ "projects": items, "count": items.len() }))
    }

    pub(crate) async fn handle_create_project(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, "MCP tool: create_project");

        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("create_project requires 'name'"))?;

        // Resolve folder: prefer folder_id; else look up / create by
        // folder_path. Both paths funnel through the caller's folder
        // boundary — `scope_folder_target` rejects an explicit foreign
        // folder_id, and the folder_path branch must resolve to the
        // caller's own folder path. Without these checks, a worker in
        // folder A could materialise a project in folder B by passing a
        // different `folder_id` or `folder_path` in the arguments.
        let caller = ctx
            .db
            .get_folder(&ctx.folder_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("caller folder vanished"))?;
        let folder_id = if let Some(fid) = args.get("folder_id").and_then(|v| v.as_str()) {
            ctx.scope_folder_target(fid)?.as_str().to_string()
        } else if let Some(fp) = args.get("folder_path").and_then(|v| v.as_str()) {
            if fp != caller.path {
                anyhow::bail!(
                    "create_project is restricted to the caller's own folder \
                     (path: {})",
                    caller.path
                );
            }
            caller.id.clone()
        } else {
            // Default to the caller's own folder rather than failing —
            // omitting both args is the common, safe case.
            caller.id.clone()
        };

        let context = args
            .get("context")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let worker_count = args
            .get("worker_count")
            .and_then(|v| v.as_i64())
            .unwrap_or(1) as i32;

        // Project workflow is a required (NOT NULL) column. MCP callers
        // may pass an explicit `workflow`; otherwise we assign the
        // platform default so the resulting project ends up with an
        // actual workflow.
        let workflow_id = match args.get("workflow").and_then(|v| v.as_str()).map(str::trim) {
            Some(w) if !w.is_empty() => {
                if crate::workflow::workflow_by_id(w).is_none() {
                    anyhow::bail!("unknown workflow id '{w}'");
                }
                w.to_string()
            }
            _ => crate::workflow::DEFAULT_WORKFLOW_ID.to_string(),
        };

        let now = chrono::Utc::now().to_rfc3339();
        let project = ctx
            .db
            .create_project(NewProject {
                id: uuid::Uuid::new_v4().to_string(),
                name: name.to_string(),
                context,
                folder_id: folder_id.clone(),
                worker_count,
                status: "active".to_string(),
                workflow: workflow_id,
                model: None,
                effort: None,
                parallel_instructions: false,
                auto_notify_changes: false,
                worker_communication: false,
                created_at: now.clone(),
                last_accessed_at: now,
                budget_usd_cents: None,
                budget_period: None,
            })
            .await?;

        Ok(serde_json::json!({
            "status": "ok",
            "project": {
                "id": project.id,
                "name": project.name,
                "status": project.status,
                "workerCount": project.worker_count,
                "folderId": folder_id,
            }
        }))
    }

    pub(crate) async fn handle_update_project(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let scope = ctx
            .scope_project(args.get("project_id").and_then(|v| v.as_str()))
            .await?;
        let project_id = scope.as_str();

        tracing::info!(session_id = %ctx.session_id, project_id = %project_id, "MCP tool: update_project");

        let update = UpdateProject {
            name: args
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            context: args
                .get("context")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            worker_count: args
                .get("worker_count")
                .and_then(|v| v.as_i64())
                .map(|n| n as i32),
            status: args
                .get("status")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            last_accessed_at: Some(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };

        let project = ctx
            .db
            .update_project(project_id, update)
            .await?
            .ok_or_else(|| anyhow::anyhow!("project not found: {project_id}"))?;

        Ok(serde_json::json!({
            "status": "ok",
            "project": {
                "id": project.id,
                "name": project.name,
                "status": project.status,
                "workerCount": project.worker_count,
            }
        }))
    }

    pub(crate) async fn handle_pause_project(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let scope = ctx
            .scope_project(args.get("project_id").and_then(|v| v.as_str()))
            .await?;
        let project_id = scope.as_str();

        tracing::info!(session_id = %ctx.session_id, project_id = %project_id, "MCP tool: pause_project");

        let update = UpdateProject {
            status: Some("paused".to_string()),
            last_accessed_at: Some(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };

        let project = ctx
            .db
            .update_project(project_id, update)
            .await?
            .ok_or_else(|| anyhow::anyhow!("project not found: {project_id}"))?;

        // Cancel any in-flight workers so pause means stop, not "stop
        // spawning new ones but let the current turn finish and advance
        // the card." Mirrors the HTTP /pause route, including the
        // queued-message drop so the cancel's completion listener can't
        // drain a buffered message into a fresh agent run.
        if let Err(e) = ctx.db.delete_queued_messages_for_project(project_id).await {
            tracing::warn!(project_id = %project_id, "Failed to clear queued messages on pause: {e}");
        }
        if let Some(registry) = ctx.provider_registry.as_ref() {
            if let Ok(workers) = ctx.db.list_worker_sessions_by_project(project_id).await {
                for ws in &workers {
                    for info in registry.list_providers().await {
                        if let Some(p) = registry.get_provider(&info.id).await {
                            if p.is_running(&ws.id).await {
                                p.cancel(&ws.id).await;
                                break;
                            }
                        }
                    }
                }
            }
        }

        Ok(serde_json::json!({
            "status": "ok",
            "project": {
                "id": project.id,
                "name": project.name,
                "status": project.status,
            }
        }))
    }

    pub(crate) async fn handle_resume_project(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let scope = ctx
            .scope_project(args.get("project_id").and_then(|v| v.as_str()))
            .await?;
        let project_id = scope.as_str();

        tracing::info!(session_id = %ctx.session_id, project_id = %project_id, "MCP tool: resume_project");

        // Mirror `/api/projects/:id/resume`: clear pause_reason so a stale
        // auto-pause message doesn't linger on the project page, and
        // append the auto-pause counter reset marker so the next crash
        // gets its full retry budget.
        let update = UpdateProject {
            status: Some("active".to_string()),
            last_accessed_at: Some(chrono::Utc::now().to_rfc3339()),
            pause_reason: Some(None),
            ..Default::default()
        };

        let project = ctx
            .db
            .update_project(project_id, update)
            .await?
            .ok_or_else(|| anyhow::anyhow!("project not found: {project_id}"))?;

        if let Err(e) = crate::worker::orchestrator::mark_project_resumed(&ctx.db, project_id).await
        {
            tracing::warn!(project_id = %project_id, "Failed to mark resume sentinel: {e}");
        }

        Ok(serde_json::json!({
            "status": "ok",
            "project": {
                "id": project.id,
                "name": project.name,
                "status": project.status,
            }
        }))
    }

    pub(crate) async fn handle_delete_project(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let scope = ctx
            .scope_project(args.get("project_id").and_then(|v| v.as_str()))
            .await?;
        let project_id = scope.as_str();

        tracing::info!(session_id = %ctx.session_id, project_id = %project_id, "MCP tool: delete_project");

        // Project name for the response message (looked up before cascade
        // so we still have it).
        let project_name = ctx
            .db
            .get_project(project_id)
            .await?
            .map(|p| p.name)
            .unwrap_or_else(|| project_id.to_string());

        // Atomic cascade — single closure on the DB mutex so concurrent
        // mutations can't slip in between gather and delete.
        let report = ctx.db.delete_project_cascade(project_id).await?;

        Ok(serde_json::json!({
            "status": "ok",
            "message": format!(
                "Project '{}' deleted ({} cards, {} sessions cascaded)",
                project_name, report.cards_deleted, report.sessions_deleted,
            ),
        }))
    }
}

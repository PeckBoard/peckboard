use serde_json::Value;

use super::super::McpToolRegistry;
use crate::db::models::{NewProject, UpdateProject};
use crate::service::mcp_server::context::ToolCallContext;

impl McpToolRegistry {
    pub(crate) async fn handle_list_projects(
        &self,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, "MCP tool: list_projects");

        let projects = ctx.db.list_projects().await?;

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

        // Resolve folder: prefer folder_id; else look up / create by folder_path.
        let folder_id = if let Some(fid) = args.get("folder_id").and_then(|v| v.as_str()) {
            fid.to_string()
        } else if let Some(fp) = args.get("folder_path").and_then(|v| v.as_str()) {
            let folder_name = args
                .get("folder_name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    std::path::Path::new(fp)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(fp)
                        .to_string()
                });
            let create_if_missing = args
                .get("create_folder_if_missing")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let folder = self
                .upsert_folder(ctx, &folder_name, fp, create_if_missing)
                .await?;
            folder.id
        } else {
            anyhow::bail!("create_project requires 'folder_id' or 'folder_path'");
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
                auto_notify_changes: true,
                worker_communication: true,
                created_at: now.clone(),
                last_accessed_at: now,
            })
            .await?;

        // Idempotently give the new project its own question-expert so workers
        // can consult it before bothering the user. Non-fatal: a failure must
        // not roll back the project create (callers fall back to the global
        // question-expert). The spin_up_experts tool is the entry point for
        // the project's knowledge-experts — we do NOT auto-spin those here.
        if let Err(e) =
            crate::service::question_expert::ensure_project_question_expert(&ctx.db, &project).await
        {
            tracing::warn!(project_id = %project.id, "Failed to ensure project question-expert: {e}");
        }

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
        let scope = ctx.scope_project(args.get("project_id").and_then(|v| v.as_str()))?;
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
        let scope = ctx.scope_project(args.get("project_id").and_then(|v| v.as_str()))?;
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
        // the card." Mirrors the HTTP /pause route.
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
        let scope = ctx.scope_project(args.get("project_id").and_then(|v| v.as_str()))?;
        let project_id = scope.as_str();

        tracing::info!(session_id = %ctx.session_id, project_id = %project_id, "MCP tool: resume_project");

        let update = UpdateProject {
            status: Some("active".to_string()),
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
            }
        }))
    }

    pub(crate) async fn handle_delete_project(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let scope = ctx.scope_project(args.get("project_id").and_then(|v| v.as_str()))?;
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

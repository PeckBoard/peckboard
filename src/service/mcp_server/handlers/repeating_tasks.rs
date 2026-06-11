//! MCP tool handlers for repeating tasks. Folder-scoped via
//! [`ScopedFolderId`]: every handler starts by calling
//! `ctx.scope_folder()` which both verifies "this is a non-project
//! session" and pins the folder for the rest of the call. Worker
//! sessions get rejected at that boundary, so even a malicious
//! `task_id` argument can't reach a row outside the session's folder.

use serde_json::Value;

use super::super::McpToolRegistry;
use crate::db::models::{NewRepeatingTask, UpdateRepeatingTask};
use crate::repeating::{Schedule, initial_next_run_at};
use crate::service::mcp_server::context::{ScopedFolderId, ToolCallContext};

const MAX_NAME_LEN: usize = 200;
const MAX_DESC_LEN: usize = 2000;
const MAX_PROMPT_LEN: usize = 200_000;

fn validate_name(name: &str) -> anyhow::Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        anyhow::bail!("name is required");
    }
    if trimmed.len() > MAX_NAME_LEN {
        anyhow::bail!("name is too long (max {MAX_NAME_LEN})");
    }
    Ok(trimmed.to_string())
}

fn validate_description(desc: &str) -> anyhow::Result<String> {
    if desc.len() > MAX_DESC_LEN {
        anyhow::bail!("description is too long (max {MAX_DESC_LEN})");
    }
    Ok(desc.to_string())
}

fn validate_prompt(prompt: &str) -> anyhow::Result<String> {
    if prompt.trim().is_empty() {
        anyhow::bail!("prompt is required");
    }
    if prompt.len() > MAX_PROMPT_LEN {
        anyhow::bail!("prompt is too long (max {MAX_PROMPT_LEN} bytes)");
    }
    Ok(prompt.to_string())
}

fn validate_schedule(kind: &str, value: &Value) -> anyhow::Result<String> {
    let value_str = serde_json::to_string(value)?;
    Schedule::parse(kind, &value_str)?;
    Ok(value_str)
}

/// Resolve and verify that a task belongs to the scoped folder. The
/// proof token means the caller already knows "this session can manage
/// tasks in folder F"; this helper then confirms "task T is in F" so a
/// crafted `task_id` can't escape the scope.
async fn fetch_task_in_scope(
    ctx: &ToolCallContext,
    scope: &ScopedFolderId,
    task_id: &str,
) -> anyhow::Result<crate::db::models::RepeatingTask> {
    let task = ctx
        .db
        .get_repeating_task(task_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("repeating task not found: {task_id}"))?;
    if task.folder_id != scope.as_str() {
        anyhow::bail!(
            "repeating task {task_id} is in a different folder; this session cannot manage it"
        );
    }
    Ok(task)
}

impl McpToolRegistry {
    pub(crate) async fn handle_list_repeating_tasks(
        &self,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let scope = ctx.scope_folder().await?;
        tracing::info!(
            session_id = %ctx.session_id,
            folder_id = %scope.as_str(),
            "MCP tool: list_repeating_tasks",
        );
        let tasks = ctx
            .db
            .list_repeating_tasks_by_folder(scope.as_str())
            .await?;
        let count = tasks.len();
        Ok(serde_json::json!({
            "tasks": tasks,
            "count": count,
        }))
    }

    pub(crate) async fn handle_create_repeating_task(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let scope = ctx.scope_folder().await?;
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("name is required"))?;
        let name = validate_name(name)?;
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let description = validate_description(description)?;
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("prompt is required"))?;
        let prompt = validate_prompt(prompt)?;
        let schedule_kind = args
            .get("schedule_kind")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("schedule_kind is required"))?
            .to_string();
        let schedule_value = args
            .get("schedule_value")
            .ok_or_else(|| anyhow::anyhow!("schedule_value is required"))?;
        let schedule_value = validate_schedule(&schedule_kind, schedule_value)?;
        let model = args
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let effort = args
            .get("effort")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let enabled = args
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        tracing::info!(
            session_id = %ctx.session_id,
            folder_id = %scope.as_str(),
            name = %name,
            "MCP tool: create_repeating_task",
        );

        let now = chrono::Utc::now().to_rfc3339();
        let id = uuid::Uuid::new_v4().to_string();
        let draft = crate::db::models::RepeatingTask {
            id: id.clone(),
            name: name.clone(),
            description: description.clone(),
            folder_id: scope.as_str().to_string(),
            prompt: prompt.clone(),
            schedule_kind: schedule_kind.clone(),
            schedule_value: schedule_value.clone(),
            model: model.clone(),
            effort: effort.clone(),
            enabled,
            next_run_at: None,
            last_run_at: None,
            created_at: now.clone(),
            updated_at: now.clone(),
        };
        let next_run_at = if enabled {
            initial_next_run_at(&draft)
        } else {
            None
        };

        let task = ctx
            .db
            .create_repeating_task(NewRepeatingTask {
                id,
                name,
                description,
                folder_id: scope.as_str().to_string(),
                prompt,
                schedule_kind,
                schedule_value,
                model,
                effort,
                enabled,
                next_run_at,
                last_run_at: None,
                created_at: now.clone(),
                updated_at: now,
            })
            .await?;

        ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "repeating-task-changed".into(),
            session_id: task.id.clone(),
            data: serde_json::json!({ "action": "created", "task": &task }),
        });

        Ok(serde_json::json!({ "status": "ok", "task": task }))
    }

    pub(crate) async fn handle_update_repeating_task(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let scope = ctx.scope_folder().await?;
        let task_id = args
            .get("task_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("task_id is required"))?;
        let existing = fetch_task_in_scope(ctx, &scope, task_id).await?;

        let name = match args.get("name").and_then(|v| v.as_str()) {
            Some(n) => Some(validate_name(n)?),
            None => None,
        };
        let description = match args.get("description").and_then(|v| v.as_str()) {
            Some(d) => Some(validate_description(d)?),
            None => None,
        };
        let prompt = match args.get("prompt").and_then(|v| v.as_str()) {
            Some(p) => Some(validate_prompt(p)?),
            None => None,
        };

        // Schedule kind and value must validate together (one without
        // the other would be ambiguous).
        let schedule_kind = args.get("schedule_kind").and_then(|v| v.as_str());
        let schedule_value = args.get("schedule_value");
        let (sched_kind, sched_val) = match (schedule_kind, schedule_value) {
            (None, None) => (None, None),
            (Some(k), Some(v)) => (Some(k.to_string()), Some(validate_schedule(k, v)?)),
            (Some(k), None) => {
                let parsed: Value = serde_json::from_str(&existing.schedule_value)?;
                (Some(k.to_string()), Some(validate_schedule(k, &parsed)?))
            }
            (None, Some(v)) => (
                Some(existing.schedule_kind.clone()),
                Some(validate_schedule(&existing.schedule_kind, v)?),
            ),
        };

        // For model/effort, JSON `null` means "clear it" while an absent
        // key means "leave unchanged".
        let model = match args.get("model") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(Value::String(s)) => Some(Some(s.clone())),
            Some(_) => anyhow::bail!("model must be a string or null"),
        };
        let effort = match args.get("effort") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(Value::String(s)) => Some(Some(s.clone())),
            Some(_) => anyhow::bail!("effort must be a string or null"),
        };
        let enabled = args.get("enabled").and_then(|v| v.as_bool());

        let now = chrono::Utc::now().to_rfc3339();
        let recompute_next = sched_kind.is_some() || sched_val.is_some() || enabled.is_some();
        let next_run_at = if recompute_next {
            let draft = crate::db::models::RepeatingTask {
                id: existing.id.clone(),
                name: name.clone().unwrap_or(existing.name.clone()),
                description: description.clone().unwrap_or(existing.description.clone()),
                folder_id: existing.folder_id.clone(),
                prompt: prompt.clone().unwrap_or(existing.prompt.clone()),
                schedule_kind: sched_kind.clone().unwrap_or(existing.schedule_kind.clone()),
                schedule_value: sched_val.clone().unwrap_or(existing.schedule_value.clone()),
                model: existing.model.clone(),
                effort: existing.effort.clone(),
                enabled: enabled.unwrap_or(existing.enabled),
                next_run_at: None,
                last_run_at: existing.last_run_at.clone(),
                created_at: existing.created_at.clone(),
                updated_at: now.clone(),
            };
            if draft.enabled {
                Some(initial_next_run_at(&draft))
            } else {
                Some(None)
            }
        } else {
            None
        };

        let updated = ctx
            .db
            .update_repeating_task(
                &existing.id,
                UpdateRepeatingTask {
                    name,
                    description,
                    folder_id: None, // folder cannot be changed via MCP
                    prompt,
                    schedule_kind: sched_kind,
                    schedule_value: sched_val,
                    model,
                    effort,
                    enabled,
                    next_run_at,
                    last_run_at: None,
                    updated_at: Some(now),
                },
            )
            .await?
            .ok_or_else(|| anyhow::anyhow!("repeating task vanished mid-update"))?;

        ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "repeating-task-changed".into(),
            session_id: updated.id.clone(),
            data: serde_json::json!({ "action": "updated", "task": &updated }),
        });

        Ok(serde_json::json!({ "status": "ok", "task": updated }))
    }

    pub(crate) async fn handle_delete_repeating_task(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let scope = ctx.scope_folder().await?;
        let task_id = args
            .get("task_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("task_id is required"))?
            .to_string();
        // Confirm the task is in scope before deleting. Without this an
        // arbitrary id would let a non-project session delete tasks in
        // any folder.
        let _existing = fetch_task_in_scope(ctx, &scope, &task_id).await?;

        let deleted = ctx.db.delete_repeating_task(&task_id).await?;
        if !deleted {
            anyhow::bail!("repeating task not found: {task_id}");
        }

        ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "repeating-task-changed".into(),
            session_id: task_id.clone(),
            data: serde_json::json!({ "action": "deleted", "id": task_id }),
        });

        Ok(serde_json::json!({ "status": "ok" }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    async fn seed_db() -> Arc<crate::db::Db> {
        let db = Arc::new(crate::db::Db::in_memory().unwrap());
        let ts = chrono::Utc::now().to_rfc3339();
        use crate::db::models::{NewFolder, NewSession};
        for fid in ["f1", "f2"] {
            db.create_folder(NewFolder {
                id: fid.into(),
                name: fid.into(),
                path: format!("/tmp/{fid}"),
                created_at: ts.clone(),
            })
            .await
            .unwrap();
        }
        // Plain session bound to f1 — used by the scope test below.
        db.create_session(NewSession {
            id: "s1".into(),
            name: "S".into(),
            folder_id: "f1".into(),
            model: None,
            effort: None,
            is_worker: false,
            project_id: None,
            card_id: None,
            conversation_id: None,
            created_at: ts.clone(),
            last_activity: ts,
            repeating_task_id: None,
            ..Default::default()
        })
        .await
        .unwrap();
        db
    }

    fn ctx(db: Arc<crate::db::Db>) -> ToolCallContext {
        ToolCallContext {
            session_id: "s1".into(),
            project_id: None,
            card_id: None,
            db,
            broadcaster: crate::ws::broadcaster::Broadcaster::new(),
            provider_registry: None,
            expert_dispatcher: None,
            data_dir: None,
            folder_id: "f1".into(),
            pm_authorizations: Default::default(),
        }
    }

    #[tokio::test]
    async fn create_repeating_task_via_mcp_lands_in_session_folder() {
        let registry = crate::service::mcp_server::McpToolRegistry::new();
        let db = seed_db().await;
        let ctx = ctx(db.clone());
        let result = registry
            .handle_create_repeating_task(
                serde_json::json!({
                    "name": "daily sweep",
                    "prompt": "do thing",
                    "schedule_kind": "interval",
                    "schedule_value": { "minutes": 60 },
                }),
                &ctx,
            )
            .await
            .unwrap();
        let id = result["task"]["id"].as_str().unwrap();
        let task = db.get_repeating_task(id).await.unwrap().unwrap();
        assert_eq!(task.folder_id, "f1");
        assert!(task.enabled);
        assert!(task.next_run_at.is_some());
    }

    #[tokio::test]
    async fn update_repeating_task_rejects_task_in_a_different_folder() {
        let registry = crate::service::mcp_server::McpToolRegistry::new();
        let db = seed_db().await;

        // Create a task in f2 directly (out of scope for the s1 session).
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_repeating_task(crate::db::models::NewRepeatingTask {
            id: "t-foreign".into(),
            name: "foreign".into(),
            description: "".into(),
            folder_id: "f2".into(),
            prompt: "x".into(),
            schedule_kind: "interval".into(),
            schedule_value: r#"{"minutes":60}"#.into(),
            model: None,
            effort: None,
            enabled: true,
            next_run_at: None,
            last_run_at: None,
            created_at: ts.clone(),
            updated_at: ts,
        })
        .await
        .unwrap();

        let ctx = ctx(db);
        let err = registry
            .handle_update_repeating_task(
                serde_json::json!({ "task_id": "t-foreign", "name": "hijacked" }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("different folder"), "got: {err}");
    }

    #[tokio::test]
    async fn delete_repeating_task_rejects_out_of_scope_task() {
        let registry = crate::service::mcp_server::McpToolRegistry::new();
        let db = seed_db().await;

        let ts = chrono::Utc::now().to_rfc3339();
        db.create_repeating_task(crate::db::models::NewRepeatingTask {
            id: "t-foreign".into(),
            name: "foreign".into(),
            description: "".into(),
            folder_id: "f2".into(),
            prompt: "x".into(),
            schedule_kind: "interval".into(),
            schedule_value: r#"{"minutes":60}"#.into(),
            model: None,
            effort: None,
            enabled: true,
            next_run_at: None,
            last_run_at: None,
            created_at: ts.clone(),
            updated_at: ts,
        })
        .await
        .unwrap();

        let ctx = ctx(db.clone());
        let err = registry
            .handle_delete_repeating_task(serde_json::json!({ "task_id": "t-foreign" }), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("different folder"), "got: {err}");
        // The task must still exist.
        assert!(db.get_repeating_task("t-foreign").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn list_repeating_tasks_only_returns_session_folder_entries() {
        let registry = crate::service::mcp_server::McpToolRegistry::new();
        let db = seed_db().await;
        let ts = chrono::Utc::now().to_rfc3339();
        for (id, folder) in [("t-in", "f1"), ("t-out", "f2")] {
            db.create_repeating_task(crate::db::models::NewRepeatingTask {
                id: id.into(),
                name: id.into(),
                description: "".into(),
                folder_id: folder.into(),
                prompt: "x".into(),
                schedule_kind: "interval".into(),
                schedule_value: r#"{"minutes":60}"#.into(),
                model: None,
                effort: None,
                enabled: true,
                next_run_at: None,
                last_run_at: None,
                created_at: ts.clone(),
                updated_at: ts.clone(),
            })
            .await
            .unwrap();
        }
        let ctx = ctx(db);
        let result = registry.handle_list_repeating_tasks(&ctx).await.unwrap();
        let tasks = result["tasks"].as_array().unwrap();
        let ids: Vec<&str> = tasks.iter().map(|t| t["id"].as_str().unwrap()).collect();
        assert_eq!(
            ids,
            vec!["t-in"],
            "must only list tasks in session's folder"
        );
    }

    #[tokio::test]
    async fn create_repeating_task_rejects_invalid_schedule() {
        let registry = crate::service::mcp_server::McpToolRegistry::new();
        let db = seed_db().await;
        let ctx = ctx(db);
        let err = registry
            .handle_create_repeating_task(
                serde_json::json!({
                    "name": "x",
                    "prompt": "x",
                    "schedule_kind": "interval",
                    "schedule_value": { "minutes": 0 },
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("minutes must be"), "got: {err}");
    }
}

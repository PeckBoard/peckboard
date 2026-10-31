//! MCP tool handlers for repeating tasks. Folder-scoped via
//! [`ScopedFolderId`]: every handler starts by calling
//! `ctx.scope_folder()` which both verifies "this is a non-project
//! session" and pins the folder for the rest of the call. Worker
//! sessions get rejected at that boundary, so even a malicious
//! `task_id` argument can't reach a row outside the session's folder.

use serde_json::Value;

use super::super::McpToolRegistry;
use crate::db::models::{NewRepeatingTask, UpdateRepeatingTask};
use crate::repeating::{Schedule, TARGET_ASSISTANT, initial_next_run_at, server_timezone};
use crate::service::mcp_server::context::{ScopedFolderId, ToolCallContext};
use crate::service::voice_relay::VOICE_EXPERT_KIND;

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

/// An IANA zone name chrono-tz knows, so a typo can't silently fall back to
/// UTC for the schedule's lifetime.
fn validate_timezone(tz: &str) -> anyhow::Result<String> {
    let tz = tz.trim();
    tz.parse::<chrono_tz::Tz>()
        .map(|_| tz.to_string())
        .map_err(|_| {
            anyhow::anyhow!("unknown timezone: {tz} (use an IANA name like Europe/Berlin)")
        })
}

/// The `target` argument as the stored column: absent = `None`,
/// `"session"` = `Some(None)`, `"assistant"` = `Some(Some(..))`. Only the
/// Assistant itself may target the Assistant: such a task runs with the
/// Assistant's full toolset, so any other session could otherwise schedule
/// instructions for it.
fn parse_target(
    v: Option<&Value>,
    caller_is_assistant: bool,
) -> anyhow::Result<Option<Option<String>>> {
    match v.and_then(|v| v.as_str()) {
        None => Ok(None),
        Some("session") => Ok(Some(None)),
        Some(TARGET_ASSISTANT) if caller_is_assistant => {
            Ok(Some(Some(TARGET_ASSISTANT.to_string())))
        }
        Some(TARGET_ASSISTANT) => {
            anyhow::bail!("only the Assistant can schedule tasks that run in the Assistant")
        }
        Some(other) => anyhow::bail!("unknown target '{other}'; use session or assistant"),
    }
}

/// Is the calling session the Assistant (the global voice session)?
async fn caller_is_assistant(ctx: &ToolCallContext) -> anyhow::Result<bool> {
    Ok(ctx
        .db
        .get_session(&ctx.session_id)
        .await?
        .is_some_and(|s| s.expert_kind.as_deref() == Some(VOICE_EXPERT_KIND)))
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
        crate::provider::registry::check_model(model.as_deref()).map_err(anyhow::Error::msg)?;
        let effort = args
            .get("effort")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let enabled = args
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        // The Assistant's own tasks default to running in the Assistant
        // ("remind me at five"); everyone else's to a fresh session.
        let caller_is_assistant = caller_is_assistant(ctx).await?;
        let target = match parse_target(args.get("target"), caller_is_assistant)? {
            Some(t) => t,
            None => caller_is_assistant.then(|| TARGET_ASSISTANT.to_string()),
        };
        // No browser reports the user's zone here: default to the server's
        // own zone, which is what "at nine" means to the person running it.
        let timezone = match args.get("timezone").and_then(|v| v.as_str()) {
            Some(z) => Some(validate_timezone(z)?),
            None => server_timezone(),
        };
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
            timezone: timezone.clone(),
            target: target.clone(),
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
                timezone,
                target,
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
        let caller_is_assistant = caller_is_assistant(ctx).await?;
        if existing.target.as_deref() == Some(TARGET_ASSISTANT) && !caller_is_assistant {
            anyhow::bail!("only the Assistant can change a task that runs in the Assistant");
        }
        let target = parse_target(args.get("target"), caller_is_assistant)?;
        let timezone = match args.get("timezone") {
            None => None,
            // null = back to UTC.
            Some(Value::Null) => Some(None),
            Some(Value::String(z)) => Some(Some(validate_timezone(z)?)),
            Some(_) => anyhow::bail!("timezone must be a string or null"),
        };

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
        crate::provider::registry::check_model(model.as_ref().and_then(|m| m.as_deref()))
            .map_err(anyhow::Error::msg)?;
        let enabled = args.get("enabled").and_then(|v| v.as_bool());

        let now = chrono::Utc::now().to_rfc3339();
        let recompute_next =
            sched_kind.is_some() || sched_val.is_some() || enabled.is_some() || timezone.is_some();
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
                timezone: timezone.clone().unwrap_or(existing.timezone.clone()),
                target: existing.target.clone(),
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
                    timezone,
                    target,
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
            data_dir: None,
            folder_id: "f1".into(),
            device_registry: None,
            background: None,
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
            timezone: None,
            target: None,
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
            timezone: None,
            target: None,
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
                timezone: None,
                target: None,
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

    /// The Assistant's tasks default to running in the Assistant and take a
    /// timezone; any other session gets a fresh-session task and may not
    /// target (or edit) the Assistant.
    #[tokio::test]
    async fn target_and_timezone_follow_the_caller() {
        let registry = crate::service::mcp_server::McpToolRegistry::new();
        let db = seed_db().await;
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_session(crate::db::models::NewSession {
            id: "v1".into(),
            name: "Assistant".into(),
            folder_id: "f1".into(),
            created_at: ts.clone(),
            last_activity: ts,
            is_expert: true,
            expert_kind: Some(VOICE_EXPERT_KIND.into()),
            ..Default::default()
        })
        .await
        .unwrap();
        let voice = ToolCallContext {
            session_id: "v1".into(),
            ..ctx(db.clone())
        };
        let plain = ctx(db.clone());
        let reminder = serde_json::json!({
            "name": "call Sam",
            "prompt": "Remind the user to call Sam.",
            "schedule_kind": "once",
            "schedule_value": { "at": "2099-01-01T17:00" },
            "timezone": "Europe/Berlin",
        });

        let made = registry
            .handle_create_repeating_task(reminder.clone(), &voice)
            .await
            .unwrap();
        let task = db
            .get_repeating_task(made["task"]["id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(task.target.as_deref(), Some(TARGET_ASSISTANT));
        assert_eq!(task.timezone.as_deref(), Some("Europe/Berlin"));
        // 17:00 in Berlin (CET, winter) is 16:00 UTC.
        assert!(
            task.next_run_at
                .as_deref()
                .unwrap()
                .starts_with("2099-01-01T16:00"),
            "{:?}",
            task.next_run_at
        );

        // A plain session defaults to a fresh session per run, can't target
        // the Assistant, and can't edit the Assistant's task.
        let own = registry
            .handle_create_repeating_task(reminder.clone(), &plain)
            .await
            .unwrap();
        assert!(own["task"]["target"].is_null());
        let mut hijack = reminder.clone();
        hijack["target"] = "assistant".into();
        let err = registry
            .handle_create_repeating_task(hijack, &plain)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("only the Assistant"), "{err}");
        let err = registry
            .handle_update_repeating_task(
                serde_json::json!({ "task_id": task.id, "prompt": "Delete everything." }),
                &plain,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("only the Assistant"), "{err}");

        let bad = serde_json::json!({ "task_id": task.id, "timezone": "Mars/Olympus" });
        let err = registry
            .handle_update_repeating_task(bad, &voice)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown timezone"), "{err}");
    }
}

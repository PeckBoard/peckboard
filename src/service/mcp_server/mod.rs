//! MCP tool registry and dispatch for the per-session worker stdio
//! server. The public surface (`McpTokenRegistry`, `McpToolRegistry`,
//! `ToolCallContext`, `write_mcp_config`, `delete_mcp_config`) is
//! re-exported here so external callers keep importing from
//! `crate::service::mcp_server::…`.

mod auth;
mod config;
mod context;
mod handlers;
mod schemas;
mod spawn;

pub use auth::McpTokenRegistry;
pub use config::{delete_mcp_config, write_mcp_config};
pub use context::{ExpertDispatcher, McpToolDef, ScopedFolderId, ScopedProjectId, ToolCallContext};
pub use spawn::{AppExpertDispatcher, AppLiveHost};

use serde_json::Value;

use crate::db::models::Card;
use context::CommRateLimiter;

/// Registry of MCP tools exposed to workers via stdio MCP server.
pub struct McpToolRegistry {
    tools: Vec<McpToolDef>,
    // Handlers in submodules need this for rate-limit checks; visibility
    // is scoped to the mcp_server module so it never leaks outside.
    pub(in crate::service::mcp_server) comm_limiter: CommRateLimiter,
}

impl Default for McpToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl McpToolRegistry {
    pub fn new() -> Self {
        McpToolRegistry {
            tools: schemas::tool_definitions(),
            // 20 inter-worker messages per project per 60 seconds
            comm_limiter: CommRateLimiter::new(20, 60),
        }
    }

    /// Return the list of tool definitions (for MCP tools/list).
    pub fn tool_definitions(&self) -> &[McpToolDef] {
        &self.tools
    }

    /// Dispatch a tool call to the appropriate handler.
    pub async fn handle_tool_call(
        &self,
        tool_name: &str,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        match tool_name {
            "complete_step" => self.handle_complete_step(args, ctx).await,
            "finish_card" => self.handle_finish_card(args, ctx).await,
            "wont_do_card" => self.handle_wont_do_card(args, ctx).await,
            "ask_user" => self.handle_ask_user(args, ctx).await,
            "create_card" => self.handle_create_card(args, ctx).await,
            "list_cards" => self.handle_list_cards(args, ctx).await,
            "list_card_dependencies" => self.handle_list_card_dependencies(args, ctx).await,
            "get_card_dependency_tree" => self.handle_get_card_dependency_tree(args, ctx).await,
            "list_projects" => self.handle_list_projects(ctx).await,
            "list_workflows" => self.handle_list_workflows(args, ctx).await,
            "set_workflow_instructions" => self.handle_set_workflow_instructions(args, ctx).await,
            "write_report" => self.handle_write_report(args, ctx).await,
            "attach_report_file" => self.handle_attach_report_file(args, ctx).await,
            "update_card" => self.handle_update_card(args, ctx).await,
            "update_project" => self.handle_update_project(args, ctx).await,
            "create_project" => self.handle_create_project(args, ctx).await,
            "create_folder" => self.handle_create_folder(args, ctx).await,
            "list_folders" => self.handle_list_folders(ctx).await,
            "pause_project" => self.handle_pause_project(args, ctx).await,
            "resume_project" => self.handle_resume_project(args, ctx).await,
            "delete_project" => self.handle_delete_project(args, ctx).await,
            "delete_card" => self.handle_delete_card(args, ctx).await,
            "move_card_to_done" => self.handle_move_card_to_done(args, ctx).await,
            "move_card_to_wont_do" => self.handle_move_card_to_wont_do(args, ctx).await,
            "notify_workers" => self.handle_notify_workers(args, ctx).await,
            "fetch_url" => self.handle_fetch_url(args, ctx).await,
            "list_models" => self.handle_list_models(ctx).await,
            "list_project_reports" => self.handle_list_project_reports(ctx).await,
            "read_report" => self.handle_read_report(args, ctx).await,
            "read_worker_session" => self.handle_read_worker_session(args, ctx).await,
            "search_sessions" => self.handle_search_sessions(args, ctx).await,
            "list_sessions" => self.handle_list_sessions(ctx).await,
            "list_worker_sessions" => self.handle_list_worker_sessions(ctx).await,
            "share_finding" => self.handle_share_finding(args, ctx).await,
            "get_finding_details" => self.handle_get_finding_details(args, ctx).await,
            "send_worker_message" => self.handle_send_worker_message(args, ctx).await,
            "list_repeating_tasks" => self.handle_list_repeating_tasks(ctx).await,
            "create_repeating_task" => self.handle_create_repeating_task(args, ctx).await,
            "update_repeating_task" => self.handle_update_repeating_task(args, ctx).await,
            "delete_repeating_task" => self.handle_delete_repeating_task(args, ctx).await,
            _ => anyhow::bail!("unknown tool: {tool_name}"),
        }
    }

    // ── Shared helpers used by handlers across submodules ───────────────

    /// Returns true if the worker session is associated with a card in a
    /// terminal state (`done` or `wont_do`). Used to filter inter-worker
    /// communication so finished cards don't get woken up.
    pub(super) async fn is_session_terminal(
        &self,
        ctx: &ToolCallContext,
        session: &crate::db::models::Session,
    ) -> bool {
        let Some(card_id) = session.card_id.as_deref() else {
            return false;
        };
        match ctx.db.get_card(card_id).await {
            Ok(Some(card)) => card.step == "done" || card.step == "wont_do",
            _ => false,
        }
    }

    /// Deliver a message to a worker session immediately: append a user
    /// event (so the agent sees it on resume) and broadcast for in-flight
    /// stdin delivery to any running agent.
    pub(super) async fn deliver_to_worker(
        &self,
        ctx: &ToolCallContext,
        target_session_id: &str,
        message: &str,
    ) {
        // Append as user event for persistence (agent sees it on resume)
        let _ = ctx
            .db
            .append_event(
                target_session_id,
                "user",
                serde_json::json!({
                    "text": message,
                    "source": "worker-communication",
                }),
            )
            .await;

        // Broadcast for immediate stdin delivery to running agent
        // (agent sees it in context, and handle_worker_done will force
        // a follow-up turn if the agent didn't respond)
        ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "worker-stdin-deliver".into(),
            session_id: target_session_id.to_string(),
            data: serde_json::json!({ "text": message }),
        });
    }

    /// Resolve project_id from context or session lookup.
    pub(super) async fn resolve_project_id(&self, ctx: &ToolCallContext) -> Option<String> {
        if ctx.project_id.is_some() {
            return ctx.project_id.clone();
        }
        ctx.db
            .get_session(&ctx.session_id)
            .await
            .ok()
            .flatten()
            .and_then(|s| s.project_id)
    }

    /// Resolve card title from context or session lookup.
    pub(super) async fn resolve_card_title(&self, ctx: &ToolCallContext) -> Option<String> {
        let card_id = if ctx.card_id.is_some() {
            ctx.card_id.clone()
        } else {
            ctx.db
                .get_session(&ctx.session_id)
                .await
                .ok()
                .flatten()
                .and_then(|s| s.card_id)
        };
        if let Some(ref cid) = card_id {
            ctx.db.get_card(cid).await.ok().flatten().map(|c| c.title)
        } else {
            None
        }
    }
}

// ── Free helpers ────────────────────────────────────────────────────

/// Build a nested dependency tree rooted at `card_id`. `path` tracks the
/// nodes on the current branch so a cycle (which the `would_create_cycle`
/// guard normally prevents, but we stay defensive) is reported as a leaf
/// with `"cycle": true` instead of recursing forever.
pub(super) fn build_dependency_tree(
    card_id: &str,
    deps_by_card: &std::collections::HashMap<&str, Vec<&str>>,
    info_by_id: &std::collections::HashMap<&str, &Card>,
    path: &mut std::collections::HashSet<String>,
) -> Value {
    let (title, step) = match info_by_id.get(card_id) {
        Some(c) => (c.title.as_str(), c.step.as_str()),
        None => ("<unknown>", "<unknown>"),
    };

    if !path.insert(card_id.to_string()) {
        return serde_json::json!({
            "id": card_id,
            "title": title,
            "step": step,
            "cycle": true,
        });
    }

    let depends_on: Vec<Value> = deps_by_card
        .get(card_id)
        .map(|deps| {
            deps.iter()
                .map(|d| build_dependency_tree(d, deps_by_card, info_by_id, path))
                .collect()
        })
        .unwrap_or_default();

    path.remove(card_id);

    serde_json::json!({
        "id": card_id,
        "title": title,
        "step": step,
        "depends_on": depends_on,
    })
}

/// Collect every transitive dependency id of `card_id` (excluding itself)
/// into `seen`. The `seen` set doubles as cycle protection.
pub(super) fn collect_transitive_deps(
    card_id: &str,
    deps_by_card: &std::collections::HashMap<&str, Vec<&str>>,
    seen: &mut std::collections::HashSet<String>,
) {
    if let Some(deps) = deps_by_card.get(card_id) {
        for dep in deps {
            if seen.insert(dep.to_string()) {
                collect_transitive_deps(dep, deps_by_card, seen);
            }
        }
    }
}

/// Run one MCP tool call end to end, exactly as the `/mcp` JSON-RPC route
/// does, so the route and any other dispatcher (e.g. the Ollama provider's
/// tool-calling loop) can't drift:
///
/// 1. Fire the `mcp.tool.call.before` observer hook — a plugin may rewrite
///    the arguments (via the returned `args`) or cancel the call outright.
/// 2. Dispatch to the active plugin that declared the tool, falling back to
///    core's own [`McpToolRegistry::handle_tool_call`] when no plugin claims
///    the name (mirrors [`crate::plugin::manager::PluginManager::invoke_mcp_tool`]).
/// 3. Fire `mcp.tool.call.after` (success) or `mcp.tool.call.failed` (error).
///
/// Returns the tool result, or an error (a plugin cancellation, an invalid
/// verdict, or the handler's own failure). The caller maps it onto its own
/// transport — a JSON-RPC error for the route, a `role: "tool"` error body
/// for Ollama.
pub async fn dispatch_tool_call(
    plugins: &crate::plugin::manager::PluginManager,
    registry: &McpToolRegistry,
    tool_name: &str,
    arguments: Value,
    ctx: &ToolCallContext,
) -> anyhow::Result<Value> {
    use crate::plugin::hooks::HookResult;

    // ── Hook: mcp.tool.call.before ── (may rewrite args or cancel)
    let mut final_args = arguments;
    match plugins
        .dispatch(
            "mcp.tool.call.before",
            serde_json::json!({
                "sessionId": &ctx.session_id,
                "toolName": tool_name,
                "args": &final_args,
            }),
        )
        .await
    {
        HookResult::Cancelled { plugin, reason } => {
            tracing::info!(plugin = %plugin, reason = %reason, "mcp.tool.call.before cancelled");
            anyhow::bail!("cancelled by plugin {plugin}: {reason}");
        }
        HookResult::Allowed(modified) => {
            if let Some(new_args) = modified.get("args") {
                final_args = new_args.clone();
            }
        }
    }

    // A plugin that declared this tool owns the call; otherwise core handles
    // it. The scoped `ctx` carries session/project/card/folder so plugin and
    // core dispatch enforce the same folder boundary.
    let plugin_ctx = serde_json::json!({
        "sessionId": &ctx.session_id,
        "projectId": &ctx.project_id,
        "cardId": &ctx.card_id,
        "folderId": &ctx.folder_id,
    });
    let tool_result = match plugins
        .invoke_mcp_tool(tool_name, final_args.clone(), plugin_ctx)
        .await
    {
        Some(r) => r,
        None => registry.handle_tool_call(tool_name, final_args, ctx).await,
    };

    match &tool_result {
        Ok(result) => {
            plugins
                .dispatch(
                    "mcp.tool.call.after",
                    serde_json::json!({
                        "sessionId": &ctx.session_id,
                        "toolName": tool_name,
                        "result": result,
                    }),
                )
                .await;
        }
        Err(e) => {
            plugins
                .dispatch(
                    "mcp.tool.call.failed",
                    serde_json::json!({
                        "sessionId": &ctx.session_id,
                        "toolName": tool_name,
                        "reason": e.to_string(),
                    }),
                )
                .await;
        }
    }
    tool_result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_tool_registry_has_all_tools() {
        let registry = McpToolRegistry::new();
        let names: Vec<&str> = registry
            .tool_definitions()
            .iter()
            .map(|t| t.name.as_str())
            .collect();

        assert!(names.contains(&"complete_step"));
        assert!(names.contains(&"finish_card"));
        assert!(names.contains(&"wont_do_card"));
        assert!(names.contains(&"ask_user"));
        assert!(names.contains(&"create_card"));
        assert!(names.contains(&"list_cards"));
        assert!(names.contains(&"list_card_dependencies"));
        assert!(names.contains(&"get_card_dependency_tree"));
        assert!(names.contains(&"list_projects"));
        assert!(names.contains(&"list_workflows"));
        assert!(names.contains(&"set_workflow_instructions"));
        assert!(names.contains(&"write_report"));
        assert!(names.contains(&"attach_report_file"));
        assert!(names.contains(&"update_card"));
        assert!(names.contains(&"update_project"));
        assert!(names.contains(&"create_project"));
        assert!(names.contains(&"pause_project"));
        assert!(names.contains(&"resume_project"));
        assert!(names.contains(&"delete_card"));
        assert!(names.contains(&"delete_project"));
        assert!(names.contains(&"move_card_to_done"));
        assert!(names.contains(&"move_card_to_wont_do"));
        assert!(names.contains(&"create_folder"));
        assert!(names.contains(&"list_folders"));
        assert!(names.contains(&"list_repeating_tasks"));
        assert!(names.contains(&"create_repeating_task"));
        assert!(names.contains(&"update_repeating_task"));
        assert!(names.contains(&"delete_repeating_task"));
        assert!(names.contains(&"search_sessions"));
        assert!(names.contains(&"list_sessions"));
        assert_eq!(names.len(), 40);
    }

    #[test]
    fn test_tool_definitions_have_valid_schemas() {
        let registry = McpToolRegistry::new();
        for tool in registry.tool_definitions() {
            assert!(!tool.name.is_empty());
            assert!(!tool.description.is_empty());
            assert_eq!(tool.input_schema["type"], "object");
        }
    }

    #[tokio::test]
    async fn test_unknown_tool_returns_error() {
        let registry = McpToolRegistry::new();
        let db = Arc::new(crate::db::Db::in_memory().unwrap());
        let ctx = ToolCallContext {
            session_id: "s1".into(),
            project_id: None,
            card_id: None,
            db,
            broadcaster: crate::ws::broadcaster::Broadcaster::new(),
            provider_registry: None,
            data_dir: None,
            folder_id: "f1".into(),
        };

        let result = registry
            .handle_tool_call("nonexistent", serde_json::json!({}), &ctx)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown tool"));
    }

    /// write_report / read_report must use the configured data dir from
    /// the tool-call context — not `~/.peckboard`. A report written via
    /// MCP with `--data-dir X` has to be readable by the HTTP reports API
    /// (which serves from X) and by read_report itself.
    #[tokio::test]
    async fn test_write_report_uses_ctx_data_dir() {
        use crate::db::models::{NewFolder, NewSession};

        let registry = McpToolRegistry::new();
        let db = Arc::new(crate::db::Db::in_memory().unwrap());
        let ts = chrono::Utc::now().to_rfc3339();
        let tmp = tempfile::tempdir().unwrap();

        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "Folder".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_session(NewSession {
            id: "s1".into(),
            name: "worker".into(),
            folder_id: "f1".into(),
            is_worker: true,
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();

        let ctx = ToolCallContext {
            session_id: "s1".into(),
            project_id: None,
            card_id: None,
            db,
            broadcaster: crate::ws::broadcaster::Broadcaster::new(),
            provider_registry: None,
            data_dir: Some(tmp.path().to_path_buf()),
            folder_id: "f1".into(),
        };

        let written = registry
            .handle_tool_call(
                "write_report",
                serde_json::json!({ "title": "My Findings", "body": "hello" }),
                &ctx,
            )
            .await
            .unwrap();
        let folder = written["folder"].as_str().unwrap().to_string();
        let file = written["file"].as_str().unwrap().to_string();

        // The file landed under the configured data dir, not ~/.peckboard.
        let on_disk = tmp.path().join("reports").join(&folder).join(&file);
        assert!(on_disk.exists(), "report not at {}", on_disk.display());

        // read_report resolves from the same root.
        let read = registry
            .handle_tool_call(
                "read_report",
                serde_json::json!({ "folder": folder, "file": file }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(read["content"].as_str(), Some("hello"));
    }

    #[tokio::test]
    async fn test_card_dependency_tools() {
        use crate::db::models::{NewFolder, NewProject};

        let registry = McpToolRegistry::new();
        let db = Arc::new(crate::db::Db::in_memory().unwrap());
        let ts = chrono::Utc::now().to_rfc3339();

        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "Folder".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_project(NewProject {
            id: "p1".into(),
            name: "Project".into(),
            context: "".into(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "task".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: true,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts.clone(),
        })
        .await
        .unwrap();

        let ctx = ToolCallContext {
            session_id: "s1".into(),
            project_id: Some("p1".into()),
            card_id: None,
            db: db.clone(),
            broadcaster: crate::ws::broadcaster::Broadcaster::new(),
            provider_registry: None,
            data_dir: None,
            folder_id: "f1".into(),
        };

        // Two prerequisites via create_card (no deps yet).
        let a = registry
            .handle_tool_call(
                "create_card",
                serde_json::json!({ "title": "A", "description": "root prereq" }),
                &ctx,
            )
            .await
            .unwrap();
        let a_id = a["card"]["id"].as_str().unwrap().to_string();

        let b = registry
            .handle_tool_call(
                "create_card",
                serde_json::json!({ "title": "B", "description": "mid", "depends_on": [a_id] }),
                &ctx,
            )
            .await
            .unwrap();
        let b_id = b["card"]["id"].as_str().unwrap().to_string();
        // create_card persists and echoes the dependency.
        assert_eq!(b["card"]["depends_on"], serde_json::json!([a_id]));

        // C depends on B (so transitively on A). Bogus + self ids are dropped.
        let c = registry
            .handle_tool_call(
                "create_card",
                serde_json::json!({
                    "title": "C",
                    "description": "leaf",
                    "depends_on": [b_id, "does-not-exist"],
                }),
                &ctx,
            )
            .await
            .unwrap();
        let c_id = c["card"]["id"].as_str().unwrap().to_string();
        assert_eq!(c["card"]["depends_on"], serde_json::json!([b_id]));

        // list_card_dependencies: direct deps only, none met yet.
        let direct = registry
            .handle_tool_call(
                "list_card_dependencies",
                serde_json::json!({ "card_id": c_id }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(direct["count"], 1);
        assert_eq!(direct["dependencies"][0]["id"], serde_json::json!(b_id));
        assert_eq!(direct["all_met"], serde_json::json!(false));

        // get_card_dependency_tree: C -> B -> A, two transitive deps, unmet.
        let tree = registry
            .handle_tool_call(
                "get_card_dependency_tree",
                serde_json::json!({ "card_id": c_id }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(tree["dependency_count"], 2);
        assert_eq!(tree["all_dependencies_met"], serde_json::json!(false));
        assert_eq!(tree["tree"]["id"], serde_json::json!(c_id));
        assert_eq!(tree["tree"]["depends_on"][0]["id"], serde_json::json!(b_id));
        assert_eq!(
            tree["tree"]["depends_on"][0]["depends_on"][0]["id"],
            serde_json::json!(a_id)
        );

        // Mark both prerequisites done; the tree now reports satisfied.
        registry
            .handle_tool_call(
                "move_card_to_done",
                serde_json::json!({ "card_id": a_id }),
                &ctx,
            )
            .await
            .unwrap();
        registry
            .handle_tool_call(
                "move_card_to_done",
                serde_json::json!({ "card_id": b_id }),
                &ctx,
            )
            .await
            .unwrap();
        let tree2 = registry
            .handle_tool_call(
                "get_card_dependency_tree",
                serde_json::json!({ "card_id": c_id }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(tree2["all_dependencies_met"], serde_json::json!(true));

        // Unknown card id is an error, not a panic.
        let err = registry
            .handle_tool_call(
                "list_card_dependencies",
                serde_json::json!({ "card_id": "nope" }),
                &ctx,
            )
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn test_list_workflows() {
        let registry = McpToolRegistry::new();
        let db = Arc::new(crate::db::Db::in_memory().unwrap());
        let ctx = ToolCallContext {
            session_id: "s1".into(),
            project_id: None,
            card_id: None,
            db,
            broadcaster: crate::ws::broadcaster::Broadcaster::new(),
            provider_registry: None,
            data_dir: None,
            folder_id: "f1".into(),
        };

        let result = registry
            .handle_tool_call("list_workflows", serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(result["workflows"].is_array());
    }
}

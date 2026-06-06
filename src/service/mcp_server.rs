use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::db::Db;
use crate::db::models::NewCard;

// ── MCP config file generation ────────────────────────────────────

/// Write a per-session MCP config JSON file so workers can discover
/// the peckboard MCP endpoint.
pub fn write_mcp_config(
    data_dir: &Path,
    session_id: &str,
    http_port: u16,
    token: &str,
) -> anyhow::Result<PathBuf> {
    let mcp_dir = data_dir.join("worker-mcp");
    std::fs::create_dir_all(&mcp_dir)?;
    let config_path = mcp_dir.join(format!("{session_id}.json"));

    let config = serde_json::json!({
        "mcpServers": {
            "peckboard": {
                "url": format!("http://127.0.0.1:{http_port}/api/internal/mcp"),
                "headers": {
                    "Authorization": format!("Bearer {token}")
                }
            }
        }
    });

    std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;
    Ok(config_path)
}

/// Remove a per-session MCP config file.
pub fn delete_mcp_config(data_dir: &Path, session_id: &str) {
    let config_path = data_dir
        .join("worker-mcp")
        .join(format!("{session_id}.json"));
    let _ = std::fs::remove_file(config_path);
}

// ── MCP bearer token registry ─────────────────────────────────────

/// Metadata associated with an issued MCP token.
pub struct McpTokenInfo {
    pub session_id: String,
    pub project_id: Option<String>,
}

/// A simple in-memory registry mapping token hashes to session metadata.
pub struct McpTokenRegistry {
    tokens: Mutex<HashMap<String, McpTokenInfo>>, // token_hash -> info
}

impl McpTokenRegistry {
    pub fn new() -> Self {
        McpTokenRegistry {
            tokens: Mutex::new(HashMap::new()),
        }
    }

    /// Issue a new bearer token for the given session/project.
    /// Returns the raw token (caller must pass it to the worker).
    pub async fn issue_token(
        &self,
        session_id: String,
        project_id: Option<String>,
    ) -> String {
        use rand::Rng;
        use sha2::Digest;

        let mut raw = [0u8; 24];
        rand::thread_rng().fill(&mut raw);
        let token = hex::encode(raw);

        let hash = hex::encode(sha2::Sha256::digest(token.as_bytes()));

        self.tokens.lock().await.insert(
            hash,
            McpTokenInfo {
                session_id,
                project_id,
            },
        );
        token
    }

    /// Look up a token by its SHA-256 hash.
    pub async fn lookup(&self, token: &str) -> Option<(String, Option<String>)> {
        use sha2::Digest;
        let hash = hex::encode(sha2::Sha256::digest(token.as_bytes()));
        let guard = self.tokens.lock().await;
        guard
            .get(&hash)
            .map(|info| (info.session_id.clone(), info.project_id.clone()))
    }

    /// Revoke all tokens belonging to a session.
    pub async fn revoke_by_session(&self, session_id: &str) {
        self.tokens
            .lock()
            .await
            .retain(|_, info| info.session_id != session_id);
    }
}

/// Context scoped from the MCP token — identifies what session/project/card
/// the tool call is operating within.
#[derive(Clone)]
pub struct ToolCallContext {
    pub session_id: String,
    pub project_id: Option<String>,
    pub card_id: Option<String>,
    pub db: Arc<Db>,
}

/// A single MCP tool definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Registry of MCP tools exposed to workers via stdio MCP server.
pub struct McpToolRegistry {
    tools: Vec<McpToolDef>,
}

impl McpToolRegistry {
    pub fn new() -> Self {
        let tools = vec![
            McpToolDef {
                name: "complete_step".into(),
                description: "Mark the current workflow step as complete and advance to the next step.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "handoff_context": {
                            "type": "string",
                            "description": "Context to pass to the next step's worker"
                        }
                    },
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "finish_card".into(),
                description: "Mark the card as finished (done). No further steps will run.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "summary": {
                            "type": "string",
                            "description": "Final summary of what was accomplished"
                        }
                    },
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "wont_do_card".into(),
                description: "Mark the card as won't-do. Stops all work on this card.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "reason": {
                            "type": "string",
                            "description": "Reason this card cannot or should not be done"
                        }
                    },
                    "required": ["reason"],
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "ask_user".into(),
                description: "Ask the user a question and block until they respond.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "The question to ask the user"
                        }
                    },
                    "required": ["question"],
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "create_card".into(),
                description: "Create a new card in the current project.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "Card title"
                        },
                        "description": {
                            "type": "string",
                            "description": "Card description / instructions"
                        },
                        "priority": {
                            "type": "integer",
                            "description": "Priority (lower = higher priority)"
                        },
                        "workflow": {
                            "type": "string",
                            "description": "Optional workflow override"
                        }
                    },
                    "required": ["title", "description"],
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "list_cards".into(),
                description: "List all cards in the current project.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "list_projects".into(),
                description: "List all projects.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "list_workflows".into(),
                description: "List available workflow definitions.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "write_report".into(),
                description: "Write a report or note to the event log for human review.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "Report title"
                        },
                        "body": {
                            "type": "string",
                            "description": "Report body (markdown)"
                        }
                    },
                    "required": ["title", "body"],
                    "additionalProperties": false
                }),
            },
        ];

        McpToolRegistry { tools }
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
            "list_cards" => self.handle_list_cards(ctx).await,
            "list_projects" => self.handle_list_projects(ctx).await,
            "list_workflows" => self.handle_list_workflows(ctx).await,
            "write_report" => self.handle_write_report(args, ctx).await,
            _ => anyhow::bail!("unknown tool: {tool_name}"),
        }
    }

    // ── Tool Handlers ───────────────────────────────────────────────

    async fn handle_complete_step(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let card_id = ctx
            .card_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("complete_step requires card context"))?;

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

    async fn handle_finish_card(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let card_id = ctx
            .card_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("finish_card requires card context"))?;

        let summary = args
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("");

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

    async fn handle_wont_do_card(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let card_id = ctx
            .card_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("wont_do_card requires card context"))?;

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

    async fn handle_ask_user(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let question = args
            .get("question")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("ask_user requires 'question' argument"))?;

        ctx.db
            .append_event(
                &ctx.session_id,
                "ask-user-requested",
                serde_json::json!({
                    "question": question,
                    "cardId": ctx.card_id,
                }),
            )
            .await?;

        Ok(serde_json::json!({
            "status": "ok",
            "message": "Question sent to user"
        }))
    }

    async fn handle_create_card(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let project_id = ctx
            .project_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("create_card requires project context"))?;

        let title = args
            .get("title")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("create_card requires 'title'"))?;

        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("create_card requires 'description'"))?;

        let priority = args
            .get("priority")
            .and_then(|v| v.as_i64())
            .unwrap_or(100) as i32;

        let workflow = args
            .get("workflow")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let now = chrono::Utc::now().to_rfc3339();
        let card = ctx
            .db
            .create_card(NewCard {
                id: uuid::Uuid::new_v4().to_string(),
                project_id: project_id.to_string(),
                title: title.to_string(),
                description: description.to_string(),
                step: "todo".to_string(),
                priority,
                workflow,
                model: None,
                effort: None,
                created_at: now.clone(),
                updated_at: now,
            })
            .await?;

        Ok(serde_json::json!({
            "status": "ok",
            "card": {
                "id": card.id,
                "title": card.title,
                "step": card.step,
                "priority": card.priority,
            }
        }))
    }

    async fn handle_list_cards(&self, ctx: &ToolCallContext) -> anyhow::Result<Value> {
        let project_id = ctx
            .project_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("list_cards requires project context"))?;

        let cards = ctx.db.list_cards_by_project(project_id).await?;

        let items: Vec<Value> = cards
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "title": c.title,
                    "step": c.step,
                    "priority": c.priority,
                    "blocked": c.blocked,
                })
            })
            .collect();

        Ok(serde_json::json!({ "cards": items }))
    }

    async fn handle_list_projects(&self, ctx: &ToolCallContext) -> anyhow::Result<Value> {
        let projects = ctx.db.list_projects().await?;

        let items: Vec<Value> = projects
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "name": p.name,
                    "status": p.status,
                    "workerCount": p.worker_count,
                })
            })
            .collect();

        Ok(serde_json::json!({ "projects": items }))
    }

    async fn handle_list_workflows(&self, _ctx: &ToolCallContext) -> anyhow::Result<Value> {
        // Workflows are defined conventionally; return the default set.
        Ok(serde_json::json!({
            "workflows": [
                {
                    "name": "default",
                    "steps": ["todo", "in-progress", "review", "done"]
                }
            ]
        }))
    }

    async fn handle_write_report(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let title = args
            .get("title")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("write_report requires 'title'"))?;

        let body = args
            .get("body")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("write_report requires 'body'"))?;

        // Write to disk: <dataDir>/reports/<date>/<sanitized-title>.md
        let now = chrono::Utc::now();
        let date_folder = now.format("%Y-%m-%d").to_string();
        let data_dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".peckboard");
        let reports_dir = data_dir.join("reports").join(&date_folder);
        std::fs::create_dir_all(&reports_dir)?;

        // Sanitize title for filename
        let sanitized: String = title
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' { c } else { '_' })
            .collect::<String>()
            .replace(' ', "-")
            .to_lowercase();
        let sanitized = if sanitized.is_empty() { "report".to_string() } else { sanitized };

        // Collision avoidance
        let mut filename = format!("{sanitized}.md");
        let mut path = reports_dir.join(&filename);
        let mut counter = 1;
        while path.exists() {
            filename = format!("{sanitized}-{counter}.md");
            path = reports_dir.join(&filename);
            counter += 1;
        }

        // Resolve project name for frontmatter
        let project_name = if let Some(ref pid) = ctx.project_id {
            ctx.db.get_project(pid).await.ok().flatten().map(|p| p.name)
        } else {
            None
        };

        // Build markdown with YAML frontmatter
        let mut content = format!("---\ntitle: \"{title}\"\ndate: \"{}\"\nsessionId: \"{}\"",
            now.to_rfc3339(), ctx.session_id);
        if let Some(ref pn) = project_name {
            content.push_str(&format!("\nprojectName: \"{pn}\""));
        }
        content.push_str("\n---\n\n");
        content.push_str(body);

        std::fs::write(&path, &content)?;
        tracing::info!(session_id = %ctx.session_id, path = %path.display(), "Report written to disk");

        // Append system event so it shows in the chat
        ctx.db
            .append_event(
                &ctx.session_id,
                "system",
                serde_json::json!({
                    "text": format!("Report written: {title}"),
                    "reportFolder": date_folder,
                    "reportFile": filename,
                }),
            )
            .await?;

        Ok(serde_json::json!({
            "status": "ok",
            "folder": date_folder,
            "file": filename,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_registry_has_all_tools() {
        let registry = McpToolRegistry::new();
        let names: Vec<&str> = registry.tool_definitions().iter().map(|t| t.name.as_str()).collect();

        assert!(names.contains(&"complete_step"));
        assert!(names.contains(&"finish_card"));
        assert!(names.contains(&"wont_do_card"));
        assert!(names.contains(&"ask_user"));
        assert!(names.contains(&"create_card"));
        assert!(names.contains(&"list_cards"));
        assert!(names.contains(&"list_projects"));
        assert!(names.contains(&"list_workflows"));
        assert!(names.contains(&"write_report"));
        assert_eq!(names.len(), 9);
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
        };

        let result = registry
            .handle_tool_call("nonexistent", serde_json::json!({}), &ctx)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown tool"));
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
        };

        let result = registry
            .handle_tool_call("list_workflows", serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(result["workflows"].is_array());
    }

    #[test]
    fn test_write_and_delete_mcp_config() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_mcp_config(tmp.path(), "sess-1", 3333, "tok123").unwrap();

        assert!(path.exists());
        let content: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            content["mcpServers"]["peckboard"]["url"],
            "http://127.0.0.1:3333/api/internal/mcp"
        );
        assert_eq!(
            content["mcpServers"]["peckboard"]["headers"]["Authorization"],
            "Bearer tok123"
        );

        delete_mcp_config(tmp.path(), "sess-1");
        assert!(!path.exists());
    }

    #[test]
    fn test_delete_mcp_config_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        // Should not panic even if file doesn't exist
        delete_mcp_config(tmp.path(), "nonexistent");
    }

    #[tokio::test]
    async fn test_token_registry_issue_and_lookup() {
        let registry = McpTokenRegistry::new();
        let token = registry
            .issue_token("sess-1".into(), Some("proj-a".into()))
            .await;

        assert_eq!(token.len(), 48); // 24 bytes => 48 hex chars

        let info = registry.lookup(&token).await;
        assert!(info.is_some());
        let (sid, pid) = info.unwrap();
        assert_eq!(sid, "sess-1");
        assert_eq!(pid.as_deref(), Some("proj-a"));

        // Unknown token returns None
        assert!(registry.lookup("bogus").await.is_none());
    }

    #[tokio::test]
    async fn test_token_registry_revoke_by_session() {
        let registry = McpTokenRegistry::new();
        let t1 = registry
            .issue_token("sess-1".into(), None)
            .await;
        let t2 = registry
            .issue_token("sess-1".into(), Some("proj-b".into()))
            .await;
        let t3 = registry
            .issue_token("sess-2".into(), None)
            .await;

        registry.revoke_by_session("sess-1").await;

        assert!(registry.lookup(&t1).await.is_none());
        assert!(registry.lookup(&t2).await.is_none());
        assert!(registry.lookup(&t3).await.is_some());
    }
}

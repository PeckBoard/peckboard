use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::Db;
use crate::db::models::NewCard;

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

        ctx.db
            .append_event(
                &ctx.session_id,
                "report",
                serde_json::json!({
                    "title": title,
                    "body": body,
                    "cardId": ctx.card_id,
                    "projectId": ctx.project_id,
                }),
            )
            .await?;

        Ok(serde_json::json!({
            "status": "ok",
            "message": "Report written"
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
}

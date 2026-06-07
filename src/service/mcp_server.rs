use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::db::Db;
use crate::db::models::{NewCard, NewProject, UpdateCard, UpdateProject};

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

    // Write a Node.js MCP stdio-to-HTTP proxy that properly implements the
    // MCP protocol including the initialize handshake. The bash curl approach
    // doesn't handle MCP protocol negotiations.
    let proxy_path = mcp_dir.join("mcp-proxy.mjs");
    // Always rewrite to pick up fixes
    let proxy_script = r#"#!/usr/bin/env node
import { createInterface } from 'readline';
import { request } from 'http';

const TOKEN = process.env.PECKBOARD_TOKEN;
const URL = process.env.PECKBOARD_MCP_URL;
const parsed = new globalThis.URL(URL);

const SERVER_INFO = {
  name: "peckboard",
  version: "1.0.0",
};
const CAPABILITIES = { tools: {} };

function send(obj) {
  process.stdout.write(JSON.stringify(obj) + '\n');
}

function httpPost(body) {
  return new Promise((resolve, reject) => {
    const req = request({
      hostname: parsed.hostname,
      port: parsed.port,
      path: parsed.pathname,
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        'Authorization': `Bearer ${TOKEN}`,
      },
    }, (res) => {
      let data = '';
      res.on('data', (c) => data += c);
      res.on('end', () => {
        try { resolve(JSON.parse(data)); }
        catch { resolve({ error: { code: -32000, message: data } }); }
      });
    });
    req.on('error', reject);
    req.write(body);
    req.end();
  });
}

const rl = createInterface({ input: process.stdin });
rl.on('line', async (line) => {
  if (!line.trim()) return;
  let msg;
  try { msg = JSON.parse(line); } catch { return; }

  // Handle MCP protocol messages locally
  if (msg.method === 'initialize') {
    send({
      jsonrpc: '2.0',
      id: msg.id,
      result: {
        protocolVersion: msg.params?.protocolVersion || '2024-11-05',
        serverInfo: SERVER_INFO,
        capabilities: CAPABILITIES,
      },
    });
    return;
  }

  if (msg.method === 'notifications/initialized') {
    // No response needed for notifications
    return;
  }

  // Forward everything else to the HTTP backend
  try {
    const result = await httpPost(line);
    send(result);
  } catch (e) {
    send({ jsonrpc: '2.0', id: msg.id, error: { code: -32000, message: String(e) } });
  }
});
"#;
    std::fs::write(&proxy_path, proxy_script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&proxy_path, std::fs::Permissions::from_mode(0o755))?;
    }

    let config = serde_json::json!({
        "mcpServers": {
            "peckboard": {
                "command": "node",
                "args": [proxy_path.to_string_lossy()],
                "env": {
                    "PECKBOARD_TOKEN": token,
                    "PECKBOARD_MCP_URL": format!("http://127.0.0.1:{http_port}/mcp")
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
    pub async fn issue_token(&self, session_id: String, project_id: Option<String>) -> String {
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
    pub broadcaster: Arc<crate::ws::broadcaster::Broadcaster>,
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
                description: "Ask the user one or more questions with multiple choice or fill-in-the-blank answers. The UI renders interactive controls. The tool returns when the user submits their answers.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "questions": {
                            "type": "array",
                            "description": "Array of questions to ask",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "question": { "type": "string", "description": "The question text" },
                                    "header": { "type": "string", "description": "Category label (e.g. Setup, Input, Configuration)" },
                                    "multiSelect": { "type": "boolean", "description": "true for checkboxes (multi), false for radio buttons (single). Default false." },
                                    "options": {
                                        "type": "array",
                                        "description": "If provided, renders as multiple choice. Omit for free-form text input.",
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "label": { "type": "string", "description": "Option text" },
                                                "description": { "type": "string", "description": "Help text below the label" }
                                            },
                                            "required": ["label", "description"]
                                        }
                                    }
                                },
                                "required": ["question", "header"]
                            }
                        }
                    },
                    "required": ["questions"],
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "create_card".into(),
                description: "Create a new card in a project. Uses current project context if available, or pass project_id explicitly.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "project_id": {
                            "type": "string",
                            "description": "Project ID (optional if in a worker session with project context)"
                        },
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
                        },
                        "model": {
                            "type": "string",
                            "description": "Optional model override (e.g. claude-opus-4-8)"
                        },
                        "effort": {
                            "type": "string",
                            "description": "Optional effort level (low, medium, high, xhigh, max)"
                        }
                    },
                    "required": ["title", "description"],
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "list_cards".into(),
                description: "List all cards in a project. Uses current project context if available, or pass project_id explicitly.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "project_id": { "type": "string", "description": "Project ID (optional if in a worker session with project context)" }
                    },
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
            McpToolDef {
                name: "attach_report_file".into(),
                description: "Attach a file to a report folder. Accepts base64-encoded data with allowlisted extensions and a 10MB size cap.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "folder": {
                            "type": "string",
                            "description": "Report folder name (e.g. date string)"
                        },
                        "file": {
                            "type": "string",
                            "description": "File base name (without extension)"
                        },
                        "data": {
                            "type": "string",
                            "description": "Base64-encoded file content"
                        },
                        "extension": {
                            "type": "string",
                            "description": "File extension (e.g. png, pdf, csv, json, txt, md, html, svg, jpg, jpeg, gif, webp)"
                        }
                    },
                    "required": ["folder", "file", "data", "extension"],
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "update_card".into(),
                description: "Update fields on an existing card.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "card_id": {
                            "type": "string",
                            "description": "ID of the card to update"
                        },
                        "title": {
                            "type": "string",
                            "description": "New card title"
                        },
                        "description": {
                            "type": "string",
                            "description": "New card description"
                        },
                        "priority": {
                            "type": "integer",
                            "description": "New priority value"
                        },
                        "step": {
                            "type": "string",
                            "description": "New workflow step"
                        },
                        "blocked": {
                            "type": "boolean",
                            "description": "Whether the card is blocked"
                        },
                        "block_reason": {
                            "type": "string",
                            "description": "Reason the card is blocked"
                        }
                    },
                    "required": ["card_id"],
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "update_project".into(),
                description: "Update fields on an existing project.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "project_id": {
                            "type": "string",
                            "description": "ID of the project to update"
                        },
                        "name": {
                            "type": "string",
                            "description": "New project name"
                        },
                        "context": {
                            "type": "string",
                            "description": "New project context"
                        },
                        "worker_count": {
                            "type": "integer",
                            "description": "New worker count"
                        },
                        "status": {
                            "type": "string",
                            "description": "New project status"
                        }
                    },
                    "required": ["project_id"],
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "create_project".into(),
                description: "Create a new project in a folder.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Project name"
                        },
                        "folder_id": {
                            "type": "string",
                            "description": "Folder ID to create the project in"
                        },
                        "context": {
                            "type": "string",
                            "description": "Project context / instructions"
                        },
                        "worker_count": {
                            "type": "integer",
                            "description": "Number of concurrent workers (default 1)"
                        }
                    },
                    "required": ["name", "folder_id"],
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "pause_project".into(),
                description: "Pause a project, preventing new work from being scheduled.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "project_id": {
                            "type": "string",
                            "description": "ID of the project to pause"
                        }
                    },
                    "required": ["project_id"],
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "resume_project".into(),
                description: "Resume a paused project, allowing work to be scheduled again.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "project_id": {
                            "type": "string",
                            "description": "ID of the project to resume"
                        }
                    },
                    "required": ["project_id"],
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "delete_card".into(),
                description: "Delete a card permanently.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "card_id": {
                            "type": "string",
                            "description": "ID of the card to delete"
                        }
                    },
                    "required": ["card_id"],
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "move_card_to_done".into(),
                description: "Move a card to the done step.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "card_id": {
                            "type": "string",
                            "description": "ID of the card to mark as done"
                        }
                    },
                    "required": ["card_id"],
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "move_card_to_wont_do".into(),
                description: "Move a card to the won't-do step, optionally with a reason.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "card_id": {
                            "type": "string",
                            "description": "ID of the card to mark as won't-do"
                        },
                        "reason": {
                            "type": "string",
                            "description": "Reason the card won't be done"
                        }
                    },
                    "required": ["card_id"],
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "notify_workers".into(),
                description: "Broadcast a message to all other running workers in the same project. Use this to inform other workers about file changes, shared state updates, or coordination needs. Other workers will receive your message before their next action.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "message": {
                            "type": "string",
                            "description": "The message to broadcast (e.g. 'Modified src/auth/mod.rs — added JWT validation middleware')"
                        },
                        "files_changed": {
                            "type": "array",
                            "description": "List of file paths that were modified, created, or deleted",
                            "items": { "type": "string" }
                        }
                    },
                    "required": ["message"],
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "fetch_url".into(),
                description: "Fetch a URL from peckboard's server (bypasses bot protection that blocks the CLI's WebFetch). Use this when WebFetch returns 403 or is blocked. Returns the page text content.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "The URL to fetch"
                        },
                        "max_length": {
                            "type": "integer",
                            "description": "Max response length in chars (default 10000)"
                        }
                    },
                    "required": ["url"],
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "share_finding".into(),
                description: "Share a finding, discovery, or insight with all other running workers. This can be anything valuable: research results, data patterns, bugs, architectural decisions, experimental observations, domain knowledge, constraints, or any information that may help other workers. Broadcasts a summary; workers can retrieve full detail and ask follow-up questions.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "summary": { "type": "string", "description": "Brief summary of the finding (shown to all workers)" },
                        "detail": { "type": "string", "description": "Full detail (available on request via get_finding_details)" },
                        "tags": { "type": "array", "items": { "type": "string" }, "description": "Optional tags for categorization" }
                    },
                    "required": ["summary", "detail"],
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "get_finding_details".into(),
                description: "Retrieve the full detail of a finding shared by another worker.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "finding_id": { "type": "string", "description": "The finding event ID" }
                    },
                    "required": ["finding_id"],
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "send_worker_message".into(),
                description: "Send a direct message to another worker session. The message is queued and delivered on their next turn. Useful for asking follow-up questions about a finding.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "target_session_id": { "type": "string", "description": "The session ID of the worker to message" },
                        "message": { "type": "string", "description": "The message to send" }
                    },
                    "required": ["target_session_id", "message"],
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "list_project_reports".into(),
                description: "List all reports written by workers in the same project. Returns report titles, dates, and paths so you can read them.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "read_report".into(),
                description: "Read the full content of a report by its folder and file path.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "folder": { "type": "string", "description": "Report folder (e.g. 2026-06-07)" },
                        "file": { "type": "string", "description": "Report filename (e.g. my-report.md)" }
                    },
                    "required": ["folder", "file"],
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "read_worker_session".into(),
                description: "Read the event history of another worker session in the same project. Use to understand what another worker did, see their tool calls, and review their work.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "session_id": { "type": "string", "description": "The worker session ID to read" },
                        "last_n": { "type": "integer", "description": "Number of recent events to return (default 50, max 200)" }
                    },
                    "required": ["session_id"],
                    "additionalProperties": false
                }),
            },
            McpToolDef {
                name: "list_worker_sessions".into(),
                description: "List all worker sessions in the same project with their card titles and status. Use to find sessions you can read or message.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
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
            "list_cards" => self.handle_list_cards(args, ctx).await,
            "list_projects" => self.handle_list_projects(ctx).await,
            "list_workflows" => self.handle_list_workflows(ctx).await,
            "write_report" => self.handle_write_report(args, ctx).await,
            "attach_report_file" => self.handle_attach_report_file(args, ctx).await,
            "update_card" => self.handle_update_card(args, ctx).await,
            "update_project" => self.handle_update_project(args, ctx).await,
            "create_project" => self.handle_create_project(args, ctx).await,
            "pause_project" => self.handle_pause_project(args, ctx).await,
            "resume_project" => self.handle_resume_project(args, ctx).await,
            "delete_card" => self.handle_delete_card(args, ctx).await,
            "move_card_to_done" => self.handle_move_card_to_done(args, ctx).await,
            "move_card_to_wont_do" => self.handle_move_card_to_wont_do(args, ctx).await,
            "notify_workers" => self.handle_notify_workers(args, ctx).await,
            "fetch_url" => self.handle_fetch_url(args, ctx).await,
            "list_project_reports" => self.handle_list_project_reports(ctx).await,
            "read_report" => self.handle_read_report(args, ctx).await,
            "read_worker_session" => self.handle_read_worker_session(args, ctx).await,
            "list_worker_sessions" => self.handle_list_worker_sessions(ctx).await,
            "share_finding" => self.handle_share_finding(args, ctx).await,
            "get_finding_details" => self.handle_get_finding_details(args, ctx).await,
            "send_worker_message" => self.handle_send_worker_message(args, ctx).await,
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

    async fn handle_finish_card(
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

    async fn handle_wont_do_card(
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

    async fn handle_ask_user(&self, args: Value, ctx: &ToolCallContext) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, "MCP tool: ask_user");

        // Support both old format (single "question" string) and new format ("questions" array)
        let questions_data =
            if let Some(questions) = args.get("questions").and_then(|v| v.as_array()) {
                // New structured format
                let mut normalized = Vec::new();
                for q in questions {
                    let question_text = q.get("question").and_then(|v| v.as_str()).unwrap_or("");
                    let header = q.get("header").and_then(|v| v.as_str());
                    let multi_select = q
                        .get("multiSelect")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);

                    let mut options = Vec::new();
                    let mut option_objects = Vec::new();
                    if let Some(opts) = q.get("options").and_then(|v| v.as_array()) {
                        for opt in opts {
                            if let Some(label) = opt.get("label").and_then(|v| v.as_str()) {
                                options.push(serde_json::Value::String(label.to_string()));
                                option_objects.push(opt.clone());
                            }
                        }
                    }

                    let mut entry = serde_json::json!({
                        "question": question_text,
                        "multiSelect": multi_select,
                    });
                    if let Some(h) = header {
                        entry["header"] = serde_json::Value::String(h.to_string());
                    }
                    if !options.is_empty() {
                        entry["options"] = serde_json::Value::Array(options);
                        entry["optionObjects"] = serde_json::Value::Array(option_objects);
                    }
                    normalized.push(entry);
                }
                serde_json::Value::Array(normalized)
            } else if let Some(question) = args.get("question").and_then(|v| v.as_str()) {
                // Old simple format — single text question
                serde_json::json!([{ "question": question, "header": "Question" }])
            } else {
                return Err(anyhow::anyhow!(
                    "ask_user requires 'questions' array or 'question' string"
                ));
            };

        // Look up card and project context for worker questions
        let mut card_title = None;
        let mut card_description = None;
        let mut project_name = None;
        let mut project_id_val = None;
        // Get card_id from context or fall back to session lookup
        let resolved_card_id = if ctx.card_id.is_some() {
            ctx.card_id.clone()
        } else {
            ctx.db
                .get_session(&ctx.session_id)
                .await
                .ok()
                .flatten()
                .and_then(|s| s.card_id)
        };
        if let Some(ref card_id) = resolved_card_id {
            if let Ok(Some(card)) = ctx.db.get_card(card_id).await {
                card_title = Some(card.title);
                card_description = Some(card.description);
            }
        }
        // Get project_id from context or fall back to session lookup
        let resolved_project_id = if ctx.project_id.is_some() {
            ctx.project_id.clone()
        } else {
            ctx.db
                .get_session(&ctx.session_id)
                .await
                .ok()
                .flatten()
                .and_then(|s| s.project_id)
        };
        if let Some(ref pid) = resolved_project_id {
            project_id_val = Some(pid.clone());
            if let Ok(Some(project)) = ctx.db.get_project(pid).await {
                project_name = Some(project.name);
            }
        }

        // Check if this is a worker session
        let is_worker = resolved_card_id.is_some();

        let mut event_data = serde_json::json!({
            "questions": questions_data,
            "cardId": ctx.card_id,
            "sessionId": ctx.session_id,
            "source": "mcp",
            "isWorker": is_worker,
        });
        if let Some(ref title) = card_title {
            event_data["cardTitle"] = serde_json::Value::String(title.clone());
        }
        if let Some(ref desc) = card_description {
            event_data["cardDescription"] = serde_json::Value::String(desc.clone());
        }
        if let Some(ref name) = project_name {
            event_data["projectName"] = serde_json::Value::String(name.clone());
        }
        if let Some(ref pid) = project_id_val {
            event_data["projectId"] = serde_json::Value::String(pid.clone());
        }

        // Emit as a "question" event so the frontend renders the question card UI
        let event = ctx
            .db
            .append_event(&ctx.session_id, "question", event_data.clone())
            .await?;

        // Broadcast as session event
        ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "event".into(),
            session_id: ctx.session_id.clone(),
            data: serde_json::json!({
                "id": event.id,
                "seq": event.seq,
                "ts": event.ts,
                "kind": "question",
                "data": event_data,
            }),
        });

        // Broadcast as global worker-question event so the project page updates live
        if is_worker {
            if let Some(ref pid) = project_id_val {
                ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
                    event_type: "worker-question".into(),
                    session_id: pid.clone(),
                    data: serde_json::json!({
                        "eventId": event.id,
                        "sessionId": ctx.session_id,
                        "projectId": pid,
                        "cardTitle": card_title,
                    }),
                });
            }
        }

        // Also emit the ask-user-requested event for worker intent derivation
        ctx.db
            .append_event(
                &ctx.session_id,
                "ask-user-requested",
                serde_json::json!({
                    "questionEventId": event.id,
                    "cardId": ctx.card_id,
                }),
            )
            .await?;

        Ok(serde_json::json!({
            "status": "ok",
            "message": "Question sent to user. They will see interactive controls to answer."
        }))
    }

    async fn handle_create_card(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        // Accept project_id from args (for chat sessions) or context (for workers)
        let project_id = args.get("project_id").and_then(|v| v.as_str()).map(|s| s.to_string())
            .or_else(|| ctx.project_id.clone())
            .or_else(|| {
                // Fallback: resolve from session
                // (can't await here, handled below)
                None
            })
            .ok_or_else(|| anyhow::anyhow!("create_card requires 'project_id' parameter or project context"))?;
        let project_id = project_id.as_str();

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

        let workflow = args.get("workflow").and_then(|v| v.as_str()).map(|s| s.to_string());
        let model = args.get("model").and_then(|v| v.as_str()).map(|s| s.to_string());
        let effort = args.get("effort").and_then(|v| v.as_str()).map(|s| s.to_string());

        let now = chrono::Utc::now().to_rfc3339();
        let card = ctx
            .db
            .create_card(NewCard {
                id: uuid::Uuid::new_v4().to_string(),
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

        // Broadcast for live kanban
        ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "card-update".into(),
            session_id: project_id.to_string(),
            data: serde_json::json!({ "card": card }),
        });

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

    async fn handle_list_cards(&self, args: Value, ctx: &ToolCallContext) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, "MCP tool: list_cards");

        let project_id = args.get("project_id").and_then(|v| v.as_str()).map(|s| s.to_string())
            .or_else(|| ctx.project_id.clone())
            .ok_or_else(|| anyhow::anyhow!("list_cards requires 'project_id' parameter or project context"))?;

        let cards = ctx.db.list_cards_by_project(&project_id).await?;

        let items: Vec<Value> = cards
            .iter()
            .map(|c| {
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
                })
            })
            .collect();

        Ok(serde_json::json!({ "cards": items, "count": items.len(), "project_id": project_id }))
    }

    async fn handle_list_projects(&self, ctx: &ToolCallContext) -> anyhow::Result<Value> {
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
                    "default_workflow": p.default_workflow,
                    "model": p.model,
                    "effort": p.effort,
                })
            })
            .collect();

        Ok(serde_json::json!({ "projects": items, "count": items.len() }))
    }

    async fn handle_list_workflows(&self, ctx: &ToolCallContext) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, "MCP tool: list_workflows");

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
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>()
            .replace(' ', "-")
            .to_lowercase();
        let sanitized = if sanitized.is_empty() {
            "report".to_string()
        } else {
            sanitized
        };

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

        // Resolve card_id
        let resolved_card_id = if ctx.card_id.is_some() {
            ctx.card_id.clone()
        } else {
            ctx.db.get_session(&ctx.session_id).await.ok().flatten().and_then(|s| s.card_id)
        };

        // Build markdown with YAML frontmatter
        let mut content = format!(
            "---\ntitle: \"{title}\"\ndate: \"{}\"\nsessionId: \"{}\"",
            now.to_rfc3339(),
            ctx.session_id
        );
        if let Some(ref pn) = project_name {
            content.push_str(&format!("\nprojectName: \"{pn}\""));
        }
        if let Some(ref cid) = resolved_card_id {
            content.push_str(&format!("\ncardId: \"{cid}\""));
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
                    "cardId": resolved_card_id,
                }),
            )
            .await?;

        // Broadcast card update so UI refreshes reports
        if let Some(ref cid) = resolved_card_id {
            if let Ok(Some(card)) = ctx.db.get_card(cid).await {
                ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
                    event_type: "card-update".into(),
                    session_id: card.project_id.clone(),
                    data: serde_json::json!({ "card": card }),
                });
            }
        }

        Ok(serde_json::json!({
            "status": "ok",
            "folder": date_folder,
            "file": filename,
            "cardId": resolved_card_id,
        }))
    }

    async fn handle_attach_report_file(
        &self,
        args: Value,
        _ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        const ALLOWED_EXTENSIONS: &[&str] = &[
            "png", "pdf", "csv", "json", "txt", "md", "html", "svg", "jpg", "jpeg", "gif", "webp",
        ];
        const MAX_DECODED_SIZE: usize = 10 * 1024 * 1024; // 10 MB

        let folder = args
            .get("folder")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("attach_report_file requires 'folder'"))?;

        let file = args
            .get("file")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("attach_report_file requires 'file'"))?;

        let data_b64 = args
            .get("data")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("attach_report_file requires 'data'"))?;

        let extension = args
            .get("extension")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("attach_report_file requires 'extension'"))?;

        // Validate extension
        let ext_lower = extension.to_lowercase();
        if !ALLOWED_EXTENSIONS.contains(&ext_lower.as_str()) {
            anyhow::bail!(
                "extension '{extension}' not allowed; allowed: {}",
                ALLOWED_EXTENSIONS.join(", ")
            );
        }

        // Sanitize folder and file names to prevent path traversal
        let sanitize = |s: &str| -> String {
            s.chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect()
        };
        let safe_folder = sanitize(folder);
        let safe_file = sanitize(file);

        if safe_folder.is_empty() || safe_file.is_empty() {
            anyhow::bail!("folder and file names must not be empty after sanitization");
        }

        // Decode base64
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(data_b64)
            .map_err(|e| anyhow::anyhow!("invalid base64 data: {e}"))?;

        if decoded.len() > MAX_DECODED_SIZE {
            anyhow::bail!("file too large: {} bytes exceeds 10MB limit", decoded.len());
        }

        // Write to <dataDir>/reports/<folder>/<file>.<ext>
        let data_dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".peckboard");
        let reports_dir = data_dir.join("reports").join(&safe_folder);
        std::fs::create_dir_all(&reports_dir)?;

        let filename = format!("{safe_file}.{ext_lower}");
        let path = reports_dir.join(&filename);
        std::fs::write(&path, &decoded)?;

        tracing::info!(path = %path.display(), size = decoded.len(), "Report file attached");

        Ok(serde_json::json!({
            "status": "ok",
            "folder": safe_folder,
            "file": filename,
            "size": decoded.len(),
        }))
    }

    async fn handle_update_card(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let card_id = args
            .get("card_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("update_card requires 'card_id'"))?;

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

    async fn handle_update_project(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let project_id = args
            .get("project_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("update_project requires 'project_id'"))?;

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

    async fn handle_create_project(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, "MCP tool: create_project");

        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("create_project requires 'name'"))?;

        let folder_id = args
            .get("folder_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("create_project requires 'folder_id'"))?;

        let context = args
            .get("context")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let worker_count = args
            .get("worker_count")
            .and_then(|v| v.as_i64())
            .unwrap_or(1) as i32;

        let now = chrono::Utc::now().to_rfc3339();
        let project = ctx
            .db
            .create_project(NewProject {
                id: uuid::Uuid::new_v4().to_string(),
                name: name.to_string(),
                context,
                folder_id: folder_id.to_string(),
                worker_count,
                status: "active".to_string(),
                default_workflow: None,
                model: None,
                effort: None,
                parallel_instructions: false,
                auto_notify_changes: true,
                worker_communication: true,
                created_at: now.clone(),
                last_accessed_at: now,
            })
            .await?;

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

    async fn handle_pause_project(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let project_id = args
            .get("project_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("pause_project requires 'project_id'"))?;

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

        Ok(serde_json::json!({
            "status": "ok",
            "project": {
                "id": project.id,
                "name": project.name,
                "status": project.status,
            }
        }))
    }

    async fn handle_resume_project(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let project_id = args
            .get("project_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("resume_project requires 'project_id'"))?;

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

    async fn handle_delete_card(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let card_id = args
            .get("card_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("delete_card requires 'card_id'"))?;

        tracing::info!(session_id = %ctx.session_id, card_id = %card_id, "MCP tool: delete_card");

        // Get card before deletion for cascade cleanup
        let card = ctx
            .db
            .get_card(card_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("card not found: {card_id}"))?;
        let project_id = card.project_id.clone();

        // Cascade: clear card refs, delete queued messages, events, sessions
        let mut session_ids = Vec::new();
        if let Some(ref sid) = card.worker_session_id {
            session_ids.push(sid.clone());
        }
        if let Some(ref sid) = card.last_worker_session_id {
            session_ids.push(sid.clone());
        }
        session_ids.sort();
        session_ids.dedup();

        // Clear card's FK references to sessions first
        let _ = ctx.db.update_card(card_id, crate::db::models::UpdateCard {
            worker_session_id: Some(None),
            last_worker_session_id: Some(None),
            ..Default::default()
        }).await;

        // Now safe to delete sessions and their data
        for sid in &session_ids {
            let _ = ctx.db.delete_queued_message(sid).await;
            let _ = ctx.db.delete_events_by_session(sid).await;
            let _ = ctx.db.delete_session(sid).await;
        }

        let deleted = ctx.db.delete_card(card_id).await?;
        if !deleted {
            anyhow::bail!("card not found: {card_id}");
        }

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

    async fn handle_move_card_to_done(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let card_id = args
            .get("card_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("move_card_to_done requires 'card_id'"))?;

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

    async fn handle_move_card_to_wont_do(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let card_id = args
            .get("card_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("move_card_to_wont_do requires 'card_id'"))?;

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

    async fn handle_notify_workers(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let project_id = ctx
            .project_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("notify_workers requires project context"))?;

        tracing::info!(session_id = %ctx.session_id, project_id = %project_id, "MCP tool: notify_workers");

        let message = args
            .get("message")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("notify_workers requires 'message'"))?;

        let files_changed: Vec<String> = args
            .get("files_changed")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        // Get the card title for context
        let sender_card_title = if let Some(ref card_id) = ctx.card_id {
            ctx.db
                .get_card(card_id)
                .await
                .ok()
                .flatten()
                .map(|c| c.title)
        } else {
            None
        };

        // Find all other worker sessions in the same project
        let worker_sessions = ctx.db.list_worker_sessions_by_project(project_id).await?;

        let mut notified_count = 0u32;

        for session in &worker_sessions {
            // Skip the sender's own session
            if session.id == ctx.session_id {
                continue;
            }
            // Only notify sessions with active cards (not in terminal states)
            if let Some(ref card_id) = session.card_id {
                if let Ok(Some(card)) = ctx.db.get_card(card_id).await {
                    if card.step == "done" || card.step == "wont_do" {
                        continue;
                    }
                }
            }

            // Build the notification message
            let mut notification = format!(
                "[Worker Cross-Communication] From worker on card \"{}\":\n\n{}",
                sender_card_title.as_deref().unwrap_or("unknown"),
                message
            );
            if !files_changed.is_empty() {
                notification.push_str("\n\nFiles changed:\n");
                for f in &files_changed {
                    notification.push_str(&format!("  - {f}\n"));
                }
            }

            // Deliver immediately to running worker + persist
            self.deliver_to_worker(ctx, &session.id, &notification)
                .await;

            // Broadcast so the frontend sees it in real-time
            ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
                event_type: "event".into(),
                session_id: session.id.clone(),
                data: serde_json::json!({
                    "kind": "system",
                    "data": {
                        "text": notification,
                        "source": "worker-notification",
                    },
                }),
            });

            notified_count += 1;
        }

        Ok(serde_json::json!({
            "status": "ok",
            "workers_notified": notified_count,
            "message": format!("Notified {} other worker(s) in this project", notified_count)
        }))
    }

    async fn handle_fetch_url(&self, args: Value, ctx: &ToolCallContext) -> anyhow::Result<Value> {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("fetch_url requires 'url'"))?;

        let max_length = args
            .get("max_length")
            .and_then(|v| v.as_u64())
            .unwrap_or(10000) as usize;

        tracing::info!(session_id = %ctx.session_id, url = %url, "MCP tool: fetch_url");

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
            .build()?;

        let response = client.get(url).send().await?;
        let status = response.status().as_u16();

        if !response.status().is_success() {
            return Ok(serde_json::json!({
                "status": "error",
                "http_status": status,
                "message": format!("HTTP {status}")
            }));
        }

        let body = response.text().await?;

        // Strip HTML tags for a rough text extraction
        let text = if body.contains('<') && body.contains('>') {
            // Simple HTML tag stripping
            let re = regex::Regex::new(
                r"<script[^>]*>[\s\S]*?</script>|<style[^>]*>[\s\S]*?</style>|<[^>]+>",
            )
            .unwrap();
            let stripped = re.replace_all(&body, " ");
            // Collapse whitespace
            let ws_re = regex::Regex::new(r"\s+").unwrap();
            ws_re.replace_all(&stripped, " ").trim().to_string()
        } else {
            body
        };

        let truncated = if text.len() > max_length {
            format!(
                "{}... (truncated at {} chars)",
                &text[..max_length],
                max_length
            )
        } else {
            text
        };

        Ok(serde_json::json!({
            "status": "ok",
            "http_status": status,
            "content": truncated,
            "length": truncated.len(),
        }))
    }

    async fn handle_share_finding(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let summary = args
            .get("summary")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("share_finding requires 'summary'"))?;
        let detail = args
            .get("detail")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("share_finding requires 'detail'"))?;
        let tags = args
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        tracing::info!(session_id = %ctx.session_id, "MCP tool: share_finding");

        // Check worker_communication is enabled
        let project_id = self.resolve_project_id(ctx).await;
        if let Some(ref pid) = project_id {
            if let Ok(Some(project)) = ctx.db.get_project(pid).await {
                if !project.worker_communication {
                    return Ok(serde_json::json!({
                        "status": "disabled",
                        "message": "Inter-worker communication is disabled for this project"
                    }));
                }
            }
        }

        let card_title = self.resolve_card_title(ctx).await;

        // Store the finding as an event
        let finding_data = serde_json::json!({
            "summary": summary,
            "detail": detail,
            "tags": tags,
            "fromSessionId": ctx.session_id,
            "fromCardTitle": card_title,
            "projectId": project_id,
        });
        let event = ctx
            .db
            .append_event(&ctx.session_id, "worker-finding", finding_data)
            .await?;

        // Broadcast summary to other workers
        if let Some(ref pid) = project_id {
            if let Ok(workers) = ctx.db.list_worker_sessions_by_project(pid).await {
                let tags_str = if tags.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", tags.join(", "))
                };
                let msg = format!(
                    "[Shared finding from worker on \"{}\"]{}\n\n{}\n\n\
                     — Finding ID: {} (call mcp__peckboard__get_finding_details to see full detail)\n\
                     — From session: {} (use mcp__peckboard__send_worker_message to ask follow-up questions)\n\n\
                     If this finding is relevant to your work, review the detail. \
                     If you have questions, send a message to the worker above.",
                    card_title.as_deref().unwrap_or("unknown"),
                    tags_str,
                    summary,
                    event.id,
                    ctx.session_id
                );
                for ws in &workers {
                    if ws.id == ctx.session_id {
                        continue;
                    }
                    if let Some(ref cid) = ws.card_id {
                        if let Ok(Some(c)) = ctx.db.get_card(cid).await {
                            if c.step == "done" || c.step == "wont_do" {
                                continue;
                            }
                        }
                    }
                    self.deliver_to_worker(ctx, &ws.id, &msg).await;
                }
            }
        }

        Ok(serde_json::json!({
            "status": "ok",
            "finding_id": event.id,
            "message": "Finding shared with other workers"
        }))
    }

    async fn handle_get_finding_details(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let finding_id = args
            .get("finding_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("get_finding_details requires 'finding_id'"))?;

        tracing::info!(session_id = %ctx.session_id, finding_id = %finding_id, "MCP tool: get_finding_details");

        let event = ctx
            .db
            .get_event(finding_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("finding not found: {finding_id}"))?;

        if event.kind != "worker-finding" {
            anyhow::bail!("event {finding_id} is not a finding");
        }

        let data: Value = serde_json::from_str(&event.data)?;
        Ok(serde_json::json!({
            "status": "ok",
            "finding_id": finding_id,
            "summary": data.get("summary"),
            "detail": data.get("detail"),
            "tags": data.get("tags"),
            "from_session_id": data.get("fromSessionId"),
            "from_card_title": data.get("fromCardTitle"),
        }))
    }

    async fn handle_send_worker_message(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let target_session_id = args
            .get("target_session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("send_worker_message requires 'target_session_id'"))?;
        let message = args
            .get("message")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("send_worker_message requires 'message'"))?;

        tracing::info!(session_id = %ctx.session_id, target = %target_session_id, "MCP tool: send_worker_message");

        // Check worker_communication is enabled
        let project_id = self.resolve_project_id(ctx).await;
        if let Some(ref pid) = project_id {
            if let Ok(Some(project)) = ctx.db.get_project(pid).await {
                if !project.worker_communication {
                    return Ok(serde_json::json!({
                        "status": "disabled",
                        "message": "Inter-worker communication is disabled for this project"
                    }));
                }
            }
        }

        let card_title = self.resolve_card_title(ctx).await;

        // Verify target is a valid worker session
        let target = ctx
            .db
            .get_session(target_session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("target session not found"))?;
        if !target.is_worker {
            anyhow::bail!("target session is not a worker");
        }

        let msg = format!(
            "[Worker message from \"{}\"] (NOT from the user — this is from another worker)\n\
             From session: {}\n\n{}\n\n\
             To reply, call mcp__peckboard__send_worker_message with target_session_id: \"{}\"",
            card_title.as_deref().unwrap_or("unknown worker"),
            ctx.session_id,
            message,
            ctx.session_id
        );

        // Deliver immediately to running worker + persist
        self.deliver_to_worker(ctx, target_session_id, &msg).await;

        Ok(serde_json::json!({
            "status": "ok",
            "message": format!("Message sent to worker session {}", target_session_id)
        }))
    }

    /// Helper: deliver a message to a worker session immediately.
    /// Appends as a user event (for persistence) AND broadcasts for
    /// immediate stdin delivery to a running agent.
    async fn handle_list_project_reports(&self, ctx: &ToolCallContext) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, "MCP tool: list_project_reports");

        let project_id = self.resolve_project_id(ctx).await;
        let project_name = if let Some(ref pid) = project_id {
            ctx.db.get_project(pid).await.ok().flatten().map(|p| p.name)
        } else {
            None
        };

        // Scan reports directory for reports matching this project
        let data_dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".peckboard");
        let reports_dir = data_dir.join("reports");

        let mut reports = Vec::new();
        if reports_dir.exists() {
            if let Ok(folders) = std::fs::read_dir(&reports_dir) {
                for folder_entry in folders.flatten() {
                    let folder_name = folder_entry.file_name().to_string_lossy().to_string();
                    if let Ok(files) = std::fs::read_dir(folder_entry.path()) {
                        for file_entry in files.flatten() {
                            let file_name = file_entry.file_name().to_string_lossy().to_string();
                            if !file_name.ends_with(".md") {
                                continue;
                            }

                            // Read frontmatter to check project match
                            if let Ok(content) = std::fs::read_to_string(file_entry.path()) {
                                let mut title = file_name.clone();
                                let mut session_id = None;
                                let mut report_project = None;

                                if content.starts_with("---") {
                                    if let Some(fm) = content.splitn(3, "---").nth(1) {
                                        for line in fm.lines() {
                                            if let Some(v) = line.strip_prefix("title: ") {
                                                title = v.trim_matches('"').to_string();
                                            }
                                            if let Some(v) = line.strip_prefix("sessionId: ") {
                                                session_id = Some(v.trim_matches('"').to_string());
                                            }
                                            if let Some(v) = line.strip_prefix("projectName: ") {
                                                report_project =
                                                    Some(v.trim_matches('"').to_string());
                                            }
                                        }
                                    }
                                }

                                // Include if project matches or no filter
                                let matches = match (&project_name, &report_project) {
                                    (Some(pn), Some(rp)) => pn == rp,
                                    (None, _) => true,
                                    _ => true,
                                };
                                if matches {
                                    reports.push(serde_json::json!({
                                        "folder": folder_name,
                                        "file": file_name,
                                        "title": title,
                                        "sessionId": session_id,
                                        "projectName": report_project,
                                    }));
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(serde_json::json!({ "reports": reports, "count": reports.len() }))
    }

    async fn handle_read_report(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let folder = args
            .get("folder")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("read_report requires 'folder'"))?;
        let file = args
            .get("file")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("read_report requires 'file'"))?;

        tracing::info!(session_id = %ctx.session_id, folder = %folder, file = %file, "MCP tool: read_report");

        // Sanitize to prevent path traversal
        if folder.contains("..")
            || file.contains("..")
            || folder.contains('/')
            || file.contains('/')
        {
            anyhow::bail!("invalid path");
        }

        let data_dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".peckboard");
        let path = data_dir.join("reports").join(folder).join(file);

        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|_| anyhow::anyhow!("report not found: {folder}/{file}"))?;

        // Strip frontmatter for the body
        let body = if content.starts_with("---") {
            content
                .splitn(3, "---")
                .nth(2)
                .unwrap_or(&content)
                .trim()
                .to_string()
        } else {
            content
        };

        Ok(serde_json::json!({
            "status": "ok",
            "folder": folder,
            "file": file,
            "content": body,
        }))
    }

    async fn handle_read_worker_session(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let target_session_id = args
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("read_worker_session requires 'session_id'"))?;
        let last_n = args
            .get("last_n")
            .and_then(|v| v.as_i64())
            .unwrap_or(50)
            .min(200) as i64;

        tracing::info!(session_id = %ctx.session_id, target = %target_session_id, "MCP tool: read_worker_session");

        // Verify target is in the same project
        let my_project = self.resolve_project_id(ctx).await;
        let target_session = ctx
            .db
            .get_session(target_session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("session not found"))?;

        if my_project.is_some() && target_session.project_id != my_project {
            anyhow::bail!("cannot read sessions from other projects");
        }

        let card_title = if let Some(ref cid) = target_session.card_id {
            ctx.db.get_card(cid).await.ok().flatten().map(|c| c.title)
        } else {
            None
        };

        let events = ctx.db.events_tail(target_session_id, last_n).await?;

        let summary: Vec<Value> = events
            .iter()
            .map(|e| {
                let data: Value = serde_json::from_str(&e.data).unwrap_or_default();
                let mut entry = serde_json::json!({
                    "seq": e.seq,
                    "kind": e.kind,
                    "ts": e.ts,
                });
                // Include key fields based on event kind
                match e.kind.as_str() {
                    "user" => {
                        entry["text"] = data.get("text").cloned().unwrap_or_default();
                    }
                    "agent-text" => {
                        entry["text"] = data.get("text").cloned().unwrap_or_default();
                    }
                    "agent-tool-start" => {
                        entry["tool"] = data.get("name").cloned().unwrap_or_default();
                        entry["input"] = data.get("input").cloned().unwrap_or_default();
                    }
                    "agent-tool-end" => {
                        entry["error"] = data.get("error").cloned().unwrap_or_default();
                    }
                    "agent-start" => {
                        entry["model"] = data.get("model").cloned().unwrap_or_default();
                    }
                    "agent-end" => {
                        entry["status"] = data.get("status").cloned().unwrap_or_default();
                    }
                    _ => {
                        entry["data"] = data;
                    }
                }
                entry
            })
            .collect();

        Ok(serde_json::json!({
            "status": "ok",
            "session_id": target_session_id,
            "session_name": target_session.name,
            "card_title": card_title,
            "is_worker": target_session.is_worker,
            "event_count": summary.len(),
            "events": summary,
        }))
    }

    async fn handle_list_worker_sessions(&self, ctx: &ToolCallContext) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, "MCP tool: list_worker_sessions");

        let project_id = self
            .resolve_project_id(ctx)
            .await
            .ok_or_else(|| anyhow::anyhow!("no project context"))?;

        let workers = ctx.db.list_worker_sessions_by_project(&project_id).await?;

        let mut items = Vec::new();
        for ws in &workers {
            let card_info = if let Some(ref cid) = ws.card_id {
                ctx.db.get_card(cid).await.ok().flatten().map(|c| {
                    serde_json::json!({
                        "id": c.id,
                        "title": c.title,
                        "step": c.step,
                        "priority": c.priority,
                        "blocked": c.blocked,
                    })
                })
            } else {
                None
            };

            items.push(serde_json::json!({
                "session_id": ws.id,
                "session_name": ws.name,
                "card": card_info,
                "is_current": ws.id == ctx.session_id,
                "last_activity": ws.last_activity,
            }));
        }

        Ok(serde_json::json!({
            "status": "ok",
            "project_id": project_id,
            "workers": items,
            "count": items.len(),
        }))
    }

    async fn deliver_to_worker(
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

    /// Helper: resolve project_id from context or session lookup
    async fn resolve_project_id(&self, ctx: &ToolCallContext) -> Option<String> {
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

    /// Helper: resolve card title from context or session lookup
    async fn resolve_card_title(&self, ctx: &ToolCallContext) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(names.contains(&"list_projects"));
        assert!(names.contains(&"list_workflows"));
        assert!(names.contains(&"write_report"));
        assert!(names.contains(&"attach_report_file"));
        assert!(names.contains(&"update_card"));
        assert!(names.contains(&"update_project"));
        assert!(names.contains(&"create_project"));
        assert!(names.contains(&"pause_project"));
        assert!(names.contains(&"resume_project"));
        assert!(names.contains(&"delete_card"));
        assert!(names.contains(&"move_card_to_done"));
        assert!(names.contains(&"move_card_to_wont_do"));
        assert_eq!(names.len(), 27);
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
            broadcaster: crate::ws::broadcaster::Broadcaster::new(),
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
        // Config uses command/args format (stdio subprocess)
        assert!(content["mcpServers"]["peckboard"]["command"].is_string());
        assert_eq!(
            content["mcpServers"]["peckboard"]["env"]["PECKBOARD_TOKEN"],
            "tok123"
        );
        assert_eq!(
            content["mcpServers"]["peckboard"]["env"]["PECKBOARD_MCP_URL"],
            "http://127.0.0.1:3333/mcp"
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
        let t1 = registry.issue_token("sess-1".into(), None).await;
        let t2 = registry
            .issue_token("sess-1".into(), Some("proj-b".into()))
            .await;
        let t3 = registry.issue_token("sess-2".into(), None).await;

        registry.revoke_by_session("sess-1").await;

        assert!(registry.lookup(&t1).await.is_none());
        assert!(registry.lookup(&t2).await.is_none());
        assert!(registry.lookup(&t3).await.is_some());
    }
}

use serde_json::Value;

use super::super::McpToolRegistry;
use crate::service::mcp_server::context::ToolCallContext;

impl McpToolRegistry {
    pub(crate) async fn handle_ask_user(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
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

    pub(crate) async fn handle_list_workflows(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, "MCP tool: list_workflows");

        // Optional project_id: if supplied (and the caller has scope to
        // it), every step that has a project-specific override gets
        // an extra `project_instructions` field so the caller sees the
        // built-in text AND the project extension side by side.
        let project_id_arg = args.get("project_id").and_then(|v| v.as_str());
        let scoped_project_id = if project_id_arg.is_some() {
            Some(
                ctx.scope_project(project_id_arg)
                    .await?
                    .as_str()
                    .to_string(),
            )
        } else {
            None
        };

        let overrides: std::collections::HashMap<(String, String), String> =
            if let Some(pid) = scoped_project_id.as_deref() {
                ctx.db
                    .list_project_workflow_instructions(pid)
                    .await?
                    .into_iter()
                    .map(|r| ((r.workflow_id, r.step), r.instructions))
                    .collect()
            } else {
                std::collections::HashMap::new()
            };

        let workflows: Vec<Value> = crate::workflow::WORKFLOWS
            .iter()
            .map(|wf| {
                let steps: Vec<Value> = wf
                    .steps
                    .iter()
                    .map(|s| {
                        let mut entry = serde_json::json!({
                            "step": s.step,
                            "instructions": s.instructions,
                        });
                        if let Some(extra) = overrides.get(&(wf.id.to_string(), s.step.to_string()))
                        {
                            entry["project_instructions"] =
                                serde_json::Value::String(extra.clone());
                        }
                        entry
                    })
                    .collect();
                serde_json::json!({
                    "id": wf.id,
                    "name": wf.name,
                    "description": wf.description,
                    "priority": wf.priority,
                    "steps": steps,
                })
            })
            .collect();

        Ok(serde_json::json!({
            "workflows": workflows,
            "project_id": scoped_project_id,
        }))
    }

    /// `set_workflow_instructions` MCP tool. Lets a worker (or operator,
    /// via the MCP CLI) edit a project's per-step prompt extension —
    /// the same data the edit-project UI writes to. Empty / missing
    /// `instructions` deletes the override.
    pub(crate) async fn handle_set_workflow_instructions(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let scope = ctx
            .scope_project(args.get("project_id").and_then(|v| v.as_str()))
            .await?;
        let project_id = scope.as_str();

        let workflow_id = args
            .get("workflow_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("set_workflow_instructions requires 'workflow_id'"))?;
        let step = args
            .get("step")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("set_workflow_instructions requires 'step'"))?;
        let instructions = args
            .get("instructions")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        tracing::info!(
            session_id = %ctx.session_id,
            project_id = %project_id,
            workflow_id = %workflow_id,
            step = %step,
            "MCP tool: set_workflow_instructions",
        );

        // Validate the (workflow, step) pair so a typo isn't silently
        // stored and ignored at worker-spawn time.
        let wf = crate::workflow::workflow_by_id(workflow_id)
            .ok_or_else(|| anyhow::anyhow!("unknown workflow id '{workflow_id}'"))?;
        let step_def = wf
            .steps
            .iter()
            .find(|s| s.step == step)
            .ok_or_else(|| anyhow::anyhow!("workflow '{workflow_id}' has no step '{step}'"))?;
        if step_def.instructions.is_empty() {
            anyhow::bail!("step '{step}' does not run a worker; cannot attach instructions");
        }

        // Ensure the project exists so we don't insert orphan rows.
        let exists = ctx.db.get_project(project_id).await?.is_some();
        if !exists {
            anyhow::bail!("project not found: {project_id}");
        }

        let row = ctx
            .db
            .upsert_project_workflow_instruction(project_id, workflow_id, step, instructions)
            .await?;

        Ok(serde_json::json!({
            "status": "ok",
            "project_id": project_id,
            "workflow_id": workflow_id,
            "step": step,
            "instructions": row.map(|r| r.instructions).unwrap_or_default(),
        }))
    }

    pub(crate) async fn handle_fetch_url(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
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

    pub(crate) async fn handle_list_system_prompts(
        &self,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, "MCP tool: list_system_prompts");
        let prompts = ctx.db.list_system_prompts().await?;
        let items: Vec<Value> = prompts
            .iter()
            .map(|p| {
                // A one-line summary: collapse whitespace and cap length so the
                // list stays scannable without dumping full bodies.
                let summary: String = p.body.split_whitespace().collect::<Vec<_>>().join(" ");
                let summary: String = summary.chars().take(140).collect();
                serde_json::json!({
                    "name": p.name,
                    "summary": summary,
                    "source_url": p.source_url,
                })
            })
            .collect();
        Ok(serde_json::json!({ "prompts": items, "total": items.len() }))
    }

    pub(crate) async fn handle_list_models(&self, ctx: &ToolCallContext) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, "MCP tool: list_models");

        let registry = ctx
            .provider_registry
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("provider registry not available"))?;

        // Resolve effective (settings-derived) model lists once, then
        // derive both the flat list and per-provider counts from it.
        let providers = registry.list_providers_with_models().await;

        let models: Vec<Value> = providers
            .iter()
            .flat_map(|p| {
                p.models.iter().map(move |model| {
                    serde_json::json!({
                        "id": format!("{}:{}", p.id, model.id),
                        "model_id": model.id,
                        "display_name": model.display_name,
                        "capabilities": model.capabilities,
                        "tier": model.tier,
                    })
                })
            })
            .collect();
        let provider_list: Vec<Value> = providers
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "display_name": p.display_name,
                    "model_count": p.models.len(),
                })
            })
            .collect();

        Ok(serde_json::json!({
            "models": models,
            "providers": provider_list,
            "total": models.len(),
        }))
    }
}

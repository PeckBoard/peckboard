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
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, "MCP tool: list_workflows");

        Ok(serde_json::json!({ "workflows": crate::workflow::WORKFLOWS }))
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

    pub(crate) async fn handle_list_models(&self, ctx: &ToolCallContext) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, "MCP tool: list_models");

        let registry = ctx
            .provider_registry
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("provider registry not available"))?;

        let all_models = registry.list_all_models().await;
        let providers = registry.list_providers().await;

        let models: Vec<Value> = all_models
            .iter()
            .map(|(full_id, model)| {
                serde_json::json!({
                    "id": full_id,
                    "model_id": model.id,
                    "display_name": model.display_name,
                    "capabilities": model.capabilities,
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

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

        // A document-review session asking a question parks its review in
        // 'needs_input', so the review screen swaps the running spinner for
        // the question card. No-op for every other kind of session.
        crate::service::doc_reviews::mark_needs_input(&ctx.db, &ctx.broadcaster, &ctx.session_id)
            .await;
        // A worker asking the user is parked until the answer lands: block
        // the card so the orchestrator's `available` filter skips it. The
        // watchdog separately keeps `worker_session_id` on the card while a
        // question is unanswered, so answering resumes THIS session instead
        // of spawning a second worker. Released by
        // `questions::clear_question_block` on answer/dismiss.
        if let Some(ref card_id) = resolved_card_id {
            crate::service::questions::block_card_for_question(&ctx.db, &ctx.broadcaster, card_id)
                .await;
        }

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

        let workflows: Vec<Value> = crate::workflow::all_workflows()
            .into_iter()
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
                    "source": wf.source,
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
            .ok_or_else(|| anyhow::anyhow!("fetch_url requires 'url'"))?
            .to_string();

        let max_length = args
            .get("max_length")
            .and_then(|v| v.as_u64())
            .unwrap_or(10000) as usize;

        tracing::info!(session_id = %ctx.session_id, url = %url, "MCP tool: fetch_url");

        // Route through the plugin host's hardened fetch (same containment as
        // `fetch_web`): http/https + GET only, private/loopback/link-local/CGNAT
        // targets refused, DNS-pinned against rebinding, no redirects followed,
        // 5 MiB body cap. A bare `reqwest::Client::get(url)` here would be an
        // unrestricted SSRF primitive reachable from worker/chat/pre-hatcher.
        let fetch_input = serde_json::json!({ "url": url, "method": "GET" }).to_string();
        let raw =
            tokio::task::spawn_blocking(move || crate::plugin::host::http_fetch_impl(&fetch_input))
                .await?;
        let parsed: Value = serde_json::from_str(&raw)?;

        if let Some(fetch_err) = parsed.get("error").and_then(|v| v.as_str()) {
            return Ok(serde_json::json!({
                "status": "error",
                "message": fetch_err,
            }));
        }
        let status = parsed.get("status").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
        if !(200..300).contains(&status) {
            return Ok(serde_json::json!({
                "status": "error",
                "http_status": status,
                "message": format!("HTTP {status}")
            }));
        }
        let body = parsed
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

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
            // Byte-slicing an arbitrary fetched `&str` at a fixed offset can land
            // mid-codepoint; walk back to the nearest char boundary first.
            let mut end = max_length.min(text.len());
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}... (truncated at {} chars)", &text[..end], max_length)
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
        // Hidden (disabled) providers are excluded before resolution so
        // they are never probed for models.
        let hidden =
            crate::routes::settings::hidden_providers_for_db(ctx.db.as_ref().clone()).await;
        let providers = registry.list_providers_with_models_except(&hidden).await;

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
                        "thinking": model.is_thinking(),
                        "images_in": p.capabilities.model_images_in(model),
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
                    "capabilities": p.capabilities,
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

#[cfg(test)]
mod fetch_url_tests {
    use super::McpToolRegistry;
    use crate::service::mcp_server::context::ToolCallContext;
    use std::sync::Arc;

    async fn ctx_with_folder() -> (ToolCallContext, tempfile::TempDir) {
        use crate::db::models::{NewFolder, NewSession};
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(crate::db::Db::in_memory().unwrap());
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f-1".into(),
            name: "f-1".into(),
            path: dir.path().to_string_lossy().to_string(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_session(NewSession {
            id: "s-1".into(),
            name: "s-1".into(),
            folder_id: "f-1".into(),
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
        let ctx = ToolCallContext {
            session_id: "s-1".into(),
            project_id: None,
            card_id: None,
            folder_id: "f-1".into(),
            db,
            broadcaster: crate::ws::broadcaster::Broadcaster::new(),
            provider_registry: None,
            data_dir: None,
        };
        (ctx, dir)
    }

    async fn assert_refused(url: &str) {
        let (ctx, _dir) = ctx_with_folder().await;
        let reg = McpToolRegistry::new();
        let out = reg
            .handle_fetch_url(serde_json::json!({ "url": url }), &ctx)
            .await
            .unwrap();
        assert_eq!(out["status"], "error", "expected refusal for {url}: {out}");
    }

    #[tokio::test]
    async fn fetch_url_refuses_loopback() {
        assert_refused("http://127.0.0.1:9/").await;
    }

    #[tokio::test]
    async fn fetch_url_refuses_link_local_metadata_ip() {
        assert_refused("http://169.254.169.254/latest/meta-data/").await;
    }

    #[tokio::test]
    async fn fetch_url_refuses_non_http_scheme() {
        assert_refused("file:///etc/passwd").await;
    }

    #[test]
    fn truncate_does_not_panic_on_multibyte_boundary() {
        // 9999 ascii bytes then a 3-byte codepoint straddling the default
        // max_length=10000 boundary at byte offset 10000..10003.
        let text: String = "a".repeat(9999) + "\u{2014}" + &"b".repeat(20);
        let max_length = 10000usize;
        assert!(text.len() > max_length);

        let mut end = max_length.min(text.len());
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        let truncated = format!("{}... (truncated at {} chars)", &text[..end], max_length);
        assert!(truncated.starts_with(&"a".repeat(9999)));
    }
}

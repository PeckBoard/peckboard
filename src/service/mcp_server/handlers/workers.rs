use serde_json::Value;

use super::super::McpToolRegistry;
use crate::service::mcp_server::context::ToolCallContext;

impl McpToolRegistry {
    pub(crate) async fn handle_notify_workers(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let project_id = ctx
            .project_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("notify_workers requires project context"))?;

        tracing::info!(session_id = %ctx.session_id, project_id = %project_id, "MCP tool: notify_workers");

        // Rate limit check
        if let Err(reset_in) = self.comm_limiter.check(project_id) {
            return Ok(serde_json::json!({
                "status": "rate_limited",
                "message": format!("Inter-worker communication rate limit reached. Try again in {reset_in} seconds. Limit: {} messages per {} seconds per project.", self.comm_limiter.max_per_window, self.comm_limiter.window_secs),
                "retry_after_seconds": reset_in,
            }));
        }

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
            // Skip terminal cards — they don't need notifications
            if self.is_session_terminal(ctx, session).await {
                continue;
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

    pub(crate) async fn handle_share_finding(
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

        let project_id = self.resolve_project_id(ctx).await;

        // Rate limit check
        if let Some(ref pid) = project_id {
            if let Err(reset_in) = self.comm_limiter.check(pid) {
                return Ok(serde_json::json!({
                    "status": "rate_limited",
                    "message": format!("Inter-worker communication rate limit reached. Try again in {reset_in} seconds. Limit: {} messages per {} seconds per project.", self.comm_limiter.max_per_window, self.comm_limiter.window_secs),
                    "retry_after_seconds": reset_in,
                }));
            }
        }

        // Check worker_communication is enabled
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
                    if self.is_session_terminal(ctx, ws).await {
                        continue;
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

    pub(crate) async fn handle_get_finding_details(
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

        // Token-scope check: the finding lives on some session; that
        // session's project must match the caller's token scope.
        let _scope = ctx.scope_session(&event.session_id).await?;

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

    pub(crate) async fn handle_send_worker_message(
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

        // Token-scope check: target session must live in the same project
        // as the caller's MCP token. This blocks a worker on project A
        // from messaging a worker on project B by guessing session ids.
        let _scope = ctx.scope_session(target_session_id).await?;

        let project_id = self.resolve_project_id(ctx).await;

        // Rate limit check
        if let Some(ref pid) = project_id {
            if let Err(reset_in) = self.comm_limiter.check(pid) {
                return Ok(serde_json::json!({
                    "status": "rate_limited",
                    "message": format!("Inter-worker communication rate limit reached. Try again in {reset_in} seconds. Limit: {} messages per {} seconds per project.", self.comm_limiter.max_per_window, self.comm_limiter.window_secs),
                    "retry_after_seconds": reset_in,
                }));
            }
        }

        // Check worker_communication is enabled
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

        // Reject sends to workers whose card is done or wont_do
        if self.is_session_terminal(ctx, &target).await {
            return Ok(serde_json::json!({
                "status": "target_terminal",
                "message": "Target worker's card is done or wont_do — message not delivered.",
            }));
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

    pub(crate) async fn handle_read_worker_session(
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

        // Folder boundary first: a foreign-folder session must look
        // exactly like a non-existent id — no 403 leak. We also enforce
        // the project boundary for worker tokens (token has a project,
        // target must match) to keep the prior semantics, but the folder
        // check is the primary guard.
        let target_session = ctx
            .db
            .get_session(target_session_id)
            .await?
            .filter(|s| s.folder_id == ctx.folder_id)
            .ok_or_else(|| anyhow::anyhow!("session not found: {target_session_id}"))?;

        let my_project = self.resolve_project_id(ctx).await;
        if my_project.is_some() && target_session.project_id != my_project {
            anyhow::bail!("session not found: {target_session_id}");
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

    pub(crate) async fn handle_list_worker_sessions(
        &self,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
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
}

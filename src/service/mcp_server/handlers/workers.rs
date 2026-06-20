use serde_json::Value;

use super::super::McpToolRegistry;
use crate::db::models::Event;
use crate::service::mcp_server::context::ToolCallContext;

/// Condense one stored event into the compact shape returned by
/// `read_worker_session` / `search_worker_session`. Keeps the
/// debug-relevant fields per kind and drops the rest so a reader doesn't
/// have to wade through full provider payloads.
fn summarize_event(e: &Event) -> Value {
    let data: Value = serde_json::from_str(&e.data).unwrap_or_default();
    let mut entry = serde_json::json!({
        "seq": e.seq,
        "kind": e.kind,
        "ts": e.ts,
    });
    match e.kind.as_str() {
        "user" | "agent-text" => {
            entry["text"] = data.get("text").cloned().unwrap_or_default();
        }
        "agent-tool-start" => {
            entry["tool"] = data.get("name").cloned().unwrap_or_default();
            entry["input"] = data.get("input").cloned().unwrap_or_default();
        }
        "agent-tool-end" => {
            entry["tool"] = data.get("name").cloned().unwrap_or_default();
            entry["error"] = data.get("error").cloned().unwrap_or_default();
        }
        "agent-start" => {
            entry["model"] = data.get("model").cloned().unwrap_or_default();
        }
        "agent-end" => {
            entry["status"] = data.get("status").cloned().unwrap_or_default();
            entry["reason"] = data.get("reason").cloned().unwrap_or_default();
        }
        _ => {
            entry["data"] = data;
        }
    }
    entry
}

/// True if an event represents an error/failure: a top-level `error`
/// event, a tool call that returned a non-null `error`, or an agent run
/// that ended `crashed`. Used by `search_worker_session`'s `errors_only`
/// filter so a reader can pull just the failures out of a long session.
fn event_is_error(e: &Event) -> bool {
    if e.kind == "error" {
        return true;
    }
    let data: Value = serde_json::from_str(&e.data).unwrap_or_default();
    match e.kind.as_str() {
        "agent-tool-end" => data.get("error").map(|v| !v.is_null()).unwrap_or(false),
        "agent-end" => data
            .get("status")
            .and_then(|v| v.as_str())
            .map(|s| s.eq_ignore_ascii_case("crashed") || s.eq_ignore_ascii_case("error"))
            .unwrap_or(false),
        _ => false,
    }
}

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

        let target_session = self.scope_readable_session(ctx, target_session_id).await?;

        let card_title = if let Some(ref cid) = target_session.card_id {
            ctx.db.get_card(cid).await.ok().flatten().map(|c| c.title)
        } else {
            None
        };

        let events = ctx.db.events_tail(target_session_id, last_n).await?;

        let summary: Vec<Value> = events.iter().map(summarize_event).collect();

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

    /// Resolve a target session the caller is allowed to read, enforcing
    /// the same boundary `read_worker_session` has always used:
    ///
    /// 1. Folder boundary first — a session in another folder must look
    ///    exactly like a non-existent id (no 403 existence leak).
    /// 2. Project boundary for worker tokens — a token scoped to project A
    ///    can only reach sessions in project A.
    ///
    /// Returns "not found" framing on every rejection so a caller can't
    /// probe for sessions outside its scope by guessing ids.
    async fn scope_readable_session(
        &self,
        ctx: &ToolCallContext,
        target_session_id: &str,
    ) -> anyhow::Result<crate::db::models::Session> {
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
        Ok(target_session)
    }

    /// Every session the caller may read: all sessions in the caller's
    /// folder, narrowed to the caller's project when the token is
    /// project-scoped. Mirrors the boundary in [`Self::scope_readable_session`]
    /// for the "search across all sessions" path of `search_worker_session`.
    async fn readable_sessions_in_scope(
        &self,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Vec<crate::db::models::Session>> {
        let mut sessions = ctx.db.list_sessions_by_folder(&ctx.folder_id).await?;
        let my_project = self.resolve_project_id(ctx).await;
        if my_project.is_some() {
            sessions.retain(|s| s.project_id == my_project);
        }
        Ok(sessions)
    }

    pub(crate) async fn handle_search_sessions(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        // How many recent events to pull from the DB before refining in
        // Rust. Bounds the work for very long sessions; matches older than
        // this window aren't returned (reported via `scan_truncated`).
        const SCAN_LIMIT: i64 = 2000;

        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .map(|s| s.to_lowercase());
        let errors_only = args
            .get("errors_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let kinds: Option<Vec<String>> = args.get("kinds").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        });
        let limit = args
            .get("limit")
            .and_then(|v| v.as_i64())
            .unwrap_or(50)
            .clamp(1, 200) as usize;

        // Require at least one filter — this tool exists to avoid reading
        // an entire session, so an unfiltered "dump everything" is refused.
        // Use read_worker_session for a plain tail instead.
        if query.is_none() && !errors_only && kinds.is_none() {
            anyhow::bail!(
                "search_sessions requires at least one of 'query', 'errors_only', or 'kinds'. Use read_worker_session for a plain tail."
            );
        }

        let target_session_id = args.get("session_id").and_then(|v| v.as_str());
        tracing::info!(
            session_id = %ctx.session_id,
            target = %target_session_id.unwrap_or("<all>"),
            "MCP tool: search_sessions"
        );

        // Resolve the set of sessions to search, enforcing the read boundary.
        let targets = match target_session_id {
            Some(id) => vec![self.scope_readable_session(ctx, id).await?],
            None => self.readable_sessions_in_scope(ctx).await?,
        };
        let names: std::collections::HashMap<String, String> = targets
            .iter()
            .map(|s| (s.id.clone(), s.name.clone()))
            .collect();
        let session_ids: Vec<String> = targets.iter().map(|s| s.id.clone()).collect();

        // Push the kind filter to SQL. When only `errors_only` is set,
        // restrict to the kinds that can carry an error so the scan window
        // covers more failures.
        let sql_kinds = kinds.clone().or_else(|| {
            errors_only.then(|| {
                vec![
                    "error".to_string(),
                    "agent-tool-end".to_string(),
                    "agent-end".to_string(),
                ]
            })
        });

        let events = ctx
            .db
            .search_session_events(session_ids, sql_kinds, SCAN_LIMIT)
            .await?;
        let scan_truncated = events.len() as i64 >= SCAN_LIMIT;

        let mut matches: Vec<Value> = Vec::new();
        for e in &events {
            if errors_only && !event_is_error(e) {
                continue;
            }
            if let Some(ref q) = query {
                // Match against the raw event payload plus its kind so a
                // search hits text, tool names, inputs, and error strings.
                let hay = format!("{} {}", e.kind, e.data).to_lowercase();
                if !hay.contains(q) {
                    continue;
                }
            }
            let mut entry = summarize_event(e);
            entry["session_id"] = serde_json::json!(e.session_id);
            entry["session_name"] = serde_json::json!(names.get(&e.session_id));
            matches.push(entry);
            if matches.len() >= limit {
                break;
            }
        }

        Ok(serde_json::json!({
            "status": "ok",
            "scope": target_session_id.map(|_| "session").unwrap_or("all_readable_sessions"),
            "sessions_searched": targets.len(),
            "match_count": matches.len(),
            "truncated": matches.len() >= limit,
            "scan_truncated": scan_truncated,
            "matches": matches,
        }))
    }

    pub(crate) async fn handle_list_sessions(
        &self,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, "MCP tool: list_sessions");

        // Every session the caller may read — chat, worker, and expert
        // alike. For a chat session this is its whole folder; for a worker
        // token it's the token's project. Used to discover sessions to
        // read/search for debugging.
        let mut sessions = self.readable_sessions_in_scope(ctx).await?;
        sessions.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));

        let mut items = Vec::new();
        for s in &sessions {
            let kind = if s.is_expert {
                "expert"
            } else if s.is_worker {
                "worker"
            } else {
                "chat"
            };
            let card_title = if let Some(ref cid) = s.card_id {
                ctx.db.get_card(cid).await.ok().flatten().map(|c| c.title)
            } else {
                None
            };
            items.push(serde_json::json!({
                "session_id": s.id,
                "session_name": s.name,
                "kind": kind,
                "project_id": s.project_id,
                "card_title": card_title,
                "is_current": s.id == ctx.session_id,
                "last_activity": s.last_activity,
            }));
        }

        Ok(serde_json::json!({
            "status": "ok",
            "sessions": items,
            "count": items.len(),
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

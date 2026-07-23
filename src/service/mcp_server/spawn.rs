//! Production `ExpertDispatcher` backed by the live `AppState`. Lives here
//! (rather than in `context.rs`) so the narrow `ExpertDispatcher` trait the
//! tool layer depends on stays free of any `AppState` coupling — only this
//! impl, constructed in the `mcp` route, pulls the full app in.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::provider::stream::SpawnConfig;
use crate::service::mcp_server::context::ExpertDispatcher;
use crate::state::AppState;

/// Dispatches a capture run on an expert session using the app's
/// `SessionManager`, mirroring the token + MCP-config + locked-dispatch
/// dance the worker orchestrator uses in `spawn_worker_for_card`.
pub struct AppExpertDispatcher {
    state: Arc<AppState>,
}

impl AppExpertDispatcher {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    /// Issue an MCP token + config for `session_id` and build a `SpawnConfig`
    /// for it. Shared by the capture-run and resume paths. `metadata` lets the
    /// caller tag the run (capture vs. ordinary resume). `send_message_locked`
    /// resolves `working_dir` from the session's folder, so an empty string
    /// here is fine.
    async fn spawn_config_for(
        &self,
        session_id: &str,
        metadata: serde_json::Value,
    ) -> anyhow::Result<SpawnConfig> {
        let state = &self.state;
        let session = state
            .db
            .get_session(session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("session not found: {session_id}"))?;

        let mcp_token = state
            .mcp_tokens
            .issue_token(session_id.to_string(), session.project_id.clone())
            .await;

        let mcp_config_path = crate::service::mcp_server::write_mcp_config(
            &state.config.data_dir,
            session_id,
            state.config.port,
            &mcp_token,
        )
        .ok()
        .map(|p| p.to_string_lossy().to_string());

        Ok(SpawnConfig {
            model: session.model.clone().unwrap_or_else(|| "default".into()),
            effort: session.effort.clone(),
            working_dir: String::new(),
            mcp_config_path,
            env: Default::default(),
            permission_mode: Some("bypass".into()),
            timeout_ms: None,
            metadata,
            system_prompt_suffix: None,
            // Filled from the session row in SessionManager::final_config.
            system_prompt_override: None,
            // Populated in SessionManager::final_config from the plugin registry.
            extra_allowed_tools: Vec::new(),
            extra_disallowed_tools: Vec::new(),
            // Set from the session row in SessionManager::final_config.
            is_worker: false,
            is_pre_hatcher: false,
        })
    }

    /// Deliver a user message with attachments to `session_id` and resume it —
    /// the attachment-carrying twin of `resume_session`, used by the
    /// session-control plugin's `send_message`. Resumes via `send_or_queue`
    /// (spawn if idle, inject/queue if running), exactly like a user message.
    pub async fn send_message_with_attachments(
        &self,
        session_id: &str,
        text: String,
        attachments: Vec<crate::provider::message::UserAttachment>,
    ) -> anyhow::Result<()> {
        let state = &self.state;
        let config = self
            .spawn_config_for(session_id, serde_json::Value::Null)
            .await?;
        let message = crate::provider::message::UserMessage { text, attachments };
        state
            .session_manager
            .send_or_queue(session_id, message, &state.db, &state.broadcaster, config)
            .await?;
        Ok(())
    }
}

/// Bridges the plugin layer's [`crate::plugin::host::LiveHost`] to the live
/// app, so a WASM plugin's `peckboard_dispatch_capture` / `_resume_session`
/// host calls schedule real agent runs. Holds a `Weak<AppState>` to avoid an
/// `AppState → PluginManager → LiveHost → AppState` reference cycle, and a
/// runtime `Handle` so its fire-and-forget methods (called from a synchronous
/// WASM host function) can spawn the async dispatch without blocking.
pub struct AppLiveHost {
    state: std::sync::Weak<AppState>,
    rt: tokio::runtime::Handle,
}

impl AppLiveHost {
    pub fn new(state: &Arc<AppState>, rt: tokio::runtime::Handle) -> Self {
        Self {
            state: Arc::downgrade(state),
            rt,
        }
    }
}

impl crate::plugin::host::LiveHost for AppLiveHost {
    fn dispatch_capture(&self, session_id: String, prompt: String) {
        let Some(state) = self.state.upgrade() else {
            return; // app is shutting down — nothing to dispatch to
        };
        self.rt.spawn(async move {
            if let Err(e) = AppExpertDispatcher::new(state)
                .dispatch_capture(&session_id, &prompt)
                .await
            {
                tracing::warn!("plugin dispatch_capture for {session_id} failed: {e}");
            }
        });
    }

    fn resume_session(&self, session_id: String, text: String) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        self.rt.spawn(async move {
            if let Err(e) = AppExpertDispatcher::new(state)
                .resume_session(&session_id, &text)
                .await
            {
                tracing::warn!("plugin resume_session for {session_id} failed: {e}");
            }
        });
    }

    fn answer_question(
        &self,
        session_id: String,
        question_id: String,
        answers: serde_json::Value,
        rejected: bool,
        user_id: String,
    ) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        self.rt.spawn(async move {
            // Mirror the shapes core's own UI posts: `{question_id, answers}`
            // to answer, `{question_id, rejected: true}` to dismiss.
            let data = if rejected {
                serde_json::json!({ "question_id": question_id, "rejected": true })
            } else {
                serde_json::json!({ "question_id": question_id, "answers": answers })
            };
            if let Err(e) = crate::service::questions::resolve_question(
                state,
                user_id,
                session_id.clone(),
                data,
            )
            .await
            {
                tracing::warn!("plugin answer_question for {session_id} failed: {e}");
            }
        });
    }
    fn ask_user(
        &self,
        session_id: String,
        question: String,
        options: Vec<String>,
        token: String,
        redirect_session_id: Option<String>,
    ) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        self.rt.spawn(async move {
            if let Err(e) = emit_plugin_question(
                &state.db,
                &state.broadcaster,
                &session_id,
                &question,
                &options,
                &token,
                redirect_session_id.as_deref(),
            )
            .await
            {
                tracing::warn!("plugin ask_user for {session_id} failed: {e}");
            }
        });
    }

    fn send_message(
        &self,
        session_id: String,
        text: String,
        attachments: Vec<crate::plugin::host::LiveAttachment>,
    ) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let attachments: Vec<crate::provider::message::UserAttachment> = attachments
            .into_iter()
            .map(|a| crate::provider::message::UserAttachment {
                filename: a.filename,
                mime_type: a.mime_type,
                data: a.data,
            })
            .collect();
        self.rt.spawn(async move {
            if let Err(e) = AppExpertDispatcher::new(state)
                .send_message_with_attachments(&session_id, text, attachments)
                .await
            {
                tracing::warn!("plugin send_message for {session_id} failed: {e}");
            }
        });
    }

    fn deliver_user_message(&self, session_id: String, text: String, data: serde_json::Value) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        self.rt.spawn(async move {
            // Persist + broadcast the user event first so the transcript
            // shows the message before any agent output streams in.
            broadcast_session_event(&state, &session_id, "user", data).await;
            if let Err(e) = AppExpertDispatcher::new(state)
                .resume_session(&session_id, &text)
                .await
            {
                tracing::warn!("plugin deliver_user_message for {session_id} failed: {e}");
            }
        });
    }
    fn interrupt_session(&self, session_id: String) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        self.rt.spawn(async move {
            // interrupt deliberately preserves the queued follow-up (it is a
            // "release the current turn" signal, not a hard stop); the
            // completion listener drains the queue afterwards. Use
            // terminate_agent for a hard stop that discards the queue.
            state.session_manager.interrupt(&session_id).await;
            broadcast_session_event(
                &state,
                &session_id,
                "interrupt",
                serde_json::json!({ "reason": "plugin-interrupt" }),
            )
            .await;
        });
    }

    fn terminate_agent(&self, session_id: String) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        self.rt.spawn(async move {
            // Explicit stop: drop any queued follow-up so the completion
            // listener doesn't immediately respawn the run from the queue.
            crate::provider::manager::clear_queued_message(
                &state.db,
                &state.broadcaster,
                &session_id,
            )
            .await;
            state.session_manager.cancel_and_wait(&session_id).await;
            broadcast_session_event(
                &state,
                &session_id,
                "system",
                serde_json::json!({
                    "text": "Agent terminated by session-control. The next message will start a fresh process.",
                }),
            )
            .await;
        });
    }

    fn clear_session(&self, session_id: String) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        self.rt.spawn(async move {
            if let Err(e) = crate::routes::sessions::clear_session_core(&state, &session_id).await {
                tracing::warn!("plugin clear_session for {session_id} failed: {e}");
            }
        });
    }

    fn recycle_agent_after_turn(&self, session_id: String) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        self.rt.spawn(async move {
            // Graceful: the stream loop exits after the current turn
            // (immediately when idle); the next message spawns a fresh child
            // with the session's current model/account/effort.
            crate::provider::manager::shutdown_after_turn_via_registry(
                &state.provider_registry,
                &session_id,
            )
            .await;
        });
    }
}

/// Append an event to `session_id` and broadcast it to live subscribers, in
/// the same `{id,seq,ts,kind,data}` frame shape the session routes use. Shared
/// by the session-control interrupt/terminate paths so the UI reflects them.
async fn broadcast_session_event(
    state: &Arc<AppState>,
    session_id: &str,
    kind: &str,
    data: serde_json::Value,
) {
    if let Ok(event) = state.db.append_event(session_id, kind, data).await {
        state.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "event".into(),
            session_id: session_id.to_string(),
            data: serde_json::json!({
                "id": event.id,
                "seq": event.seq,
                "ts": event.ts,
                "kind": event.kind,
                "data": serde_json::from_str::<serde_json::Value>(&event.data).unwrap_or_default(),
            }),
        });
    }
}

/// Emit a single-question user prompt to `session_id`, mirroring the worker
/// `ask_user` MCP tool's event surface (`handle_ask_user`) so the existing
/// question-card UI renders it and the existing answer → resume machinery
/// applies. `token` is stored on the question (`approval_token`) so the
/// emitting plugin can later resolve the user's answer via
/// `peckboard_get_answer`. Card/project are looked up from the session for the
/// worker-question broadcast.
/// `redirect_session_id`, when set, is persisted on the question event
/// (`redirectSessionId`) so the answer route resumes that session with the
/// user's answer instead of the asker.
pub(crate) async fn emit_plugin_question(
    db: &crate::db::Db,
    broadcaster: &crate::ws::broadcaster::Broadcaster,
    session_id: &str,
    question: &str,
    options: &[String],
    token: &str,
    redirect_session_id: Option<&str>,
) -> anyhow::Result<()> {
    use crate::ws::broadcaster::WsEvent;
    use serde_json::{Value, json};

    let mut entry = json!({
        "question": question,
        "header": "Approval",
        "multiSelect": false,
    });
    if !options.is_empty() {
        entry["options"] = Value::Array(options.iter().map(|o| Value::String(o.clone())).collect());
        entry["optionObjects"] = Value::Array(
            options
                .iter()
                .map(|o| json!({ "label": o, "description": "" }))
                .collect(),
        );
    }

    let session = db.get_session(session_id).await.ok().flatten();
    let card_id = session.as_ref().and_then(|s| s.card_id.clone());
    let project_id = session.as_ref().and_then(|s| s.project_id.clone());
    let is_worker = card_id.is_some() || session.as_ref().map(|s| s.is_worker).unwrap_or(false);

    let mut event_data = json!({
        "questions": [entry],
        // Correlation id the plugin reads back with peckboard_get_answer.
        "approval_token": token,
        "cardId": card_id,
        "sessionId": session_id,
        "source": "plugin",
        "isWorker": is_worker,
    });
    if let Some(ref pid) = project_id {
        event_data["projectId"] = Value::String(pid.clone());
    }
    if let Some(rid) = redirect_session_id {
        event_data["redirectSessionId"] = Value::String(rid.to_string());
    }

    let event = db
        .append_event(session_id, "question", event_data.clone())
        .await?;

    broadcaster.broadcast(WsEvent {
        event_type: "event".into(),
        session_id: session_id.to_string(),
        data: json!({
            "id": event.id,
            "seq": event.seq,
            "ts": event.ts,
            "kind": "question",
            "data": event_data,
        }),
    });

    if is_worker && let Some(ref pid) = project_id {
        broadcaster.broadcast(WsEvent {
            event_type: "worker-question".into(),
            session_id: pid.clone(),
            data: json!({
                "eventId": event.id,
                "sessionId": session_id,
                "projectId": pid,
            }),
        });
    }

    db.append_event(
        session_id,
        "ask-user-requested",
        json!({ "questionEventId": event.id, "cardId": card_id }),
    )
    .await?;

    Ok(())
}

impl ExpertDispatcher for AppExpertDispatcher {
    fn dispatch_capture<'a>(
        &'a self,
        expert_session_id: &'a str,
        prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let state = &self.state;
            let config = self
                .spawn_config_for(
                    expert_session_id,
                    serde_json::json!({ "expert": true, "capture": true }),
                )
                .await?;

            // Capture is a forced fresh run (re-read the scope), so dispatch
            // through the locked path rather than send_or_queue.
            let lock = state.session_manager.lock_session(expert_session_id).await;
            state
                .session_manager
                .send_message_locked(
                    &lock,
                    crate::provider::message::UserMessage::from_text(prompt),
                    &state.db,
                    &state.broadcaster,
                    config,
                )
                .await?;
            Ok(())
        })
    }

    fn resume_session<'a>(
        &'a self,
        session_id: &'a str,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let state = &self.state;
            let config = self
                .spawn_config_for(session_id, serde_json::Value::Null)
                .await?;

            // Resume exactly like a user message: send_or_queue spawns a fresh
            // run if idle, or queues / injects mid-stream if already running.
            state
                .session_manager
                .send_or_queue(
                    session_id,
                    crate::provider::message::UserMessage::from_text(text),
                    &state.db,
                    &state.broadcaster,
                    config,
                )
                .await?;
            Ok(())
        })
    }
}

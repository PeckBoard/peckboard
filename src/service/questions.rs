//! Question resolution — the single implementation behind answering a
//! pending user question. Both the HTTP route (`POST /api/sessions/:id/events`
//! with kind `question-resolved`) and the `peckboard_answer_question` plugin
//! host function land here, so the semantics — persist + broadcast the
//! resolution event, build the human-readable answer, feed question-expert
//! plugins, honor pre-hatcher redirects, resume the conversation — cannot
//! drift between the two surfaces.

use std::sync::Arc;

use crate::db::models::UpdateSession;
use crate::provider::stream::SpawnConfig;
use crate::routes::sessions::resolve_references;
use crate::state::AppState;

/// Resolve a question on `session_id` as `user_id`: append the
/// `question-resolved` event carrying `data` (`{question_id, answers}` or
/// `{question_id, rejected: true}`), broadcast it, then run the full answer
/// flow (readable answer text, expert feed, redirect handling, resume). The
/// conversation resume is spawned — this returns as soon as the resolution
/// event is durable, mirroring the route's behavior.
pub async fn resolve_question(
    state: Arc<AppState>,
    user_id: String,
    session_id: String,
    data: serde_json::Value,
) -> Result<crate::db::models::Event, String> {
    let session = state
        .db
        .get_session(&session_id)
        .await
        .map_err(|e| e.to_string())?;
    if session.is_none() {
        return Err("session not found".to_string());
    }

    let event = state
        .db
        .append_event(&session_id, "question-resolved", data.clone())
        .await
        .map_err(|e| e.to_string())?;

    // Update last_activity to now
    let now = chrono::Utc::now().to_rfc3339();
    let _ = state
        .db
        .update_session(
            &session_id,
            UpdateSession {
                last_activity: Some(now),
                ..Default::default()
            },
        )
        .await;

    // Broadcast the event to WebSocket subscribers
    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "event".into(),
            session_id: session_id.clone(),
            data: serde_json::json!({
                "id": event.id,
                "seq": event.seq,
                "ts": event.ts,
                "kind": event.kind,
                "data": serde_json::from_str::<serde_json::Value>(&event.data).unwrap_or_default(),
            }),
        });

    // A document-review session's question just got an answer: the pass
    // resumes below, so the review goes back to 'running'. No-op for every
    // other kind of session.
    crate::service::doc_reviews::resume_after_question(&state.db, &state.broadcaster, &session_id)
        .await;
    // A worker card parked by `ask_user` comes back into play now that the
    // question is resolved. No-op for non-worker sessions and for cards
    // blocked for any other reason.
    clear_question_block(&state.db, &state.broadcaster, &session_id).await;

    let event_data = data;
    let rejected = event_data
        .get("rejected")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let question_id = event_data
        .get("question_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // When a user answers a worker's question, hand the Q&A to whichever
    // plugin owns question experts (see USER_ANSWER_HOOK). Captured here as
    // (project_id, qa_text) and fired after the conversation resumes; core
    // itself knows nothing about experts.
    let mut question_expert_feed: Option<(String, String)> = None;

    // Build a human-readable answer message to resume the conversation
    let answer_text = if rejected {
        "The user dismissed the question without answering. The questions have been removed from the UI and are no longer visible. Do NOT say the questions are still up. If you still need answers, you must ask again using mcp__peckboard__ask_user.".to_string()
    } else {
        let answers = event_data
            .get("answers")
            .cloned()
            .unwrap_or(serde_json::json!({}));

        // Look up original questions to build readable answer text
        let mut parts = Vec::new();
        if !question_id.is_empty() {
            if let Ok(Some(q_event)) = state.db.get_event(question_id).await {
                if let Ok(q_data) = serde_json::from_str::<serde_json::Value>(&q_event.data) {
                    if let Some(questions_arr) = q_data.get("questions").and_then(|v| v.as_array())
                    {
                        if let Some(answers_obj) = answers.as_object() {
                            for (idx_str, value) in answers_obj {
                                if let Ok(idx) = idx_str.parse::<usize>() {
                                    if let Some(q) = questions_arr.get(idx) {
                                        let q_text = q
                                            .get("question")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("Question");
                                        parts.push(format!(
                                            "**{}**: {}",
                                            q_text,
                                            value.as_str().unwrap_or("")
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let answers_text = if parts.is_empty() {
            format!(
                "User answered: {}",
                serde_json::to_string(&answers).unwrap_or_default()
            )
        } else {
            format!(
                "The user answered your questions (the question form has been removed from the UI):\n\n{}",
                parts.join("\n")
            )
        };

        // Check if this is a worker session with more pending questions
        let session_info = state.db.get_session(&session_id).await.ok().flatten();
        let has_more = if let Some(ref sess) = session_info {
            if sess.is_worker {
                if let Some(ref project_id) = sess.project_id {
                    // Count remaining unresolved questions for this project
                    let worker_sessions = state
                        .db
                        .list_worker_sessions_by_project(project_id)
                        .await
                        .unwrap_or_default();
                    let mut remaining = 0u32;
                    for ws in &worker_sessions {
                        let events = state
                            .db
                            .list_events_by_session(&ws.id, None)
                            .await
                            .unwrap_or_default();
                        let resolved_ids: std::collections::HashSet<String> = events
                            .iter()
                            .filter(|e| e.kind == "question-resolved")
                            .filter_map(|e| {
                                serde_json::from_str::<serde_json::Value>(&e.data)
                                    .ok()
                                    .and_then(|d| {
                                        d.get("question_id")
                                            .or(d.get("questionId"))
                                            .and_then(|v| v.as_str())
                                            .map(|s| s.to_string())
                                    })
                            })
                            .collect();
                        // Exclude the question we just answered
                        remaining += events
                            .iter()
                            .filter(|e| {
                                e.kind == "question"
                                    && !resolved_ids.contains(&e.id)
                                    && e.id != question_id
                            })
                            .count() as u32;
                    }
                    remaining > 0
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };

        // A worker question answered by the user: feed the readable Q&A to
        // the project's question expert(s) via the plugin hook below.
        if !parts.is_empty()
            && let Some(ref sess) = session_info
            && sess.is_worker
            && let Some(ref pid) = sess.project_id
        {
            question_expert_feed = Some((pid.clone(), parts.join("\n")));
        }

        if has_more {
            format!(
                "{}\n\n**Note:** The user is still answering other worker questions. More answers may follow shortly. Continue working with what you have — do not ask the same questions again.",
                answers_text
            )
        } else {
            answers_text
        }
    };

    // Resolve references in the answer text (e.g. [session:id] from autocomplete)
    let answer_text = resolve_references(&answer_text, &state, None).await;

    // Notify question-expert plugins of the Q&A, under the answering user's
    // authority. Fire-and-forget: it must not delay the conversation resume,
    // and a plugin failure must not fail the answer.
    if let Some((project_id, qa_text)) = question_expert_feed {
        let plugins = state.plugins.clone();
        let asker_session_id = session_id.clone();
        let user_id = user_id.clone();
        tokio::spawn(async move {
            plugins
                .dispatch_authed(
                    crate::plugin::hooks::USER_ANSWER_HOOK,
                    &user_id,
                    serde_json::json!({
                        "asker_session_id": asker_session_id,
                        "project_id": project_id,
                        "qa_text": qa_text,
                    }),
                )
                .await;
        });
    }

    // Resume the conversation. With the long-lived stream-json
    // process we just append the user event and write the answer
    // to stdin via `send_or_queue` — if the agent is still mid-
    // turn (because another worker reply was streaming back), the
    // CLI buffers the user envelope and consumes it after the
    // current `result`. Spawned so the caller returns immediately.
    // A plugin question may redirect the answer to another session: the
    // pre-hatcher's clarifying question renders on the chat session, but
    // the answer must feed its temp research session — resuming the chat
    // agent with a bare answer would start the very turn the plugin is
    // still preparing. The target is read from the question event core
    // itself persisted (host-side), never from the client request.
    // The question event core persisted carries both the redirect target
    // and the plugin's correlation token (`approval_token`).
    let q_event_data: Option<serde_json::Value> = if question_id.is_empty() {
        None
    } else {
        state
            .db
            .get_event(question_id)
            .await
            .ok()
            .flatten()
            .and_then(|q| serde_json::from_str::<serde_json::Value>(&q.data).ok())
    };
    let redirect_target = q_event_data.as_ref().and_then(|d| {
        d.get("redirectSessionId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    });
    let approval_token = q_event_data
        .as_ref()
        .and_then(|d| d.get("approval_token").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();
    // The single selected option label (these plugin cards are one
    // question); empty when the user dismissed the card.
    let answer_label = event_data
        .get("answers")
        .and_then(|a| a.as_object())
        .and_then(|o| o.values().next())
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let state_clone = state.clone();
    let id_clone = redirect_target
        .clone()
        .unwrap_or_else(|| session_id.clone());
    let chat_id = session_id.clone();
    let hook_user_id = user_id;
    let redirect_for_hook = redirect_target.clone();
    let answer_rejected = rejected;
    tokio::spawn(async move {
        // A pre-hatcher question (opt-in / enriched-message approval) is
        // resolved in the plugin's CODE, not by resuming the cheap model:
        // fire the answer hook and, when the plugin owns the outcome
        // (delivered the message, or dispatched the read-only research
        // turn), skip resuming the temp agent with the raw answer. A
        // non-owning verdict (e.g. a clarifying-question continuation the
        // research agent must read) falls through to the normal resume.
        if let Some(ref temp_id) = redirect_for_hook {
            let is_pre_hatcher = state_clone
                .db
                .get_session(temp_id)
                .await
                .ok()
                .flatten()
                .and_then(|s| s.expert_kind)
                .as_deref()
                == Some(crate::service::mcp_server::PRE_HATCHER_EXPERT_KIND);
            if is_pre_hatcher {
                let chat_sess = state_clone.db.get_session(&chat_id).await.ok().flatten();
                let folder = chat_sess.as_ref().map(|s| s.folder_id.clone());
                let project = chat_sess.as_ref().and_then(|s| s.project_id.clone());
                let res = state_clone
                    .plugins
                    .dispatch_scoped(
                        crate::plugin::hooks::PREHATCH_ANSWER_HOOK,
                        &hook_user_id,
                        folder,
                        project,
                        Some(chat_id.clone()),
                        serde_json::json!({
                            "chat_session_id": chat_id,
                            "temp_session_id": temp_id,
                            "token": approval_token,
                            "answer": answer_label,
                            "rejected": answer_rejected,
                        }),
                    )
                    .await;
                if res.is_cancelled() {
                    return;
                }
            }
        }
        // Append the user event up front so the conversation log
        // reflects the typed order regardless of mid-turn vs. idle.
        if let Ok(user_ev) = state_clone
            .db
            .append_event(&id_clone, "user", serde_json::json!({"text": &answer_text}))
            .await
        {
            state_clone
                .broadcaster
                .broadcast(crate::ws::broadcaster::WsEvent {
                    event_type: "event".into(),
                    session_id: id_clone.clone(),
                    data: serde_json::json!({
                        "id": user_ev.id,
                        "seq": user_ev.seq,
                        "ts": user_ev.ts,
                        "kind": "user",
                        "data": {"text": &answer_text},
                    }),
                });
        }

        let session_project_id = state_clone
            .db
            .get_session(&id_clone)
            .await
            .ok()
            .flatten()
            .and_then(|s| s.project_id);
        let mcp_token = state_clone
            .mcp_tokens
            .issue_token(id_clone.clone(), session_project_id)
            .await;
        let mcp_config_path = crate::service::mcp_server::write_mcp_config(
            &state_clone.config.data_dir,
            &id_clone,
            state_clone.config.port,
            &mcp_token,
        )
        .ok()
        .map(|p| p.to_string_lossy().to_string());

        let config = SpawnConfig {
            model: "default".into(),
            effort: None,
            working_dir: String::new(),
            mcp_config_path,
            env: Default::default(),
            permission_mode: None, // host default: enforced unless the bypass setting is on
            timeout_ms: None,
            metadata: serde_json::Value::Null,
            system_prompt_suffix: None,
            system_prompt_override: None,
            // Populated in SessionManager::final_config from the plugin registry.
            extra_allowed_tools: Vec::new(),
            extra_disallowed_tools: Vec::new(),
            // Set from the session row in SessionManager::final_config.
            is_worker: false,
            is_pre_hatcher: false,
        };

        // Inject: the answer must reach the agent that asked — if its
        // turn is still winding down, a queued answer would stall the
        // conversation instead of resuming it. The user event was
        // appended above.
        if let Err(e) = state_clone
            .session_manager
            .send_or_queue(
                &id_clone,
                crate::provider::message::UserMessage::from_text(answer_text),
                &state_clone.db,
                &state_clone.broadcaster,
                config,
                crate::provider::manager::MidTurnPolicy::Inject,
                true,
            )
            .await
        {
            tracing::error!(session_id = %id_clone, "Failed to resume session with answer: {e}");
        }
    });

    Ok(event)
}

// ── ask_user card blocking ───────────────────────────────────────────
//
// A worker that asks the user is not making progress and must not be
// resumed until the answer lands. Two mechanisms, both required:
//
//   1. `card.blocked` with [`ASK_USER_BLOCK_REASON`] — keeps the card out
//      of the orchestrator's `available` filter and shows the human why
//      the card is parked.
//   2. The watchdog's stale-ref sweep skips cards whose worker session has
//      an unanswered question, so the card keeps its `worker_session_id`.
//
// Without (2), the 90s `ORPHAN_GRACE_SECS` sweep unassigns the card;
// answering then resumes the old session while the orchestrator spawns a
// second worker on the now-free card. Without (1), the unassigned card is
// respawned every orchestrator tick — a billed agent run and a duplicate
// question every couple of minutes, forever (the AskUser path appends no
// `NO_PROGRESS_KIND` event, so the no-progress backoff never engages).

/// `block_reason` written when a worker parks its card on `ask_user`.
/// Also the guard for un-blocking: a card blocked for any other reason
/// (money-loop defense, a human) is never unblocked by an answer.
pub const ASK_USER_BLOCK_REASON: &str = "Waiting for your answer to the worker's question";

/// True when `session_id` has at least one `question` event with no
/// matching `question-resolved`. Mirrors the resolution bookkeeping used
/// by `dismiss_pending_questions` and `/api/projects/:id/pending-questions`
/// (both `question_id` and `questionId` spellings).
pub async fn session_has_pending_question(db: &crate::db::Db, session_id: &str) -> bool {
    let events = match db.list_events_by_session(session_id, None).await {
        Ok(events) => events,
        Err(e) => {
            // Fail closed: an unreadable event log must not license a
            // respawn of a worker that may be waiting on the user.
            tracing::warn!(
                session_id = %session_id,
                "Failed to scan events for pending questions: {e}"
            );
            return true;
        }
    };

    let mut resolved: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut questions: Vec<&str> = Vec::new();
    let parsed: Vec<Option<serde_json::Value>> = events
        .iter()
        .map(|e| serde_json::from_str::<serde_json::Value>(&e.data).ok())
        .collect();
    for (ev, data) in events.iter().zip(parsed.iter()) {
        match ev.kind.as_str() {
            "question" => questions.push(ev.id.as_str()),
            "question-resolved" => {
                if let Some(qid) = data.as_ref().and_then(|d| {
                    d.get("question_id")
                        .or_else(|| d.get("questionId"))
                        .and_then(|v| v.as_str())
                }) {
                    resolved.insert(qid);
                }
            }
            _ => {}
        }
    }

    questions.iter().any(|qid| !resolved.contains(qid))
}

/// Park `card_id` on the user's answer: set `blocked` with
/// [`ASK_USER_BLOCK_REASON`] and broadcast the card update. No-op when the
/// card is already blocked — an existing block (money-loop defense, a
/// human) carries a reason we must not overwrite, and it already keeps the
/// card out of the orchestrator's `available` filter.
pub async fn block_card_for_question(
    db: &crate::db::Db,
    broadcaster: &crate::ws::broadcaster::Broadcaster,
    card_id: &str,
) {
    let card = match db.get_card(card_id).await {
        Ok(Some(card)) => card,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(card_id = %card_id, "ask_user: get_card failed: {e}");
            return;
        }
    };
    if card.blocked {
        return;
    }
    let update = crate::db::models::UpdateCard {
        blocked: Some(true),
        block_reason: Some(Some(ASK_USER_BLOCK_REASON.to_string())),
        updated_at: Some(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };
    match db.update_card(card_id, update).await {
        Ok(Some(updated)) => {
            tracing::info!(card_id = %card_id, "Card blocked while a worker question is unanswered");
            broadcast_card(broadcaster, &updated);
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(card_id = %card_id, "ask_user: failed to block card: {e}"),
    }
}

/// Release an [`ASK_USER_BLOCK_REASON`] block on the card owned by
/// `session_id`, once that session has no unanswered question left. Called
/// from every path that resolves a question (a real answer, a dismissal, a
/// superseding user message), so the card can never stay parked on a
/// question nobody is going to answer.
///
/// Guarded twice: only a block this module wrote is cleared, and only when
/// the last pending question is gone (a worker may ask several).
pub async fn clear_question_block(
    db: &crate::db::Db,
    broadcaster: &crate::ws::broadcaster::Broadcaster,
    session_id: &str,
) {
    let Ok(Some(session)) = db.get_session(session_id).await else {
        return;
    };
    let Some(card_id) = session.card_id else {
        return;
    };
    let card = match db.get_card(&card_id).await {
        Ok(Some(card)) => card,
        _ => return,
    };
    if !card.blocked || card.block_reason.as_deref() != Some(ASK_USER_BLOCK_REASON) {
        return;
    }
    if session_has_pending_question(db, session_id).await {
        return;
    }
    let update = crate::db::models::UpdateCard {
        blocked: Some(false),
        block_reason: Some(None),
        updated_at: Some(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };
    match db.update_card(&card_id, update).await {
        Ok(Some(updated)) => {
            tracing::info!(card_id = %card_id, "Card unblocked — worker question resolved");
            broadcast_card(broadcaster, &updated);
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(card_id = %card_id, "failed to unblock card after answer: {e}"),
    }
}

/// Live kanban update, same shape as the MCP card handlers emit.
fn broadcast_card(
    broadcaster: &crate::ws::broadcaster::Broadcaster,
    card: &crate::db::models::Card,
) {
    broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
        event_type: "card-update".into(),
        session_id: card.project_id.clone(),
        data: serde_json::json!({ "card": card }),
    });
}

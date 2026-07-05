//! Model-switch handover.
//!
//! A model id carries a `provider:model@account` shape. The pair
//! `(provider, account)` is a session's **continuity key**: as long as it
//! stays the same, the running provider can resume the same underlying
//! conversation (Claude via `--resume=<conversation_id>`, or mid-stream
//! stdin injection). The moment it changes — a different provider, or the
//! same provider under a different account — the incoming model spawns a
//! fresh child with no memory of anything said so far.
//!
//! To bridge that gap, the **outgoing** model writes a handover document
//! and the **incoming** model reads it on its first turn:
//!
//! 1. [`begin_handover`] — on a continuity-key-changing switch, park the
//!    target in `session.handover_to_model`, append a `handover-start`
//!    marker event, and dispatch a doc-generation turn to the *current*
//!    (outgoing) model. The session's stored `model` is left unchanged so
//!    that turn still routes to the outgoing provider/account.
//! 2. [`finalize_handover`] — when that turn completes **cleanly** (the
//!    process completion listener calls this), collect the outgoing model's
//!    text into the handover doc, record a `handover` event, flip
//!    into the handover doc, record a `handover` event, flip
//!    `session.model` to the target, and stash the doc in
//!    `session.pending_handover_doc`.
//! 3. [`take_pending_injection`] — the next user message under the new
//!    model consumes the doc and prepends it, so the incoming model opens
//!    with its predecessor's context.
//!
//! If the doc-generation turn instead fails or the user interrupts it, the
//! completion listener calls [`abort_handover`] rather than
//! [`finalize_handover`]: it clears the parked target and leaves `model` and
//! `conversation_id` untouched, so the switch simply doesn't happen and no
//! context is lost. The user can therefore cancel a handover mid-flight.
//!
//! **Compaction** reuses the same machinery with `from == to`: when a
//! **worker** session's context occupancy crosses
//! [`WORKER_COMPACT_CONTEXT_THRESHOLD`], the Claude stream loop recycles its
//! child after the turn and the completion listener calls
//! [`maybe_auto_compact`], which dispatches the compaction turn
//! automatically — no user prompt: the model writes a continuation doc for
//! itself and the conversation restarts fresh with the doc injected — same
//! model, same account, a fraction of the context. Workers additionally
//! require that their card would still resume this session (see the guards
//! on [`maybe_auto_compact`]). Interactive sessions are **never**
//! auto-compacted: the UI prompts the user to clear / compact / continue
//! instead. `POST /api/sessions/:id/compact` → [`begin_compaction`] is the
//! manual trigger, valid at any occupancy (it is what the UI's Compact
//! choice calls).

use std::sync::Arc;

use crate::db::models::UpdateSession;
use crate::provider::message::UserMessage;
use crate::provider::registry::{ProviderRegistry, split_model_account};
use crate::provider::stream::SpawnConfig;
use crate::state::AppState;
use crate::ws::broadcaster::WsEvent;

/// The default provider a bare (prefix-less) model id resolves to. Kept in
/// sync with `SessionManager`'s constant of the same name — bare ids are
/// legacy Claude sessions.
const DEFAULT_PROVIDER: &str = "claude";

/// Context-window occupancy (tokens) above which a **worker** session gets
/// auto-compacted. Checked against the latest `usage_events.context_tokens`
/// row — the real occupancy of the last API call, not the turn-sum.
/// Interactive sessions are never auto-compacted; the UI prompts the user
/// to clear / compact / continue instead (client-side, from ~150k). 200k
/// keeps unattended worker compaction rare while still leaving room for the
/// doc-generation turn itself.
pub const WORKER_COMPACT_CONTEXT_THRESHOLD: i64 = 200_000;

/// A session's continuity key: `(provider, account)`. Two model ids that
/// share a key can resume the same conversation; a differing key means the
/// incoming model starts cold and needs a handover.
pub fn continuity_key(model_id: &str) -> (String, Option<String>) {
    let (provider, rest) = ProviderRegistry::parse_model_id(model_id, DEFAULT_PROVIDER);
    let (_base, account) = split_model_account(&rest);
    (provider, account.map(str::to_string))
}

/// Does switching from `old` to `new` cross a provider/account boundary?
/// A plain model swap within the same provider+account returns `false` —
/// the existing resume path carries the context, no handover needed.
pub fn needs_handover(old: &str, new: &str) -> bool {
    continuity_key(old) != continuity_key(new)
}

/// Instruction handed to the outgoing model to produce the handover doc.
/// Deliberately provider-neutral and self-contained: the reader is a
/// *different* model with no shared memory, so the doc must stand alone.
fn handover_prompt(to_model: &str) -> String {
    format!(
        "You are about to hand this conversation off to a different AI model \
         ({to_model}), running under a different provider or account. It has \
         **no memory** of anything said here — the only thing it will receive \
         is the document you write now.\n\n\
         Write a HANDOVER document, in Markdown, so your successor can \
         continue seamlessly. Be concrete and self-contained. Cover:\n\n\
         1. **Goal** — what the user is ultimately trying to accomplish.\n\
         2. **Current state** — what has been done so far, what works, what \
         doesn't. Reference concrete files, functions, commands, and results.\n\
         3. **Key decisions & rationale** — choices made and why, so they \
         aren't relitigated or reversed by accident.\n\
         4. **Important context & constraints** — anything non-obvious your \
         successor must respect (conventions, gotchas, user preferences).\n\
         5. **Open threads** — unresolved questions and known issues.\n\
         6. **Next steps** — the concrete actions you'd take next.\n\n\
         Write ONLY the document — no preamble, no sign-off. Do not run tools \
         or make further changes; just summarize from what you already know."
    )
}

/// Instruction for a same-model compaction doc: the model summarizes for
/// ITSELF — the visible history is dropped and the doc is all that survives.
fn compaction_prompt() -> &'static str {
    "Your context window is nearly full, so this conversation is being \
     COMPACTED: the visible history will be dropped and you will continue \
     with only the document you write now. Write a CONTINUATION document, \
     in Markdown, for yourself. Be concrete and self-contained. Cover:\n\n\
     1. **Goal** — what the user is ultimately trying to accomplish.\n\
     2. **Current state** — what has been done so far, what works, what \
     doesn't. Reference concrete files, functions, commands, and results.\n\
     3. **Key decisions & rationale** — choices made and why, so they \
     aren't relitigated or reversed by accident.\n\
     4. **Important context & constraints** — anything non-obvious to \
     respect (conventions, gotchas, user preferences).\n\
     5. **Open threads** — unresolved questions and known issues.\n\
     6. **Next steps** — the concrete actions you'd take next.\n\n\
     Write ONLY the document — no preamble, no sign-off. Do not run tools \
     or make further changes; just summarize from what you already know."
}

/// Wrap `user_text` with the handover doc so the incoming model opens with
/// its predecessor's context ahead of the user's actual message.
pub fn build_injection(from_model: &str, doc: &str, user_text: &str) -> String {
    format!(
        "[Handover context — you are continuing a conversation previously \
         handled by a different model ({from_model}). You share no memory with \
         it; the document below is everything it chose to pass on. Treat it as \
         authoritative background, then respond to the user's message that \
         follows.]\n\n\
         <handover>\n{doc}\n</handover>\n\n\
         ---\n\nUser's message:\n{user_text}"
    )
}

/// Same-model variant of [`build_injection`] used after auto-compaction: the
/// reader IS the author, so frame the doc as its own preserved summary.
pub fn build_compaction_injection(doc: &str, user_text: &str) -> String {
    format!(
        "[Context compaction — this conversation's earlier history was \
         compacted to keep the context window small. The document below is \
         the continuation summary you wrote before the reset; treat it as \
         authoritative background, then respond to the user's message that \
         follows.]\n\n\
         <compaction>\n{doc}\n</compaction>\n\n\
         ---\n\nUser's message:\n{user_text}"
    )
}

/// Fallback doc when the outgoing model produced no usable text (e.g. its
/// generation turn crashed). The switch still completes so the user isn't
/// stranded on the old model.
const EMPTY_DOC_FALLBACK: &str = "(The previous model could not produce a handover document — its \
     generation turn ended without output. Ask the user to recap anything \
     you need.)";

/// Kick off a handover: dispatch a doc-generation turn to the *outgoing*
/// model and park the target model in `handover_to_model`. The session's
/// stored `model` is intentionally left unchanged here — [`finalize_handover`]
/// flips it once the doc is ready. Returns `Ok(())` once the turn is
/// dispatched; the finalize step runs later off the completion listener.
///
/// `lock`: pass the already-held session lock to dispatch immediately
/// without queueing (the auto-compaction path, which decides under the lock
/// that the session is idle and must not queue a doc turn behind a racing
/// dispatch); pass `None` to go through `send_or_queue` (the route paths,
/// where queueing behind an in-flight turn is the desired behaviour).
///
/// Preconditions (enforced by the callers): for a model switch, `from` and
/// `to` cross a continuity boundary and the session has real history to
/// summarize; for a compaction, `from == to`.
pub async fn begin_handover(
    state: &Arc<AppState>,
    session_id: &str,
    from_model: &str,
    to_model: &str,
    lock: Option<&crate::provider::manager::SessionLock>,
) -> anyhow::Result<()> {
    // Park the target. Leaving `model` alone keeps the doc-gen turn routed
    // to the outgoing provider/account so it can resume the conversation.
    state
        .db
        .update_session(
            session_id,
            UpdateSession {
                handover_to_model: Some(Some(to_model.to_string())),
                ..Default::default()
            },
        )
        .await?;

    // Visible marker that also bounds the text scan in `finalize_handover`
    // to exactly this turn's output.
    let start_data = serde_json::json!({
        "from": from_model,
        "to": to_model,
        "compaction": from_model == to_model,
    });
    if let Ok(ev) = state
        .db
        .append_event(session_id, "handover-start", start_data.clone())
        .await
    {
        state.broadcaster.broadcast(WsEvent {
            event_type: "event".into(),
            session_id: session_id.to_string(),
            data: serde_json::json!({
                "id": ev.id,
                "seq": ev.seq,
                "ts": ev.ts,
                "kind": ev.kind,
                "data": start_data,
            }),
        });
    }

    // Dispatch the doc-generation turn on the OUTGOING model. No MCP config
    // and no attachments — the model summarizes from conversation context
    // alone; it must not go make further changes.
    let config = SpawnConfig {
        model: from_model.to_string(),
        effort: None,
        working_dir: String::new(),
        mcp_config_path: None,
        env: Default::default(),
        permission_mode: Some("bypass".into()),
        timeout_ms: None,
        metadata: serde_json::Value::Null,
        system_prompt_suffix: None,
        system_prompt_override: None,
        // Populated in SessionManager::final_config from the plugin registry.
        extra_allowed_tools: Vec::new(),
        // Set from the session row in SessionManager::final_config.
        is_worker: false,
    };

    let message = UserMessage::from_text(if from_model == to_model {
        compaction_prompt().to_string()
    } else {
        handover_prompt(to_model)
    });
    match lock {
        Some(lock) => {
            state
                .session_manager
                .send_message_locked(lock, message, &state.db, &state.broadcaster, config)
                .await?;
        }
        None => {
            state
                .session_manager
                .send_or_queue(session_id, message, &state.db, &state.broadcaster, config)
                .await?;
        }
    }

    // Ask the outgoing provider to exit once the doc turn's result lands.
    // Load-bearing, twice over:
    //
    // - Mid-stream providers (Claude) keep one long-lived child per session
    //   and deliver a `ProcessCompletion` only when that child EXITS — not
    //   at end of turn. The completion listener that calls
    //   `finalize_handover` would otherwise not fire until the 30-minute
    //   idle reaper recycles the child, leaving the session stuck in
    //   "handover in progress" (composer locked, sends 409ing) the whole
    //   time.
    // - The old child is authenticated as the OUTGOING provider/account and
    //   can never serve a turn after the switch. If it stayed alive, the
    //   provider's run map would still hold it and the incoming model's
    //   first message would be written to the stale child's stdin.
    //
    // The shutdown request rides the same FIFO stdin channel as the doc
    // turn just dispatched, so it cannot overtake it: the stream loop marks
    // the doc turn active, then records the shutdown, then exits right
    // after the doc turn's result. Default no-op for per-turn providers
    // (mock/ollama/grok/cursor), which already deliver a completion after
    // every turn.
    crate::provider::manager::shutdown_after_turn_via_registry(
        &state.provider_registry,
        session_id,
    )
    .await;

    Ok(())
}

/// Auto-compaction: a same-model handover dispatched automatically when a
/// **worker** session's context occupancy has crossed
/// [`WORKER_COMPACT_CONTEXT_THRESHOLD`]. Called by the completion listener
/// after a normal (non-handover) completion — the natural idle gap right
/// after a turn, while the prompt prefix is still cache-warm. Returns
/// whether a compaction turn was dispatched.
///
/// Every guard is a conservative skip — occupancy only grows, so a skipped
/// check simply retries at the next completion:
/// - interactive sessions never auto-compact — the UI prompts the user to
///   clear / compact / continue; only workers compact unattended;
/// - no handover in flight and no doc still waiting to inject;
/// - not an expert or repeating-task session (their dispatchers own the
///   session lifecycle and aren't audited for a doc turn interleaving);
/// - no queued message (the listener's drain right after this check would
///   deliver it into the middle of the doc turn);
/// - only while the card would still resume THIS session (same step,
///   currently unclaimed, non-terminal, resumable conversation) — otherwise
///   the doc would summarize a conversation nothing reads;
/// - idle under the session lock — losing the race to a concurrent
///   dispatch (e.g. the orchestrator tick resuming the card) means that
///   turn runs first and we compact after it instead.
pub async fn maybe_auto_compact(state: &Arc<AppState>, session_id: &str) -> anyhow::Result<bool> {
    let Some(session) = state.db.get_session(session_id).await? else {
        return Ok(false);
    };
    // Interactive sessions are never auto-compacted — the UI prompts the
    // user (clear / compact / continue). Only workers compact unattended.
    if !session.is_worker {
        return Ok(false);
    }
    if session.handover_to_model.is_some()
        || session.pending_handover_doc.is_some()
        || session.is_expert
        || session.repeating_task_id.is_some()
    {
        return Ok(false);
    }
    let occupancy = state
        .db
        .latest_context_tokens(session_id)
        .await
        .unwrap_or(None)
        .unwrap_or(0);
    if occupancy < WORKER_COMPACT_CONTEXT_THRESHOLD {
        return Ok(false);
    }
    if matches!(state.db.get_queued_message(session_id).await, Ok(Some(_))) {
        return Ok(false);
    }
    if !card_resumes_session(state, &session).await {
        return Ok(false);
    }
    let model = session.model.clone().unwrap_or_else(|| "default".into());
    let lock = state.session_manager.lock_session(session_id).await;
    if state.session_manager.is_running(session_id).await {
        return Ok(false);
    }
    tracing::info!(
        session_id = %session_id,
        occupancy,
        is_worker = session.is_worker,
        "Context occupancy over compaction threshold; auto-compacting"
    );
    begin_handover(state, session_id, &model, &model, Some(&lock)).await?;
    Ok(true)
}

/// Would the worker orchestrator resume `session` for its card's next
/// chunk? Mirrors the resume filter in `spawn_worker_for_card`: same card,
/// same step, currently unclaimed, non-terminal, and a conversation to
/// resume. Only then is a compaction doc worth writing — the resumed chunk
/// is what reads it.
async fn card_resumes_session(state: &Arc<AppState>, session: &crate::db::models::Session) -> bool {
    if session.conversation_id.is_none() {
        return false;
    }
    let Some(card_id) = session.card_id.as_deref() else {
        return false;
    };
    let Ok(Some(card)) = state.db.get_card(card_id).await else {
        return false;
    };
    card.worker_session_id.is_none()
        && card.last_worker_session_id.as_deref() == Some(session.id.as_str())
        && session.worker_step.as_deref() == Some(card.step.as_str())
        && card.step != "done"
        && card.step != "wont_do"
}

/// Manual compaction (`POST /api/sessions/:id/compact`), valid at any
/// occupancy. Same-model handover: the model writes a continuation doc, the
/// conversation restarts fresh with the doc injected. Errors describe why
/// the session is ineligible. Interactive sessions only — workers compact
/// automatically between chunks via [`maybe_auto_compact`], where the card
/// resume-link eligibility is checked.
pub async fn begin_compaction(state: &Arc<AppState>, session_id: &str) -> anyhow::Result<()> {
    let Some(session) = state.db.get_session(session_id).await? else {
        anyhow::bail!("session not found");
    };
    if session.is_worker {
        anyhow::bail!("worker sessions compact automatically between chunks");
    }
    if session.handover_to_model.is_some() {
        anyhow::bail!("a handover or compaction is already in progress");
    }
    if session.pending_handover_doc.is_some() {
        anyhow::bail!("a handover/compaction doc is still waiting to be delivered");
    }
    let occupancy = state
        .db
        .latest_context_tokens(session_id)
        .await
        .unwrap_or(None)
        .unwrap_or(0);
    if occupancy == 0 {
        anyhow::bail!("nothing to compact yet — the session has no recorded context");
    }
    let model = session.model.clone().unwrap_or_else(|| "default".into());
    tracing::info!(session_id = %session_id, occupancy, "Manual compaction dispatching");
    begin_handover(state, session_id, &model, &model, None).await
}

/// Complete a handover after the outgoing model's doc-generation turn
/// finishes. Collects that turn's `agent-text` into the doc, records a
/// `handover` event, flips `session.model` to the parked target, and stashes
/// the doc for the incoming model's first turn.
///
/// Idempotent-ish: if `handover_to_model` is already clear (no pending
/// handover), it returns without touching anything, so a spurious completion
/// can't double-fire.
pub async fn finalize_handover(state: &Arc<AppState>, session_id: &str) -> anyhow::Result<()> {
    let session = match state.db.get_session(session_id).await? {
        Some(s) => s,
        None => return Ok(()),
    };
    let to_model = match session.handover_to_model {
        Some(m) => m,
        None => return Ok(()), // no handover in flight
    };
    let from_model = session.model.clone().unwrap_or_default();

    let doc = collect_handover_doc(state, session_id).await;
    if doc.trim().is_empty() && from_model == to_model {
        // A compaction that produced no summary must NOT finalize:
        // dropping the conversation with nothing to inject would destroy
        // the very context the compaction was meant to preserve. Roll it
        // back instead — the session keeps its model and full history,
        // and the user can retry (or log in again) and compact later.
        tracing::warn!(
            session_id = %session_id,
            "Compaction doc turn produced no text; aborting instead of finalizing"
        );
        return abort_handover(
            state,
            session_id,
            Some("the model produced no compaction summary"),
        )
        .await;
    }
    let doc = if doc.trim().is_empty() {
        EMPTY_DOC_FALLBACK.to_string()
    } else {
        doc
    };

    // Record the finished doc as a visible, durable event.
    let handover_data = serde_json::json!({
        "from": from_model,
        "to": to_model,
        "doc": doc,
        "compaction": from_model == to_model,
    });
    if let Ok(ev) = state
        .db
        .append_event(session_id, "handover", handover_data.clone())
        .await
    {
        state.broadcaster.broadcast(WsEvent {
            event_type: "event".into(),
            session_id: session_id.to_string(),
            data: serde_json::json!({
                "id": ev.id,
                "seq": ev.seq,
                "ts": ev.ts,
                "kind": ev.kind,
                "data": handover_data,
            }),
        });
    }

    // Flip to the new model, clear the in-flight flag, stash the doc for
    // injection. Also drop any stale conversation_id — the incoming
    // provider/account can't resume the outgoing one's conversation — and
    // stamp `context_reset_ts`: the doc-generation turn just recorded a
    // full-context usage row, and without the stamp the badge (and the
    // worker auto-compaction check) would keep reporting the discarded
    // conversation's occupancy until the next turn lands.
    let updated = state
        .db
        .update_session(
            session_id,
            UpdateSession {
                model: Some(Some(to_model.clone())),
                conversation_id: Some(None),
                handover_to_model: Some(None),
                pending_handover_doc: Some(Some(doc)),
                context_reset_ts: Some(Some(chrono::Utc::now().timestamp_millis())),
                ..Default::default()
            },
        )
        .await?;

    // Tell every connected client the switch landed so the model label and
    // composer state update without a manual refetch.
    if let Some(s) = updated {
        state.broadcaster.broadcast(WsEvent {
            event_type: "session-updated".into(),
            session_id: session_id.to_string(),
            data: serde_json::to_value(&s).unwrap_or(serde_json::Value::Null),
        });
    }

    tracing::info!(
        session_id = %session_id,
        from = %from_model,
        to = %to_model,
        "Model-switch handover finalized"
    );
    Ok(())
}

/// Abort an in-flight handover WITHOUT switching models. Called when the
/// outgoing model's doc-generation turn failed (crashed) or the user
/// interrupted it: clearing only `handover_to_model` leaves `model` and
/// `conversation_id` untouched, so the outgoing model resumes with its full
/// context on the next turn — nothing is lost. This is the load-bearing
/// half of "don't switch if the switch fails": [`finalize_handover`] (which
/// flips the model and drops the conversation) runs only on a *clean* doc
/// turn; every other outcome lands here. Records a `handover-aborted` marker
/// so the transcript shows the switch didn't happen.
///
/// No-op if no handover is parked, so a spurious completion can't misfire.
///
/// `reason`: why the doc turn failed (the provider-reported error, e.g. a
/// 401 from an expired login), recorded on the `handover-aborted` event so
/// the UI can tell the user what went wrong — and, for a compaction, offer
/// the way out (log in again, or clear the session and lose the context).
/// `None` for a user-initiated interrupt.
pub async fn abort_handover(
    state: &Arc<AppState>,
    session_id: &str,
    reason: Option<&str>,
) -> anyhow::Result<()> {
    let Some(session) = state.db.get_session(session_id).await? else {
        return Ok(());
    };
    let Some(to_model) = session.handover_to_model.clone() else {
        return Ok(()); // no handover in flight
    };
    let from_model = session.model.clone().unwrap_or_default();

    let data = serde_json::json!({
        "from": from_model,
        "to": to_model,
        "compaction": from_model == to_model,
        "reason": reason,
    });
    if let Ok(ev) = state
        .db
        .append_event(session_id, "handover-aborted", data.clone())
        .await
    {
        state.broadcaster.broadcast(WsEvent {
            event_type: "event".into(),
            session_id: session_id.to_string(),
            data: serde_json::json!({
                "id": ev.id,
                "seq": ev.seq,
                "ts": ev.ts,
                "kind": ev.kind,
                "data": data,
            }),
        });
    }

    // Clear ONLY the parked target. Deliberately leave `model` and
    // `conversation_id` as they were so the next turn resumes the original
    // conversation on the original model — the whole point of aborting
    // instead of finalizing.
    let updated = state
        .db
        .update_session(
            session_id,
            UpdateSession {
                handover_to_model: Some(None),
                ..Default::default()
            },
        )
        .await?;

    if let Some(s) = updated {
        state.broadcaster.broadcast(WsEvent {
            event_type: "session-updated".into(),
            session_id: session_id.to_string(),
            data: serde_json::to_value(&s).unwrap_or(serde_json::Value::Null),
        });
    }

    tracing::info!(
        session_id = %session_id,
        from = %from_model,
        to = %to_model,
        "Handover aborted — model and context left unchanged"
    );
    Ok(())
}

/// Concatenate the `agent-text` the outgoing model emitted during the
/// doc-generation turn — i.e. every text event after the most recent
/// `handover-start` marker.
async fn collect_handover_doc(state: &Arc<AppState>, session_id: &str) -> String {
    // 500 comfortably covers one turn's worth of text/tool events even for
    // a verbose doc; the scan stops at the marker anyway.
    match state.db.events_tail(session_id, 500).await {
        Ok(events) => extract_doc(&events),
        Err(_) => String::new(),
    }
}

/// Join the `agent-text` events that follow the most recent `handover-start`
/// marker, in order. This is the outgoing model's doc-generation turn, and
/// nothing before the marker (the prior conversation) should leak into the
/// doc. Pure so it can be unit-tested against synthetic event tails.
pub(crate) fn extract_doc(events: &[crate::db::models::Event]) -> String {
    let start_idx = events
        .iter()
        .rposition(|e| e.kind == "handover-start")
        .map(|i| i + 1)
        .unwrap_or(0);

    let mut parts: Vec<String> = Vec::new();
    for ev in &events[start_idx..] {
        if ev.kind != "agent-text" {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&ev.data)
            && let Some(t) = v.get("text").and_then(|t| t.as_str())
        {
            parts.push(t.to_string());
        }
    }
    parts.join("")
}

/// If a finalized handover left a doc waiting, consume it (clearing the
/// column) and return the injection-wrapped message. Otherwise return
/// `text` unchanged. Called from `send_message_locked` — the single dispatch
/// chokepoint — so the HTTP route, the queue drain, and every other path
/// inject consistently. Compactions (same-model handovers) get the
/// compaction wording; real model switches get the predecessor's label,
/// read back from the most recent `handover` event.
pub async fn take_pending_injection(db: &crate::db::Db, session_id: &str, text: &str) -> String {
    let session = match db.get_session(session_id).await {
        Ok(Some(s)) => s,
        _ => return text.to_string(),
    };
    let doc = match session.pending_handover_doc {
        Some(d) if !d.trim().is_empty() => d,
        _ => return text.to_string(),
    };

    // Consume it so it's injected exactly once.
    let _ = db
        .update_session(
            session_id,
            UpdateSession {
                pending_handover_doc: Some(None),
                ..Default::default()
            },
        )
        .await;

    match latest_handover_meta(db, session_id).await {
        Some((_, true)) => build_compaction_injection(&doc, text),
        Some((from, false)) => build_injection(&from, &doc, text),
        None => build_injection("a previous model", &doc, text),
    }
}

/// `(from_model, is_compaction)` of the most recent `handover` event, if any.
async fn latest_handover_meta(db: &crate::db::Db, session_id: &str) -> Option<(String, bool)> {
    let events = db.events_tail(session_id, 200).await.ok()?;
    events.iter().rev().find_map(|e| {
        if e.kind != "handover" {
            return None;
        }
        let v = serde_json::from_str::<serde_json::Value>(&e.data).ok()?;
        let from = v.get("from")?.as_str()?.to_string();
        let compaction = v
            .get("compaction")
            .and_then(|c| c.as_bool())
            .unwrap_or_else(|| v.get("to").and_then(|t| t.as_str()) == Some(from.as_str()));
        Some((from, compaction))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuity_key_parses_provider_and_account() {
        assert_eq!(continuity_key("claude:opus"), ("claude".into(), None));
        assert_eq!(
            continuity_key("claude:opus@acc_1"),
            ("claude".into(), Some("acc_1".into()))
        );
        // Bare (legacy) ids default to the claude provider.
        assert_eq!(continuity_key("opus"), ("claude".into(), None));
        assert_eq!(continuity_key("grok:grok-4"), ("grok".into(), None));
    }

    #[test]
    fn needs_handover_only_on_provider_or_account_change() {
        // Same provider + account, different model → no handover (resume works).
        assert!(!needs_handover("claude:opus", "claude:sonnet"));
        // Bare id vs explicit claude prefix → same key, no handover.
        assert!(!needs_handover("opus", "claude:opus"));
        // Different provider → handover.
        assert!(needs_handover("claude:opus", "grok:grok-4"));
        // Same provider, different account → handover.
        assert!(needs_handover("claude:opus@acc_1", "claude:opus@acc_2"));
        // Account added where there was none → handover.
        assert!(needs_handover("claude:opus", "claude:opus@acc_2"));
    }

    fn ev(seq: i32, kind: &str, data: serde_json::Value) -> crate::db::models::Event {
        crate::db::models::Event {
            id: format!("e{seq}"),
            session_id: "s1".into(),
            seq,
            ts: seq as i64,
            kind: kind.into(),
            data: data.to_string(),
        }
    }

    #[test]
    fn extract_doc_joins_only_text_after_last_marker() {
        let events = vec![
            // Prior conversation — must NOT leak into the doc.
            ev(1, "user", serde_json::json!({ "text": "hi" })),
            ev(2, "agent-text", serde_json::json!({ "text": "old reply" })),
            // The doc-generation turn.
            ev(
                3,
                "handover-start",
                serde_json::json!({ "from": "a", "to": "b" }),
            ),
            ev(4, "agent-text", serde_json::json!({ "text": "## Goal\n" })),
            ev(5, "agent-tool-start", serde_json::json!({ "name": "Bash" })),
            ev(
                6,
                "agent-text",
                serde_json::json!({ "text": "do the thing" }),
            ),
            ev(7, "agent-end", serde_json::json!({ "status": "complete" })),
        ];
        assert_eq!(extract_doc(&events), "## Goal\ndo the thing");
    }

    #[test]
    fn extract_doc_empty_when_no_text_in_turn() {
        let events = vec![
            ev(
                1,
                "handover-start",
                serde_json::json!({ "from": "a", "to": "b" }),
            ),
            ev(2, "agent-end", serde_json::json!({ "status": "crashed" })),
        ];
        assert_eq!(extract_doc(&events), "");
    }

    #[test]
    fn build_injection_wraps_doc_and_message() {
        let out = build_injection("claude:opus", "the doc body", "do the thing");
        assert!(out.contains("<handover>\nthe doc body\n</handover>"));
        assert!(out.contains("do the thing"));
        assert!(out.contains("claude:opus"));
    }

    #[test]
    fn build_compaction_injection_wraps_doc_and_message() {
        let out = build_compaction_injection("the doc body", "do the thing");
        assert!(out.contains("<compaction>\nthe doc body\n</compaction>"));
        assert!(out.contains("do the thing"));
    }
}

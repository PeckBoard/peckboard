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
//! **Recovery** is the other cross-boundary path, for when the outgoing
//! agent *cannot* write that doc — typically the account has hit a usage
//! limit. [`begin_recovery`] never talks to the current provider: it
//! reconstructs the user/assistant transcript from the event log, parks it
//! as `pending_handover_doc`, and flips `model` immediately. The incoming
//! model reads the full transcript on its first turn. That first turn is
//! billed as one large input, so [`recovery_preview`] estimates the token
//! count and input-token cost **before** the user confirms.
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

use crate::db::models::{Event, UpdateSession};
use crate::provider::message::UserMessage;
use crate::provider::registry::{ProviderRegistry, split_model_account};
use crate::provider::stream::SpawnConfig;
use crate::routes::usage::cost::{TokenKind, token_cost};
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

/// Recovery variant: the previous account/provider could not continue, so
/// the incoming model gets the reconstructed transcript rather than a
/// summary the outgoing model wrote. Distinct wording so the reader knows
/// this is the raw history, not a curated handover.
pub fn build_recovery_injection(from_model: &str, transcript: &str, user_text: &str) -> String {
    format!(
        "[Recovery context — you are continuing a conversation previously \
         handled by a different model ({from_model}) whose account or provider \
         could not continue (typically a usage limit). You share no memory \
         with it. The transcript below is the full user/assistant history, \
         reconstructed without that model's help. Treat it as authoritative \
         background, then respond to the user's message that follows.]\n\n\
         <transcript>\n{transcript}\n</transcript>\n\n\
         ---\n\nUser's message:\n{user_text}"
    )
}

/// ~4 Unicode chars per token — the usual estimate without a model-specific
/// tokenizer. Round up so the preview never undersells the first-turn bill.
/// Empty input is 0, not 1.
pub fn estimate_tokens(text: &str) -> i64 {
    let n = text.chars().count() as i64;
    if n == 0 { 0 } else { (n + 3) / 4 }
}

/// Usable context-window size (tokens) for a model id. Matches the
/// frontend's `contextWindowInfo`: `[1m]` aliases are 1M, everything else
/// is the 200K default. Recovery uses this only as a `fits` check — a miss
/// is labelled as an estimate, not a hard provider limit.
pub fn context_window_for(model: &str) -> i64 {
    let (model, _acct) = split_model_account(model);
    let id = model.rsplit(':').next().unwrap_or(model);
    if id.ends_with("[1m]") {
        1_000_000
    } else {
        200_000
    }
}

/// Pull `data.text` out of an event's JSON payload.
fn event_text(data: &str) -> String {
    serde_json::from_str::<serde_json::Value>(data)
        .ok()
        .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(str::to_string))
        .unwrap_or_default()
}

/// Reconstruct the current conversation as Markdown the incoming model can
/// read: user messages and concatenated assistant text, plus tool names as
/// one-liners (inputs/outputs are dropped — they are the usual token bomb).
///
/// Starts after the most recent `handover` event when one exists, so a
/// compacted session sends the continuation doc + later turns rather than
/// the discarded pre-compaction history.
pub fn build_recovery_transcript(events: &[Event]) -> String {
    let last_handover = events.iter().rposition(|e| e.kind == "handover");
    let (prior_doc, rest) = match last_handover {
        Some(i) => {
            let doc = serde_json::from_str::<serde_json::Value>(&events[i].data)
                .ok()
                .and_then(|v| {
                    v.get("doc")
                        .and_then(|d| d.as_str())
                        .map(|s| s.trim().to_string())
                })
                .filter(|s| !s.is_empty());
            (doc, &events[i + 1..])
        }
        None => (None, events),
    };

    let mut out = String::new();
    let mut assistant = String::new();

    let flush_assistant = |out: &mut String, assistant: &mut String| {
        if assistant.is_empty() {
            return;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("## Assistant\n\n");
        out.push_str(assistant.trim_end());
        assistant.clear();
    };

    for ev in rest {
        match ev.kind.as_str() {
            "user" => {
                let text = event_text(&ev.data);
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                flush_assistant(&mut out, &mut assistant);
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str("## User\n\n");
                out.push_str(text);
            }
            "agent-text" => {
                assistant.push_str(&event_text(&ev.data));
            }
            "agent-tool-start" => {
                let name = serde_json::from_str::<serde_json::Value>(&ev.data)
                    .ok()
                    .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_string))
                    .unwrap_or_else(|| "tool".into());
                // Strip the mcp__plugin__ prefix the same way the chat UI does.
                let bare = name
                    .strip_prefix("mcp__")
                    .and_then(|s| s.split_once("__").map(|(_, rest)| rest))
                    .unwrap_or(&name);
                assistant.push_str(&format!("\n- `{bare}`"));
            }
            "agent-end" | "handover-start" | "handover" | "handover-aborted" => {
                flush_assistant(&mut out, &mut assistant);
            }
            _ => {}
        }
    }
    flush_assistant(&mut out, &mut assistant);

    match prior_doc {
        Some(doc) if !out.is_empty() => format!("## Prior context\n\n{doc}\n\n{out}"),
        Some(doc) => format!("## Prior context\n\n{doc}"),
        None => out,
    }
}

/// Token/cost preview for a recovery switch. `tokens` is the input the
/// incoming model will see on its first turn (transcript + recovery
/// wrapper), not including the user's next message. `est_cost_usd` prices
/// that as **input** tokens at the target model's rate — the first-turn
/// bill, billed to the NEW account.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RecoveryPreview {
    pub tokens: i64,
    pub chars: i64,
    pub est_cost_usd: f64,
    pub context_window: i64,
    pub fits: bool,
    pub from_model: String,
    pub to_model: String,
}

/// Why a recovery switch can't run. The route maps these to 4xx; the
/// strings are user-facing.
#[derive(Debug)]
pub enum RecoveryError {
    NotFound,
    SameContinuity,
    Worker,
    HandoverInFlight,
    MidTurn,
    NoHistory,
    TooLarge { tokens: i64, window: i64 },
    Internal(anyhow::Error),
}

impl std::fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "session not found"),
            Self::SameContinuity => write!(
                f,
                "target model is the same provider and account; recovery is only for switching accounts or providers"
            ),
            Self::Worker => write!(
                f,
                "worker sessions can only switch models within the same provider and account; set the card or project model to move future workers elsewhere"
            ),
            Self::HandoverInFlight => write!(
                f,
                "model handover in progress; wait for it to finish before switching again"
            ),
            Self::MidTurn => write!(
                f,
                "agent is mid-turn; wait for it to finish before switching provider or account"
            ),
            Self::NoHistory => {
                write!(
                    f,
                    "nothing to recover — this session has no conversation yet"
                )
            }
            Self::TooLarge { tokens, window } => write!(
                f,
                "transcript is ~{tokens} tokens, which exceeds the new model's ~{window}-token context window"
            ),
            Self::Internal(e) => write!(f, "{e}"),
        }
    }
}

impl From<anyhow::Error> for RecoveryError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}

/// Is a turn actively streaming? True when the latest `agent-start` has no
/// `agent-end` after it — same signal the PATCH handover guard uses.
fn recovery_turn_in_flight(events: &[Event]) -> bool {
    let last_start = events.iter().rposition(|e| e.kind == "agent-start");
    let last_end = events.iter().rposition(|e| e.kind == "agent-end");
    match (last_start, last_end) {
        (Some(s), Some(e)) => s > e,
        (Some(_), None) => true,
        _ => false,
    }
}

struct PreparedRecovery {
    from_model: String,
    transcript: String,
    preview: RecoveryPreview,
}

/// Shared load + guard + transcript build for preview and execute.
/// `require_idle` is true for the POST (mid-turn is a 409) and false for
/// the GET so the dialog can show the cost while a turn is still finishing.
async fn prepare_recovery(
    state: &AppState,
    session_id: &str,
    to_model: &str,
    require_idle: bool,
) -> Result<PreparedRecovery, RecoveryError> {
    let session = state
        .db
        .get_session(session_id)
        .await
        .map_err(RecoveryError::from)?
        .ok_or(RecoveryError::NotFound)?;

    if session.handover_to_model.is_some() {
        return Err(RecoveryError::HandoverInFlight);
    }
    if session.is_worker {
        return Err(RecoveryError::Worker);
    }

    let from_model = session.model.clone().unwrap_or_else(|| "default".into());
    if !needs_handover(&from_model, to_model) {
        return Err(RecoveryError::SameContinuity);
    }

    let events = state
        .db
        .list_events_by_session(session_id, None)
        .await
        .map_err(RecoveryError::from)?;

    if require_idle && recovery_turn_in_flight(&events) {
        return Err(RecoveryError::MidTurn);
    }

    let has_history = events
        .iter()
        .any(|e| e.kind == "agent-text" || e.kind == "agent-start" || e.kind == "user");
    if !has_history {
        return Err(RecoveryError::NoHistory);
    }

    let transcript = build_recovery_transcript(&events);
    if transcript.trim().is_empty() {
        return Err(RecoveryError::NoHistory);
    }

    // Count the bytes the incoming model will actually see: wrapper +
    // transcript. The user's next message is extra and is not in this figure.
    let injected = build_recovery_injection(&from_model, &transcript, "");
    let chars = injected.chars().count() as i64;
    let tokens = estimate_tokens(&injected);
    let context_window = context_window_for(to_model);
    let preview = RecoveryPreview {
        tokens,
        chars,
        est_cost_usd: token_cost(Some(to_model), TokenKind::Input, tokens),
        context_window,
        fits: tokens <= context_window,
        from_model: from_model.clone(),
        to_model: to_model.to_string(),
    };
    Ok(PreparedRecovery {
        from_model,
        transcript,
        preview,
    })
}

/// Token/cost preview for a recovery switch. Does not mutate the session.
pub async fn recovery_preview(
    state: &AppState,
    session_id: &str,
    to_model: &str,
) -> Result<RecoveryPreview, RecoveryError> {
    Ok(prepare_recovery(state, session_id, to_model, false)
        .await?
        .preview)
}

/// Switch provider/account by sending the reconstructed transcript to the
/// incoming model — **without** asking the outgoing agent to write a
/// handover doc. Flips `model` immediately, clears `conversation_id`, and
/// parks the transcript in `pending_handover_doc` for the next turn.
pub async fn begin_recovery(
    state: &Arc<AppState>,
    session_id: &str,
    to_model: &str,
    effort: Option<Option<String>>,
) -> Result<crate::db::models::Session, RecoveryError> {
    let prepared = prepare_recovery(state, session_id, to_model, true).await?;
    if !prepared.preview.fits {
        return Err(RecoveryError::TooLarge {
            tokens: prepared.preview.tokens,
            window: prepared.preview.context_window,
        });
    }

    // Kill the outgoing child now. There is no doc turn to wait for, and
    // a live child in the run map would swallow the incoming model's first
    // message on the old account's stdin (see begin_handover's shutdown
    // comment). cancel_and_wait so a late agent-end from the dying process
    // can't land after we flip the row.
    state.session_manager.cancel_and_wait(session_id).await;

    let handover_data = serde_json::json!({
        "from": prepared.from_model,
        "to": to_model,
        "doc": prepared.transcript,
        "compaction": false,
        "recovery": true,
        "tokens": prepared.preview.tokens,
        "est_cost_usd": prepared.preview.est_cost_usd,
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

    let updated = state
        .db
        .update_session(
            session_id,
            UpdateSession {
                model: Some(Some(to_model.to_string())),
                effort,
                conversation_id: Some(None),
                handover_to_model: Some(None),
                handover_run_id: Some(None),
                pending_handover_doc: Some(Some(prepared.transcript)),
                context_reset_ts: Some(Some(chrono::Utc::now().timestamp_millis())),
                ..Default::default()
            },
        )
        .await
        .map_err(RecoveryError::from)?
        .ok_or(RecoveryError::NotFound)?;

    state.broadcaster.broadcast(WsEvent {
        event_type: "session-updated".into(),
        session_id: session_id.to_string(),
        data: serde_json::to_value(&updated).unwrap_or(serde_json::Value::Null),
    });

    tracing::info!(
        session_id = %session_id,
        from = %prepared.from_model,
        to = %to_model,
        tokens = prepared.preview.tokens,
        "Recovery switch: transcript parked, outgoing agent unused"
    );
    Ok(updated)
}

/// Fallback doc when the outgoing model produced no usable text (e.g. its
/// generation turn crashed). The switch still completes so the user isn't
/// stranded on the old model.
const EMPTY_DOC_FALLBACK: &str = "(The previous model could not produce a handover document — its \
     generation turn ended without output. Ask the user to recap anything \
     you need.)";

/// The dispatch shape of a handover doc-generation turn: the OUTGOING
/// model, no MCP config and no attachments — the model summarizes from
/// conversation context alone; it must not go make further changes.
fn doc_turn_config(from_model: &str) -> SpawnConfig {
    SpawnConfig {
        model: from_model.to_string(),
        effort: None,
        working_dir: String::new(),
        mcp_config_path: None,
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
    }
}

/// The doc-generation prompt: a compaction when the model isn't changing,
/// a cross-provider handover otherwise.
fn doc_turn_message(from_model: &str, to_model: &str) -> UserMessage {
    UserMessage::from_text(if from_model == to_model {
        compaction_prompt().to_string()
    } else {
        handover_prompt(to_model)
    })
}

/// Record the `handover-start` marker. Visible in the transcript, and it
/// bounds `extract_doc`'s scan in `finalize_handover` to exactly the doc
/// turn's output — so it is written immediately BEFORE that turn is
/// dispatched, never when the handover is merely parked. A deferred doc
/// turn marked at park time would sweep up the rest of the live turn's
/// text as its "doc".
async fn append_handover_start(
    state: &Arc<AppState>,
    session_id: &str,
    from_model: &str,
    to_model: &str,
) {
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
}

/// Kick off a handover: dispatch a doc-generation turn to the *outgoing*
/// model and park the target model in `handover_to_model`. The session's
/// stored `model` is intentionally left unchanged here — [`finalize_handover`]
/// flips it once the doc is ready. Returns `Ok(())` once the turn is
/// dispatched; the finalize step runs later off the completion listener.
///
/// `lock`: pass the already-held session lock to dispatch immediately (the
/// auto-compaction path, which decides under the lock that the session is
/// idle); pass `None` to take the lock here (the route and MCP paths).
///
/// The doc turn is NEVER persisted to `queued_messages`. A provider that
/// can't absorb a mid-stream send has no way to take it while its turn is
/// running, so the dispatch is DEFERRED instead: the target and watermark
/// are parked and [`handle_completion`] dispatches the doc turn when that
/// live run reports in. Queueing it would strand it — the stale-completion
/// guard there returns before both drain paths, so the doc turn would never
/// run and `handover_to_model` would stay set forever.
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
    //
    // `handover_run_id` is the run-id watermark: every run dispatched so far
    // has a LOWER id, and the doc turn — dispatched just below, or deferred
    // to [`handle_completion`] — has this one or a higher one. It is what
    // lets the completion listener tell the doc turn's completion from one
    // still queued for a run that ended before the handover was even
    // requested — e.g. the idle reaper recycling the previous child.
    // Finalizing on that stale completion would capture an empty doc and
    // flip the model with `EMPTY_DOC_FALLBACK`, losing the context the
    // handover exists to move.
    let run_watermark = crate::provider::agent::current_run_id();
    state
        .db
        .update_session(
            session_id,
            UpdateSession {
                handover_to_model: Some(Some(to_model.to_string())),
                handover_run_id: Some(Some(run_watermark as i64)),
                ..Default::default()
            },
        )
        .await?;

    // Dispatch the doc-generation turn on the OUTGOING model — or defer it.
    //
    // `send_or_queue` is deliberately NOT used: it persists the prompt to
    // `queued_messages` whenever the session is mid-turn on a provider that
    // can't absorb a mid-stream send (ollama/grok/cursor/kimi, most
    // plugins), and nothing would ever deliver it — `handle_completion`'s
    // stale-completion guard returns before both drain paths, so the
    // handover would stay parked forever. Dispatch under the lock when the
    // session can take the turn; otherwise leave it to `handle_completion`.
    let config = doc_turn_config(from_model);
    let message = doc_turn_message(from_model, to_model);
    // Tracks whether a doc turn was actually handed to `send_message_locked`
    // (as opposed to deferred) so a successful dispatch can be marked —
    // `handle_completion` needs that mark to tell the doc turn's completion
    // from a later, unrelated dispatch into the same still-parked session.
    let mut doc_turn_dispatched = false;
    let dispatch_result: anyhow::Result<()> = match lock {
        Some(lock) => {
            append_handover_start(state, session_id, from_model, to_model).await;
            doc_turn_dispatched = true;
            state
                .session_manager
                .send_message_locked(lock, message, &state.db, &state.broadcaster, config)
                .await
        }
        None => {
            let lock = state.session_manager.lock_session(session_id).await;
            if state.session_manager.is_running(session_id).await
                && !state
                    .session_manager
                    .supports_mid_stream_for_session(session_id, from_model)
                    .await
            {
                tracing::info!(
                    session_id = %session_id,
                    from = %from_model,
                    to = %to_model,
                    "Handover doc turn deferred — the live turn's provider can't take a \
                     mid-stream send; it dispatches when that turn completes"
                );
                Ok(())
            } else {
                append_handover_start(state, session_id, from_model, to_model).await;
                doc_turn_dispatched = true;
                state
                    .session_manager
                    .send_message_locked(&lock, message, &state.db, &state.broadcaster, config)
                    .await
            }
        }
    };

    if doc_turn_dispatched && dispatch_result.is_ok() {
        state
            .session_manager
            .mark_handover_doc_dispatched(session_id)
            .await;
    }

    // A synchronous dispatch failure (deleted account, uninstalled
    // provider, missing folder) leaves the flag parked in the DB above with
    // no run ever started — no `ProcessCompletion` will arrive to trigger
    // `finalize_handover`/`abort_handover` via `handle_completion`. Without
    // this, the session would be stuck: every send and model-switch 409s
    // forever, surviving even a restart. Abort right here instead.
    if let Err(e) = dispatch_result {
        let msg = e.to_string();
        if let Err(abort_err) = abort_handover(state, session_id, Some(&msg)).await {
            tracing::error!(
                session_id = %session_id,
                "Failed to abort handover after dispatch failure: {abort_err}"
            );
        }
        return Err(e);
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
    if matches!(state.db.next_queued_message(session_id).await, Ok(Some(_))) {
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
    state
        .session_manager
        .clear_handover_doc_dispatched(session_id)
        .await;

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
                handover_run_id: Some(None),
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
    state
        .session_manager
        .clear_handover_doc_dispatched(session_id)
        .await;

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
                handover_run_id: Some(None),
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

/// Startup reconciliation: abort every handover left parked by the last
/// shutdown. A parked `handover_to_model` with no live run behind it (the
/// process died — or was killed — between `begin_handover` parking the
/// flag and the doc turn's completion landing) has no in-process listener
/// left to ever call `finalize_handover`/`abort_handover` for it; unlike a
/// synchronous dispatch failure, `begin_handover` itself is long gone by
/// the time a fresh process boots. Every row this finds is therefore
/// stuck — abort unconditionally. Returns the number reconciled.
pub async fn reconcile_parked_handovers(state: &Arc<AppState>) -> usize {
    let ids = match state.db.list_session_ids_with_parked_handover().await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!("Failed to list sessions with a parked handover: {e}");
            return 0;
        }
    };
    let mut reconciled = 0;
    for id in ids {
        match abort_handover(
            state,
            &id,
            Some("server restarted while the handover was in flight"),
        )
        .await
        {
            Ok(()) => reconciled += 1,
            Err(e) => {
                tracing::warn!(session_id = %id, "Failed to abort parked handover at startup: {e}")
            }
        }
    }
    reconciled
}
/// Handover half of the completion listener (`main.rs`): decide whether an
/// arriving [`ProcessCompletion`] belongs to an in-flight handover and, when
/// it does, finish the handover and drain anything queued behind the doc
/// turn. Returns whether the completion was consumed here — `true` means the
/// listener must NOT run its normal worker/queue bookkeeping for it.
///
/// A doc turn isn't a normal turn, so a completion for one short-circuits
/// the rest of the listener. The catch this function exists for: completions
/// are queued and handled sequentially, and a provider removes its run from
/// the map BEFORE sending one — so a completion for a run that ended BEFORE
/// the handover started (the idle reaper recycling the previous child, an
/// interrupted turn) can be consumed after `begin_handover` already parked
/// the target and dispatched the doc turn to a fresh child. Treating that
/// stale completion as the doc turn's would finalize while the doc is still
/// streaming (empty `extract_doc` → model flipped with
/// `EMPTY_DOC_FALLBACK`, context lost) or abort a perfectly good compaction.
///
/// The `handover_run_id` watermark stamped by [`begin_handover`] is the
/// discriminator: only a completion whose `run_id` is at or above it can be
/// reporting on the doc turn. A stale one is dropped (still consumed: the
/// session is mid-handover, so respawning or advancing a card off an old
/// run's result would race the doc turn) and the real doc-turn completion
/// finishes the handover when it lands. A `None` watermark — a legacy row,
/// or a handover parked by an older build — keeps the previous behaviour of
/// treating any completion as the doc turn's.
pub async fn handle_completion(
    state: &Arc<AppState>,
    completion: &crate::provider::agent::ProcessCompletion,
) -> bool {
    let session_id = completion.session_id.as_str();
    let Ok(Some(session)) = state.db.get_session(session_id).await else {
        return false;
    };
    if session.handover_to_model.is_none() {
        return false;
    }

    if let Some(watermark) = session.handover_run_id
        && (completion.run_id as i64) < watermark
    {
        tracing::warn!(
            session_id = %session_id,
            run_id = completion.run_id,
            watermark,
            completed = completion.completed,
            "Ignoring completion from a run that predates the in-flight handover"
        );
        // The doc turn may never have been dispatched: `begin_handover`
        // defers it when the session is mid-turn on a provider that can't
        // take a mid-stream send, and THIS is that live turn's completion
        // (its run predates the watermark by construction). Dispatch it
        // here, before the `return true` that short-circuits every drain.
        dispatch_deferred_doc_turn(state, session_id).await;
        return true;
    }

    if !state
        .session_manager
        .is_handover_doc_dispatched(session_id)
        .await
    {
        // `run_id` clears the watermark but no doc turn was ever actually
        // dispatched for this handover — e.g. the orchestrator resumed an
        // ordinary worker chunk into a session whose handover is still
        // parked (a race between the doc turn's process exiting and this
        // listener processing its completion). Treating this completion as
        // the doc turn's would record the chunk's real output as the
        // handover doc and drop `conversation_id`, destroying live context,
        // and would skip `handle_worker_done`'s card bookkeeping. Abort the
        // stale park (context-preserving) and let the caller's normal
        // worker/crash bookkeeping run for this real completion instead.
        tracing::warn!(
            session_id = %session_id,
            run_id = completion.run_id,
            completed = completion.completed,
            "Completion clears the handover watermark but no doc turn was dispatched \
             for it; aborting the stale park instead of swallowing a real completion"
        );
        let _guard = state.session_manager.lock_session(session_id).await;
        if let Err(e) = abort_handover(state, session_id, Some("stale parked handover")).await {
            tracing::error!(
                session_id = %session_id,
                "Failed to abort stale parked handover: {e}"
            );
        }
        return false;
    }

    {
        let _guard = state.session_manager.lock_session(session_id).await;
        // Only a CLEAN doc turn switches the model. A crashed or interrupted
        // doc turn aborts the handover instead — leaving the model and
        // conversation_id untouched so no context is lost ("don't switch if
        // the switch fails", and the hook that lets the user interrupt a
        // handover).
        let res = if completion.completed {
            finalize_handover(state, session_id).await
        } else {
            abort_handover(state, session_id, completion.error.as_deref()).await
        };
        if let Err(e) = res {
            tracing::error!(
                session_id = %session_id,
                completed = completion.completed,
                "Handover finalize/abort failed: {e}"
            );
        }
    } // drop the lock — the drain re-acquires it

    // A message may have queued behind the doc turn; deliver it now. Its
    // dispatch injects the freshly stashed doc via `take_pending_injection`.
    if let Err(e) = crate::worker::orchestrator::drain_queue_for_session(state, session_id).await {
        tracing::warn!(
            session_id = %session_id,
            "Post-handover queue drain failed: {e}"
        );
    }
    true
}

/// Dispatch a handover doc turn that [`begin_handover`] had to defer.
///
/// A provider that can't absorb a mid-stream send has no way to take the
/// doc prompt while its turn is running, so `begin_handover` parks the
/// handover and leaves the prompt undispatched rather than persisting it to
/// `queued_messages`, where the stale-completion guard above would strand
/// it forever. This is the other half: the live turn's completion is stale
/// by construction — its run was dispatched before the watermark — so when
/// it lands with the session idle, the doc turn goes out now.
///
/// `is_running` under the session lock is what tells a deferred doc turn
/// from the case the stale guard was written for (a leftover completion
/// from an older child while the real doc turn streams): a doc turn already
/// in flight keeps the session busy. A doc turn that already FINISHED can't
/// reach here — it cleared `handover_to_model`, so `handle_completion`
/// returned `false` before the guard.
async fn dispatch_deferred_doc_turn(state: &Arc<AppState>, session_id: &str) {
    let lock = state.session_manager.lock_session(session_id).await;
    if state.session_manager.is_running(session_id).await {
        return;
    }
    // Re-read under the lock: a route may have aborted the handover (the
    // Cancel button) between this completion arriving and the lock.
    let Ok(Some(session)) = state.db.get_session(session_id).await else {
        return;
    };
    let Some(to_model) = session.handover_to_model.clone() else {
        return;
    };
    // Still the OUTGOING model: a handover parks the target and only
    // `finalize_handover` flips `model`.
    let from_model = session.model.clone().unwrap_or_else(|| "default".into());

    tracing::info!(
        session_id = %session_id,
        from = %from_model,
        to = %to_model,
        "Dispatching the handover doc turn deferred while the session was mid-turn"
    );
    let result = state
        .session_manager
        .send_message_locked(
            &lock,
            doc_turn_message(&from_model, &to_model),
            &state.db,
            &state.broadcaster,
            doc_turn_config(&from_model),
        )
        .await;
    if result.is_ok() {
        state
            .session_manager
            .mark_handover_doc_dispatched(session_id)
            .await;
    }
    if let Err(e) = result {
        let msg = e.to_string();
        if let Err(abort_err) = abort_handover(state, session_id, Some(&msg)).await {
            tracing::error!(
                session_id = %session_id,
                "Failed to abort handover after deferred dispatch failure: {abort_err}"
            );
        }
    }
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
/// If a finalized handover, a review switch, or a doc-review pass left an
/// injection waiting, consume it (clearing the columns) and return the
/// injection-wrapped message. Otherwise return `text` unchanged.
///
/// Prefer [`peek_pending_injection`] + [`clear_pending_injections`] on any
/// path that can still fail before the turn actually reaches the provider —
/// the handover doc is the only surviving copy of the pre-reset
/// conversation, so clearing it before a dispatch that then errors loses it
/// permanently. This wrapper is the eager (peek-then-clear) form, kept for
/// callers that have nothing left to fail.
/// have nothing left to fail.
pub async fn take_pending_injection(db: &crate::db::Db, session_id: &str, text: &str) -> String {
    let (out, used) = peek_pending_injection(db, session_id, text).await;
    clear_pending_injections(db, session_id, &used).await;
    out
}

/// What [`peek_pending_injection`] actually consumed from the session row, so
/// the caller can clear exactly those columns once the turn has really
/// reached the provider — and leave them armed when it hasn't.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PendingUsed {
    /// `sessions.pending_handover_doc` was non-empty and got injected.
    pub handover_doc: bool,
    /// `sessions.pending_plan_review` was set (whether or not a plan was
    /// found — the flag is one-shot either way, or it sticks forever).
    pub plan_review: bool,
    /// `sessions.pending_doc_review` held this review id.
    pub doc_review: Option<String>,
}

impl PendingUsed {
    /// Nothing was armed — the caller has nothing to clear.
    pub fn is_empty(&self) -> bool {
        !self.handover_doc && !self.plan_review && self.doc_review.is_none()
    }
}

/// Build the injected message WITHOUT consuming any of the one-shot columns.
///
/// Returns `(text, used)`. The caller owns their lifetime: call
/// [`clear_pending_injections`] once the turn has actually been handed to the
/// provider, and leave the columns alone on every error path so the user's
/// retry still carries the context.
/// error path so the user's retry still carries the context.
///
/// Called from `send_message_locked` — the single dispatch chokepoint — so
/// the HTTP route, the queue drain, and every other path inject
/// consistently. Compactions (same-model handovers) get the compaction
/// wording; real model switches get the predecessor's label, read back from
/// the most recent `handover` event.
pub async fn peek_pending_injection(
    db: &crate::db::Db,
    session_id: &str,
    text: &str,
) -> (String, PendingUsed) {
    let mut used = PendingUsed::default();
    let session = match db.get_session(session_id).await {
        Ok(Some(s)) => s,
        _ => return (text.to_string(), used),
    };

    // 1. Handover / compaction doc. NOT cleared here — see the doc comment.
    let mut out = text.to_string();
    if let Some(doc) = session
        .pending_handover_doc
        .filter(|d| !d.trim().is_empty())
    {
        used.handover_doc = true;
        out = match latest_handover_meta(db, session_id).await {
            Some(meta) if meta.recovery => build_recovery_injection(&meta.from, &doc, text),
            Some(meta) if meta.compaction => build_compaction_injection(&doc, text),
            Some(meta) => build_injection(&meta.from, &doc, text),
            None => build_injection("a previous model", &doc, text),
        };
    }

    // 2. Parent-model review: when a review switch armed `pending_plan_review`,
    //    prepend the saved plan + a "review the work against this plan"
    //    directive to the resumed turn. One-shot, but cleared by the caller
    //    after dispatch — flagged whether or not a plan is found so it can't
    //    get stuck.
    if session.pending_plan_review {
        used.plan_review = true;
        let plan = match session.card_id.as_deref() {
            Some(card_id) => db.get_plan_for_card(card_id).await.ok().flatten(),
            None => db.get_plan_for_session(session_id).await.ok().flatten(),
        };
        if let Some(plan) = plan {
            out = build_plan_review_injection(&plan.markdown, &out);
        }
    }

    // 3. Document review: a review pass armed `sessions.pending_doc_review`
    //    with the review id. Prepend the document the user is looking at —
    //    line-numbered, so the annotations' line ranges address something —
    //    plus every annotation still open. One-shot, cleared by the caller
    //    after dispatch, whether or not the review survives.
    if let Some(review_id) = session.pending_doc_review {
        used.doc_review = Some(review_id.clone());
        if let Ok(Some(review)) = db.get_doc_review(&review_id).await {
            let markdown = db
                .get_doc_review_version(&review_id, review.current_version)
                .await
                .ok()
                .flatten()
                .map(|v| v.markdown)
                .unwrap_or_default();
            let comments = db
                .list_doc_review_comments(&review_id, true)
                .await
                .unwrap_or_default();
            out = build_doc_review_injection(&review, &markdown, &comments, &out);
        }
    }

    (out, used)
}

/// Clear every one-shot injection column [`peek_pending_injection`] reported
/// as used, once the turn carrying them has actually been dispatched.
/// Best-effort: a failed write only means the injection repeats on the next
/// turn, which is strictly safer than losing it.
pub async fn clear_pending_injections(db: &crate::db::Db, session_id: &str, used: &PendingUsed) {
    if used.is_empty() {
        return;
    }
    if used.handover_doc || used.plan_review {
        let _ = db
            .update_session(
                session_id,
                UpdateSession {
                    pending_handover_doc: used.handover_doc.then_some(None),
                    pending_plan_review: used.plan_review.then_some(false),
                    ..Default::default()
                },
            )
            .await;
    }
    if used.doc_review.is_some() {
        let _ = db.take_pending_doc_review(session_id).await;
    }
}

/// Wrap the (already-injected) turn text with a parent-model review
/// directive and the saved plan, so the resumed thinking model reviews the
/// completed work against the plan before finishing.
fn build_plan_review_injection(plan_markdown: &str, following: &str) -> String {
    format!(
        "[Plan review — you are the original (thinking) model resuming to REVIEW the work done \
         while you were switched away. Below is the plan that was saved for this work. Verify the \
         wrong, and only then finish. Once the work aligns with the plan, ask the user via \
         `ask_user` whether to commit and push, and run git commit/push (through `run_command`) \
         only after they explicitly confirm.]\n\n<plan>\n{plan_markdown}\n</plan>\n\n---\n\n{following}"
    )
}

/// Prefix every line with its 1-based number, right-aligned in a fixed
/// gutter. The assistant never sees the document any other way, so the
/// annotations' `[lines A-B]` ranges always have something to point at.
fn number_lines(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len() + markdown.lines().count() * 8);
    for (i, line) in markdown.lines().enumerate() {
        out.push_str(&format!("{:>4} | {line}\n", i + 1));
    }
    out
}

/// Wrap the turn text with the document under review: what it is, the
/// numbered markdown of its current version, and the annotations still
/// open. Mirrors [`build_plan_review_injection`] — one self-contained block
/// ahead of the user's message, so a review session never has to guess
/// which document (or which version) the turn is about.
fn build_doc_review_injection(
    review: &crate::db::models::DocReview,
    markdown: &str,
    comments: &[crate::db::models::DocReviewComment],
    following: &str,
) -> String {
    let numbered = number_lines(markdown);
    let annotations = if comments.is_empty() {
        "(none open — go by the user's message below.)".to_string()
    } else {
        comments
            .iter()
            .map(|c| {
                let quote = c
                    .quote
                    .as_deref()
                    .map(str::trim)
                    .filter(|q| !q.is_empty())
                    .map(|q| format!(" «{q}»"))
                    .unwrap_or_default();
                format!(
                    "[lines {}-{}] ({}){quote} — {} (id: {})",
                    c.start_line,
                    c.end_line,
                    c.kind,
                    c.body.trim(),
                    c.id
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "[Document review — below is the document you are reviewing, exactly as the user sees it. \
         Revise it ONLY through `submit_review_revision`, passing the FULL replacement markdown; \
         never edit the source file directly, and leave every line the annotations did not ask \
         about byte-identical. The line numbers are an addressing aid for the annotations — never \
         copy them into the document. Address every open annotation and report each one in \
         `resolutions`; if an intent is unclear, ask with `ask_user` instead of guessing.]\n\n\
         Title: {title}\nSource: {kind} — {source_ref}\nVersion: {version}\n\n\
         <document>\n{numbered}</document>\n\n\
         <open-annotations>\n{annotations}\n</open-annotations>\n\n---\n\n{following}",
        title = review.title,
        kind = review.source_kind,
        source_ref = review.source_ref,
        version = review.current_version,
    )
}

/// Shape of the most recent `handover` event, if any. Drives which
/// injection wrapper `peek_pending_injection` applies.
struct HandoverMeta {
    from: String,
    compaction: bool,
    recovery: bool,
}

/// Most recent `handover` event's metadata, if any.
async fn latest_handover_meta(db: &crate::db::Db, session_id: &str) -> Option<HandoverMeta> {
    let events = db.events_tail(session_id, 200).await.ok()?;
    events.iter().rev().find_map(|e| {
        if e.kind != "handover" {
            return None;
        }
        let v = serde_json::from_str::<serde_json::Value>(&e.data).ok()?;
        let from = v.get("from")?.as_str()?.to_string();
        let recovery = v.get("recovery").and_then(|c| c.as_bool()).unwrap_or(false);
        let compaction = v
            .get("compaction")
            .and_then(|c| c.as_bool())
            .unwrap_or_else(|| v.get("to").and_then(|t| t.as_str()) == Some(from.as_str()));
        Some(HandoverMeta {
            from,
            compaction,
            recovery,
        })
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

    #[test]
    fn estimate_tokens_rounds_up_four_chars() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        // 12 chars → 3 tokens exactly.
        assert_eq!(estimate_tokens("abcdefghijkl"), 3);
    }

    #[test]
    fn context_window_for_long_context_alias() {
        assert_eq!(context_window_for("claude:opus[1m]"), 1_000_000);
        assert_eq!(context_window_for("claude:opus[1m]@acct"), 1_000_000);
        assert_eq!(context_window_for("claude:opus"), 200_000);
        assert_eq!(context_window_for("mock:echo@acct2"), 200_000);
    }

    #[test]
    fn build_recovery_injection_uses_transcript_tag() {
        let out = build_recovery_injection("claude:opus", "## User\n\nhi", "continue");
        assert!(out.contains("<transcript>\n## User\n\nhi\n</transcript>"));
        assert!(out.contains("continue"));
        assert!(out.contains("claude:opus"));
        assert!(out.contains("Recovery context"));
        assert!(!out.contains("<handover>"));
    }

    #[test]
    fn build_recovery_transcript_joins_user_and_assistant() {
        let events = vec![
            ev(1, "user", serde_json::json!({ "text": "hello" })),
            ev(2, "agent-start", serde_json::json!({ "model": "m" })),
            ev(3, "agent-text", serde_json::json!({ "text": "Hi " })),
            ev(4, "agent-text", serde_json::json!({ "text": "there." })),
            ev(5, "agent-tool-start", serde_json::json!({ "name": "Read" })),
            ev(6, "agent-end", serde_json::json!({ "status": "complete" })),
            ev(7, "user", serde_json::json!({ "text": "go on" })),
        ];
        let out = build_recovery_transcript(&events);
        assert!(out.contains("## User\n\nhello"), "got: {out}");
        assert!(
            out.contains("## Assistant\n\nHi there.\n- `Read`"),
            "chunks concatenate and tool is a one-liner: {out}"
        );
        assert!(out.contains("## User\n\ngo on"), "got: {out}");
        // Tool payloads never leak in.
        assert!(!out.contains("agent-tool-end"));
    }

    #[test]
    fn build_recovery_transcript_starts_after_last_handover() {
        let events = vec![
            ev(1, "user", serde_json::json!({ "text": "old" })),
            ev(2, "agent-text", serde_json::json!({ "text": "old reply" })),
            ev(
                3,
                "handover",
                serde_json::json!({
                    "from": "a",
                    "to": "a",
                    "doc": "Keep going on foo.rs",
                    "compaction": true
                }),
            ),
            ev(4, "user", serde_json::json!({ "text": "and then?" })),
            ev(5, "agent-text", serde_json::json!({ "text": "next" })),
        ];
        let out = build_recovery_transcript(&events);
        assert!(
            out.contains("## Prior context\n\nKeep going on foo.rs"),
            "got: {out}"
        );
        assert!(out.contains("## User\n\nand then?"), "got: {out}");
        assert!(out.contains("## Assistant\n\nnext"), "got: {out}");
        assert!(
            !out.contains("old reply"),
            "pre-compaction turns dropped: {out}"
        );
    }

    #[test]
    fn doc_review_injection_numbers_lines_and_lists_open_annotations() {
        let review = crate::db::models::DocReview {
            id: "r1".into(),
            title: "Launch plan".into(),
            source_kind: "file".into(),
            source_ref: "f1:docs/launch.md".into(),
            folder_id: Some("f1".into()),
            project_id: None,
            session_id: Some("s1".into()),
            status: "running".into(),
            current_version: 3,
            created_at: "t".into(),
            updated_at: "t".into(),
        };
        let comment = |id: &str, lines: (i32, i32), quote: Option<&str>, kind: &str, body: &str| {
            crate::db::models::DocReviewComment {
                id: id.into(),
                review_id: "r1".into(),
                version: 3,
                start_line: lines.0,
                end_line: lines.1,
                quote: quote.map(str::to_string),
                kind: kind.into(),
                body: body.into(),
                status: "sent".into(),
                resolution_note: None,
                created_at: "t".into(),
                external_kind: None,
                external_id: None,
            }
        };
        let out = build_doc_review_injection(
            &review,
            "# Launch\n\nShip on Tuesday.\n",
            &[
                comment(
                    "c1",
                    (3, 3),
                    Some("Ship on Tuesday."),
                    "wrong",
                    "it is Thursday",
                ),
                comment("c2", (1, 2), None, "shorten", "cut the preamble"),
            ],
            "Run the pass.",
        );

        assert!(out.contains("Title: Launch plan"), "got: {out}");
        assert!(
            out.contains("Source: file — f1:docs/launch.md"),
            "got: {out}"
        );
        assert!(out.contains("Version: 3"), "got: {out}");
        assert!(
            out.contains(
                "<document>\n   1 | # Launch\n   2 | \n   3 | Ship on Tuesday.\n</document>"
            ),
            "lines are 1-based and numbered: {out}"
        );
        assert!(
            out.contains("[lines 3-3] (wrong) «Ship on Tuesday.» — it is Thursday (id: c1)"),
            "got: {out}"
        );
        assert!(
            out.contains("[lines 1-2] (shorten) — cut the preamble (id: c2)"),
            "a quote-less annotation drops the guillemets: {out}"
        );
        assert!(out.ends_with("---\n\nRun the pass."), "got: {out}");
        assert!(out.contains("submit_review_revision"), "got: {out}");
    }

    #[test]
    fn doc_review_injection_says_so_when_nothing_is_open() {
        let review = crate::db::models::DocReview {
            id: "r1".into(),
            title: "Doc".into(),
            source_kind: "report".into(),
            source_ref: "2026-07-28/audit.md".into(),
            folder_id: None,
            project_id: None,
            session_id: None,
            status: "running".into(),
            current_version: 1,
            created_at: "t".into(),
            updated_at: "t".into(),
        };
        let out = build_doc_review_injection(&review, "# Doc\n", &[], "Tighten section 2.");
        assert!(out.contains("<open-annotations>\n(none open"), "got: {out}");
        assert!(out.ends_with("Tighten section 2."), "got: {out}");
    }

    /// The whole point of `pending_doc_review` being one-shot: the document
    /// rides along with the pass that armed it, and the next ordinary turn
    /// is a plain message again.
    #[tokio::test]
    async fn take_pending_injection_injects_the_review_once() {
        let db = crate::db::Db::in_memory().unwrap();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(crate::db::models::NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_session(crate::db::models::NewSession {
            id: "s1".into(),
            name: "Review".into(),
            folder_id: "f1".into(),
            is_expert: true,
            expert_kind: Some(crate::service::doc_reviews::EXPERT_KIND.into()),
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();
        let review = db
            .create_doc_review(
                "Doc",
                "file",
                "f1:docs/doc.md",
                Some("f1"),
                None,
                "# Doc\n\nbody\n",
            )
            .await
            .unwrap();
        db.set_doc_review_session(&review.id, Some("s1"))
            .await
            .unwrap();
        db.add_doc_review_comment(&review.id, 1, (3, 3), Some("body"), "expand", "say more")
            .await
            .unwrap();
        db.set_pending_doc_review("s1", &review.id).await.unwrap();

        let first = take_pending_injection(&db, "s1", "Run the pass.").await;
        assert!(first.contains("<document>\n   1 | # Doc"), "got: {first}");
        assert!(first.contains("(expand)"), "got: {first}");
        assert!(first.ends_with("Run the pass."), "got: {first}");

        let second = take_pending_injection(&db, "s1", "And again.").await;
        assert_eq!(
            second, "And again.",
            "the flag is consumed by the first turn"
        );
    }

    /// A recovery-flagged handover event must pick the transcript wrapper,
    /// not the summary-handover wrapper — the incoming model has to know
    /// this is raw history, not a curated doc.
    #[tokio::test]
    async fn take_pending_injection_uses_recovery_wrapper() {
        let db = crate::db::Db::in_memory().unwrap();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(crate::db::models::NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_session(crate::db::models::NewSession {
            id: "s1".into(),
            name: "Chat".into(),
            folder_id: "f1".into(),
            model: Some("mock:echo@acct2".into()),
            created_at: ts.clone(),
            last_activity: ts,
            pending_handover_doc: Some("## User\n\nhello".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        db.append_event(
            "s1",
            "handover",
            serde_json::json!({
                "from": "mock:echo",
                "to": "mock:echo@acct2",
                "doc": "## User\n\nhello",
                "compaction": false,
                "recovery": true,
            }),
        )
        .await
        .unwrap();

        let out = take_pending_injection(&db, "s1", "continue").await;
        assert!(out.contains("Recovery context"), "got: {out}");
        assert!(
            out.contains("<transcript>\n## User\n\nhello\n</transcript>"),
            "got: {out}"
        );
        assert!(out.contains("continue"), "got: {out}");
        assert!(
            !out.contains("<handover>"),
            "recovery must not use the summary wrapper: {out}"
        );
    }

    /// A synchronous dispatch failure (deleted account, uninstalled
    /// provider, missing folder) must not leave `handover_to_model` parked:
    /// nothing will ever complete to clear it, and every future send would
    /// 409 forever. `begin_handover` must abort itself on that path.
    #[tokio::test]
    async fn begin_handover_clears_the_flag_when_dispatch_fails() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::auth::middleware::tests::test_state(dir.path());
        let ts = chrono::Utc::now().to_rfc3339();
        state
            .db
            .create_folder(crate::db::models::NewFolder {
                id: "f1".into(),
                name: "F".into(),
                path: dir.path().to_string_lossy().into_owned(),
                created_at: ts.clone(),
            })
            .await
            .unwrap();
        // A model with no registered provider: `send_message_locked` fails
        // resolving it ("unknown agent provider") before any process is
        // ever spawned — a stand-in for any synchronous dispatch failure
        // (deleted account, uninstalled provider, missing folder).
        state
            .db
            .create_session(crate::db::models::NewSession {
                id: "s1".into(),
                name: "Chat".into(),
                folder_id: "f1".into(),
                model: Some("nosuchprovider:foo".into()),
                created_at: ts.clone(),
                last_activity: ts,
                ..Default::default()
            })
            .await
            .unwrap();

        let err = begin_handover(&state, "s1", "nosuchprovider:foo", "claude:opus", None)
            .await
            .expect_err("dispatch should fail — the provider isn't registered");
        assert!(
            err.to_string().contains("unknown agent provider"),
            "got: {err}"
        );

        let session = state.db.get_session("s1").await.unwrap().unwrap();
        assert_eq!(
            session.handover_to_model, None,
            "a failed dispatch must not leave the handover parked"
        );
        assert_eq!(session.handover_run_id, None);
        // `model`/`conversation_id` are untouched — same contract as an
        // ordinary abort.
        assert_eq!(session.model.as_deref(), Some("nosuchprovider:foo"));

        let events = state.db.events_tail("s1", 20).await.unwrap();
        assert!(
            events.iter().any(|e| e.kind == "handover-aborted"),
            "expected a handover-aborted marker, got kinds: {:?}",
            events.iter().map(|e| &e.kind).collect::<Vec<_>>()
        );
    }

    /// Same failure mode, but on the deferred-dispatch path: a handover
    /// parked while the session was mid-turn on a provider that can't take
    /// a mid-stream send, whose deferred dispatch then itself fails once
    /// the live turn completes.
    #[tokio::test]
    async fn dispatch_deferred_doc_turn_clears_the_flag_when_dispatch_fails() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::auth::middleware::tests::test_state(dir.path());
        let ts = chrono::Utc::now().to_rfc3339();
        state
            .db
            .create_folder(crate::db::models::NewFolder {
                id: "f1".into(),
                name: "F".into(),
                path: dir.path().to_string_lossy().into_owned(),
                created_at: ts.clone(),
            })
            .await
            .unwrap();
        state
            .db
            .create_session(crate::db::models::NewSession {
                id: "s1".into(),
                name: "Chat".into(),
                folder_id: "f1".into(),
                model: Some("nosuchprovider:foo".into()),
                created_at: ts.clone(),
                last_activity: ts,
                ..Default::default()
            })
            .await
            .unwrap();
        // Simulate what `begin_handover` would have parked before deferring.
        state
            .db
            .update_session(
                "s1",
                crate::db::models::UpdateSession {
                    handover_to_model: Some(Some("claude:opus".into())),
                    handover_run_id: Some(Some(0)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        dispatch_deferred_doc_turn(&state, "s1").await;

        let session = state.db.get_session("s1").await.unwrap().unwrap();
        assert_eq!(
            session.handover_to_model, None,
            "a failed deferred dispatch must not leave the handover parked"
        );
        let events = state.db.events_tail("s1", 20).await.unwrap();
        assert!(
            events.iter().any(|e| e.kind == "handover-aborted"),
            "expected a handover-aborted marker, got kinds: {:?}",
            events.iter().map(|e| &e.kind).collect::<Vec<_>>()
        );
    }

    /// Startup reconciliation: any handover left parked by the last
    /// shutdown has nothing left to ever clear it (the process that would
    /// have finalized/aborted it is gone) — reconcile must abort every one
    /// it finds and report how many.
    #[tokio::test]
    async fn reconcile_parked_handovers_clears_every_parked_row() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::auth::middleware::tests::test_state(dir.path());
        let ts = chrono::Utc::now().to_rfc3339();
        state
            .db
            .create_folder(crate::db::models::NewFolder {
                id: "f1".into(),
                name: "F".into(),
                path: dir.path().to_string_lossy().into_owned(),
                created_at: ts.clone(),
            })
            .await
            .unwrap();
        for id in ["s1", "s2", "s3"] {
            state
                .db
                .create_session(crate::db::models::NewSession {
                    id: id.into(),
                    name: "Chat".into(),
                    folder_id: "f1".into(),
                    model: Some("claude:opus".into()),
                    created_at: ts.clone(),
                    last_activity: ts.clone(),
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        // Only s1 and s3 have a handover parked; s2 is an ordinary session.
        for id in ["s1", "s3"] {
            state
                .db
                .update_session(
                    id,
                    crate::db::models::UpdateSession {
                        handover_to_model: Some(Some("grok:grok-4".into())),
                        handover_run_id: Some(Some(0)),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }

        let reconciled = reconcile_parked_handovers(&state).await;
        assert_eq!(reconciled, 2);

        for id in ["s1", "s2", "s3"] {
            let session = state.db.get_session(id).await.unwrap().unwrap();
            assert_eq!(
                session.handover_to_model, None,
                "session {id} should have no handover parked after reconciliation"
            );
        }
    }
}

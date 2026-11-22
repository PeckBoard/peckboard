//! Automatic recovery from a conversation that outgrew the context window.
//!
//! A long session eventually fails every turn with "Prompt is too long"
//! (Claude) / `context_length_exceeded` (OpenAI-compatible APIs): the
//! provider resumes the same conversation each time, so every retry carries
//! the same oversized history and fails identically. Until now the only way
//! out was "Clear session" plus a hand-written rebrief.
//!
//! This does the rebrief automatically. On a [`CrashKind::ContextOverflow`]
//! completion the session's conversation is dropped and the turn re-runs
//! fresh on the same model, carrying the reconstructed transcript
//! ([`handover::build_recovery_transcript`]: user/assistant text, tool calls
//! by name only — tool output is what usually filled the window), trimmed
//! to [`TRANSCRIPT_WINDOW_SHARE_PCT`] of the model's window. It reuses the
//! handover plumbing: a `handover` event (`recovery` + `overflow`) stops the
//! event-tail resume scan and renders the notice, and `pending_handover_doc`
//! makes the next dispatch wrap the message with
//! [`handover::build_overflow_injection`].
//!
//! Scope: plain interactive sessions. Workers are owned by the orchestrator
//! (a crashed worker's card is re-dispatched) and auto-compact well before
//! the limit; a subagent's crash has already been reported to its parent by
//! the time this runs, so a replay there would produce an orphaned result.
//!
//! One recovery per overflow streak: if the fresh-context turn overflows
//! too, the session is left alone with a notice rather than looping.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

use tokio::sync::Mutex;

use crate::db::models::UpdateSession;
use crate::handover;
use crate::provider::agent::ProcessCompletion;
use crate::provider::stream::CrashKind;
use crate::state::AppState;
use crate::ws::broadcaster::WsEvent;

/// Event appended when an overflow recurs right after a recovery, so the
/// chat can say why nothing happened automatically this time.
pub const OVERFLOW_STUCK_KIND: &str = "context-overflow-stuck";

/// Share of the model's context window the carried-over transcript may use.
/// The rest is headroom for the system prompt, tool definitions, the user's
/// message and the turn's own tool output.
pub const TRANSCRIPT_WINDOW_SHARE_PCT: i64 = 60;

/// Sessions whose current overflow streak already spent its recovery.
/// Cleared by the next clean completion. In-memory like
/// [`crate::provider::resume_recovery`]'s budget: after a restart nothing is
/// mid-turn, and the reset itself is durable.
static RECOVERED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Completion-listener entry point. Called for every settled turn, ahead of
/// [`crate::provider::auth_recovery::handle_completion`] (which drops the
/// replay snapshot this needs) and of the queue drain that delivers the
/// replay.
pub async fn handle_completion(state: &Arc<AppState>, completion: &ProcessCompletion) {
    if completion.turn_end_only {
        return;
    }
    let session_id = completion.session_id.as_str();
    if completion.completed {
        RECOVERED.lock().await.remove(session_id);
        return;
    }
    if completion.error_kind != Some(CrashKind::ContextOverflow) {
        return;
    }

    let Ok(Some(session)) = state.db.get_session(session_id).await else {
        return;
    };
    if session.is_worker || session.parent_session_id.is_some() {
        return;
    }
    if session.handover_to_model.is_some() {
        // A model switch / compaction owns the session right now.
        return;
    }

    let first = RECOVERED.lock().await.insert(session_id.to_string());
    if !first {
        tracing::warn!(
            session_id = %session_id,
            "Context overflow again right after a fresh-context recovery; not retrying"
        );
        append_event(
            state,
            session_id,
            OVERFLOW_STUCK_KIND,
            serde_json::json!({ "reason": completion.error.clone().unwrap_or_default() }),
        )
        .await;
        return;
    }

    let model = session.model.clone().unwrap_or_else(|| "default".into());
    let replay = state.session_manager.last_dispatched_turn(session_id).await;
    let events = match state.db.list_events_by_session(session_id, None).await {
        Ok(evs) => evs,
        Err(e) => {
            tracing::error!(session_id = %session_id, "Overflow recovery: failed to load events: {e}");
            return;
        }
    };

    // The failed message is replayed as the user's message, so leave it out
    // of the history: the model would otherwise read it twice.
    let cut = replay.as_ref().and_then(|r| {
        let want = r.text.trim();
        events
            .iter()
            .rposition(|e| e.kind == "user" && handover::event_text(&e.data).trim() == want)
            .or_else(|| events.iter().rposition(|e| e.kind == "user"))
    });
    let history = match cut {
        Some(i) => &events[..i],
        None => &events[..],
    };
    let budget = handover::context_window_for(&model) * TRANSCRIPT_WINDOW_SHARE_PCT / 100;
    let transcript =
        handover::trim_transcript_to_tokens(&handover::build_recovery_transcript(history), budget);
    let tokens = handover::estimate_tokens(&transcript);

    // The `handover` marker does three jobs: it stops the event-tail resume
    // scan from handing the overflowed conversation back, it tells
    // `peek_pending_injection` to use the overflow wrapper, and the chat
    // renders it as the notice.
    append_event(
        state,
        session_id,
        "handover",
        serde_json::json!({
            "from": model,
            "to": model,
            "doc": transcript,
            "compaction": false,
            "recovery": true,
            "overflow": true,
            "tokens": tokens,
            "reason": completion.error.clone().unwrap_or_default(),
        }),
    )
    .await;

    let pending = (!transcript.trim().is_empty()).then(|| transcript.clone());
    let updated = state
        .db
        .update_session(
            session_id,
            UpdateSession {
                conversation_id: Some(None),
                pending_handover_doc: Some(pending),
                context_reset_ts: Some(Some(chrono::Utc::now().timestamp_millis())),
                ..Default::default()
            },
        )
        .await;
    match updated {
        Ok(Some(s)) => state.broadcaster.broadcast(WsEvent {
            event_type: "session-updated".into(),
            session_id: session_id.to_string(),
            data: serde_json::to_value(&s).unwrap_or(serde_json::Value::Null),
        }),
        Ok(None) => return,
        Err(e) => {
            tracing::error!(session_id = %session_id, "Overflow recovery: failed to reset session: {e}");
            return;
        }
    }
    tracing::warn!(
        session_id = %session_id,
        model = %model,
        tokens,
        "Context overflow: restarting the conversation fresh with the transcript carried over"
    );

    let Some(replay) = replay else { return };
    // Queue rather than dispatch: the listener's drain runs next under the
    // session lock — the one path allowed to decide "is running → spawn" —
    // and injects the parked transcript at delivery.
    let attachment_ids = if replay.attachment_ids.is_empty() {
        None
    } else {
        serde_json::to_string(&replay.attachment_ids).ok()
    };
    let queued = state
        .db
        .enqueue_message(crate::db::models::NewQueuedMessage {
            session_id: session_id.to_string(),
            text: replay.text.clone(),
            queued_at: chrono::Utc::now().to_rfc3339(),
            model: Some(replay.model.clone()),
            effort: replay.effort.clone(),
            attachment_ids,
            // The transcript already shows this message where the user
            // typed it; the drain must not append a second copy.
            user_event_appended: true,
        })
        .await;
    match queued {
        Ok(row) => state.broadcaster.broadcast(WsEvent {
            event_type: "queue".into(),
            session_id: session_id.to_string(),
            data: serde_json::json!({ "action": "set", "id": row.id }),
        }),
        Err(e) => tracing::error!(
            session_id = %session_id,
            "Failed to re-queue a turn after a context overflow: {e}"
        ),
    }
}

async fn append_event(
    state: &Arc<AppState>,
    session_id: &str,
    kind: &str,
    data: serde_json::Value,
) {
    match state.db.append_event(session_id, kind, data.clone()).await {
        Ok(ev) => state.broadcaster.broadcast(WsEvent {
            event_type: "event".into(),
            session_id: session_id.to_string(),
            data: serde_json::json!({
                "id": ev.id,
                "seq": ev.seq,
                "ts": ev.ts,
                "kind": ev.kind,
                "data": data,
            }),
        }),
        Err(e) => tracing::warn!(
            session_id = %session_id,
            "Failed to append the {kind} marker: {e}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completion(sid: &str, completed: bool, kind: Option<CrashKind>) -> ProcessCompletion {
        ProcessCompletion {
            session_id: sid.into(),
            completed,
            error: kind.map(|_| "Prompt is too long".to_string()),
            error_kind: kind,
            run_id: 1,
            turn_end_only: false,
        }
    }

    async fn kinds(state: &AppState, sid: &str) -> Vec<String> {
        state
            .db
            .list_events_by_session(sid, None)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect()
    }

    /// An overflow drops the conversation and parks the transcript once;
    /// a second overflow in the same streak only leaves a notice; a clean
    /// turn re-arms the recovery.
    #[tokio::test]
    async fn overflow_resets_once_per_streak() {
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
                id: "ovf".into(),
                name: "Chat".into(),
                folder_id: "f1".into(),
                model: Some("claude:opus".into()),
                created_at: ts.clone(),
                last_activity: ts,
                ..Default::default()
            })
            .await
            .unwrap();
        state
            .db
            .update_session(
                "ovf",
                UpdateSession {
                    conversation_id: Some(Some("conv-1".into())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        for (kind, text) in [
            ("user", "build the parser"),
            ("agent-text", "Parser built."),
            ("user", "now the tests"),
        ] {
            state
                .db
                .append_event("ovf", kind, serde_json::json!({ "text": text }))
                .await
                .unwrap();
        }

        // Not an overflow: untouched.
        handle_completion(
            &state,
            &completion("ovf", false, Some(CrashKind::RateLimit)),
        )
        .await;
        let s = state.db.get_session("ovf").await.unwrap().unwrap();
        assert_eq!(s.conversation_id.as_deref(), Some("conv-1"));

        handle_completion(
            &state,
            &completion("ovf", false, Some(CrashKind::ContextOverflow)),
        )
        .await;
        let s = state.db.get_session("ovf").await.unwrap().unwrap();
        assert_eq!(s.conversation_id, None);
        let doc = s.pending_handover_doc.expect("transcript parked");
        assert!(doc.contains("build the parser") && doc.contains("Parser built."));
        assert_eq!(
            kinds(&state, "ovf")
                .await
                .iter()
                .filter(|k| *k == "handover")
                .count(),
            1
        );

        // Same streak: no second reset, just the notice.
        handle_completion(
            &state,
            &completion("ovf", false, Some(CrashKind::ContextOverflow)),
        )
        .await;
        let k = kinds(&state, "ovf").await;
        assert_eq!(k.iter().filter(|k| *k == "handover").count(), 1);
        assert!(k.iter().any(|k| k == OVERFLOW_STUCK_KIND));

        // A clean turn ends the streak; the next overflow recovers again.
        handle_completion(&state, &completion("ovf", true, None)).await;
        handle_completion(
            &state,
            &completion("ovf", false, Some(CrashKind::ContextOverflow)),
        )
        .await;
        assert_eq!(
            kinds(&state, "ovf")
                .await
                .iter()
                .filter(|k| *k == "handover")
                .count(),
            2
        );
    }
}

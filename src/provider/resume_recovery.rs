//! Automatic recovery from a conversation the provider refuses to resume.
//!
//! Every CLI-backed provider resumes by id, and every one of them treats an
//! id it cannot find as a hard startup failure: `no rollout found for thread
//! id …` (Codex), `No conversation found with session ID …` (Claude). The
//! id can go missing for reasons entirely outside the session — the user
//! pruned `~/.codex/sessions`, restored a Peckboard data dir onto a
//! different machine, or hit the cross-provider mix-up that
//! [`crate::provider::resume`] now prevents at the type level.
//!
//! Whatever the cause, the shape is the same and it is the worst kind of
//! stuck: the turn cannot succeed, the id is re-derived identically on the
//! next attempt, and "Terminate agent" only kills a process that will be
//! spawned again with the same argument. The session stays wedged until the
//! user clears it, which costs them the whole transcript.
//!
//! So a resume rejection is treated as what it is — a statement that this
//! conversation is gone, not that the turn is impossible. The session's
//! `conversation_id` is dropped, a [`CONVERSATION_RESET_KIND`] marker stops
//! the event-tail fallback from re-serving the same id (see
//! [`crate::provider::manager::resume_conversation_id_from_tail`]), and the
//! user's turn is re-queued to run cold. Context from before the reset is
//! lost — it was already unreachable — but the session keeps working and
//! the transcript stays intact.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use tokio::sync::Mutex;

use crate::db::models::UpdateSession;
use crate::provider::agent::ProcessCompletion;
use crate::provider::stream::CrashKind;
use crate::state::AppState;
use crate::ws::broadcaster::WsEvent;

/// Appended when a rejected resume dropped the session's conversation.
/// Load-bearing beyond the transcript: the resume scan stops here, so the
/// abandoned id can't come back off the event log.
pub const CONVERSATION_RESET_KIND: &str = "conversation-reset";

/// Session id → the failure text its cold replay was spent on.
///
/// One replay per distinct failure. A cold turn carries no id, so it cannot
/// fail this way twice for the same reason; a *different* resume failure
/// (new id, new cause) gets its own replay. In-memory like
/// [`crate::provider::auth_recovery`]'s budget — after a restart nothing is
/// mid-turn, and the reset itself is durable.
static REPLAYED: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Completion-listener entry point. Called for every settled turn, ahead of
/// [`crate::provider::auth_recovery::handle_completion`] — that one drops
/// the replay snapshot for every outcome it doesn't own, this one needs it.
pub async fn handle_completion(state: &Arc<AppState>, completion: &ProcessCompletion) {
    // A drain-only turn-end signal means the child is still alive and the
    // turn it reports on hasn't settled.
    if completion.turn_end_only || completion.error_kind != Some(CrashKind::ResumeFailed) {
        return;
    }
    let session_id = &completion.session_id;
    let reason = completion.error.clone().unwrap_or_default();

    let Ok(Some(session)) = state.db.get_session(session_id).await else {
        return;
    };

    // Drop the dead id both ways: the column, and the event-log fallback
    // that would otherwise hand the same id straight back.
    let updated = state
        .db
        .update_session(
            session_id,
            UpdateSession {
                conversation_id: Some(None),
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
            tracing::error!(
                session_id = %session_id,
                "Failed to clear a rejected conversation id: {e}"
            );
            return;
        }
    }

    let model = session.model.clone().unwrap_or_default();
    // One marker, two jobs: it stops the resume scan, and the chat renders
    // it as the notice explaining why the model lost the thread (the
    // `auth-parked` / `auth-resumed` shape — wording lives in the client).
    append_event(
        state,
        session_id,
        CONVERSATION_RESET_KIND,
        serde_json::json!({ "model": model, "reason": reason }),
    )
    .await;
    tracing::warn!(
        session_id = %session_id,
        model = %model,
        "Provider refused to resume this session's conversation; starting cold: {reason}"
    );

    let replay = state.session_manager.last_dispatched_turn(session_id).await;
    if session.is_worker {
        // The orchestrator owns a worker's prompt and re-dispatches the
        // card on its next tick — which now spawns cold, because the id is
        // gone. Nothing to replay here.
        return;
    }
    let Some(replay) = replay else { return };

    let budget_free = {
        let mut replayed = REPLAYED.lock().await;
        if replayed.get(session_id) == Some(&reason) {
            false
        } else {
            replayed.insert(session_id.to_string(), reason.clone());
            true
        }
    };
    if !budget_free {
        tracing::warn!(
            session_id = %session_id,
            "Resume failed again with the same error; not replaying a second time"
        );
        return;
    }

    // Queue it rather than dispatching here: the completion listener drains
    // the queue a step later, under the session lock, which is the one path
    // allowed to decide "is running → spawn". Nothing parks the queue on
    // this failure, so the drain delivers it in the same pass.
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
            "Failed to re-queue a turn after a rejected resume: {e}"
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

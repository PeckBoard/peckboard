//! Automatic recovery from a provider usage limit.
//!
//! A turn that ends with `rate_limit` ("You've hit your weekly limit ·
//! resets 8am (UTC)") did not fail for a reason a retry fixes now, and it
//! did not fail for good either: the provider says when it will work again.
//! So the account (for Claude, account + model) is held until then — see
//! [`crate::provider::rate_limits`] — and what was in flight continues by
//! itself at the reset:
//!
//! * **Workers** park nothing. The orchestrator owns their prompt; it skips
//!   a card whose model is held, marks it as waiting, and dispatches it on
//!   the first tick after the hold ends. Cards an older release blocked
//!   over a limit are unblocked by [`heal_limit_blocked_cards`].
//! * **Chat sessions** have their turn parked in the durable queue behind a
//!   [`LIMIT_PARKED_KIND`] marker. The queue drain leaves it alone while the
//!   hold stands; [`release_due`] delivers it when the hold ends, and
//!   [`retry_now`] on the user's say-so.
//! * **Subagents** park nothing either: the parent is told at once, with
//!   the provider's message, and decides — it can re-spawn on another model
//!   long before a weekly limit resets, and a parked copy resuming later
//!   would do the work twice.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::db::Db;
use crate::db::models::Event;
use crate::provider::agent::ProcessCompletion;
use crate::provider::manager::DispatchedTurn;
use crate::provider::stream::CrashKind;
use crate::state::AppState;
use crate::ws::broadcaster::WsEvent;

/// Appended to a chat session when a usage limit parked its turn. Carries
/// `model`, `until` (RFC 3339) and the provider's `reason`.
pub const LIMIT_PARKED_KIND: &str = "limit-parked";

/// Appended when the park is lifted. Carries `trigger`: `reset` (the hold
/// ran out), `cleared` (someone dropped the hold) or `manual` (Retry now).
pub const LIMIT_RESUMED_KIND: &str = "limit-resumed";

/// Completion-listener entry point. Called for every settled turn, before
/// auth recovery (which drops the replay snapshot this needs).
pub async fn handle_completion(state: &Arc<AppState>, completion: &ProcessCompletion) {
    if completion.turn_end_only || completion.error_kind != Some(CrashKind::RateLimit) {
        return;
    }
    let session_id = &completion.session_id;
    let Ok(Some(session)) = state.db.get_session(session_id).await else {
        return;
    };
    let replay = state.session_manager.last_dispatched_turn(session_id).await;
    // The turn's own model id when there is one: it carries the account the
    // turn actually ran on.
    let model = replay
        .as_ref()
        .map(|t| t.model.clone())
        .or_else(|| session.model.clone());
    let reason = completion.error.as_deref().unwrap_or("");
    let until = state
        .provider_registry
        .rate_limits
        .note(model.as_deref(), reason, Utc::now());

    if session.is_worker || session.parent_session_id.is_some() {
        return;
    }
    let Some(replay) = replay else {
        tracing::warn!(
            session_id = %session_id,
            "Turn hit a usage limit but no dispatched turn was recorded; nothing to replay"
        );
        return;
    };
    // A provider that keeps its child alive across the failed turn would
    // take the replay straight back into a process that already gave up.
    crate::provider::manager::terminate_after_turn_via_registry(
        &state.provider_registry,
        session_id,
    )
    .await;
    if let Err(e) = park_turn(state, session_id, &replay, until, reason).await {
        tracing::error!(session_id = %session_id, "Failed to park a turn after a usage limit: {e}");
        return;
    }
    tracing::warn!(
        session_id = %session_id,
        model = %replay.model,
        until = %until,
        "Turn hit a usage limit; parked it until the reset"
    );
}

/// Persist the turn in the durable queue so it survives a restart and is
/// delivered by the ordinary drain once the park lifts.
async fn park_turn(
    state: &Arc<AppState>,
    session_id: &str,
    turn: &DispatchedTurn,
    until: DateTime<Utc>,
    reason: &str,
) -> anyhow::Result<()> {
    let attachment_ids = if turn.attachment_ids.is_empty() {
        None
    } else {
        serde_json::to_string(&turn.attachment_ids).ok()
    };
    let row = state
        .db
        .enqueue_message(crate::db::models::NewQueuedMessage {
            session_id: session_id.to_string(),
            text: turn.text.clone(),
            queued_at: Utc::now().to_rfc3339(),
            model: Some(turn.model.clone()),
            effort: turn.effort.clone(),
            attachment_ids,
            // The transcript already shows this message where the user
            // typed it; the drain must not append a second copy.
            user_event_appended: true,
        })
        .await?;
    state.broadcaster.broadcast(WsEvent {
        event_type: "queue".into(),
        session_id: session_id.to_string(),
        data: serde_json::json!({ "action": "set", "id": row.id }),
    });
    append_marker(
        state,
        session_id,
        LIMIT_PARKED_KIND,
        serde_json::json!({
            "model": turn.model,
            "until": until.to_rfc3339(),
            "reason": reason.trim(),
        }),
    )
    .await;
    Ok(())
}

async fn append_marker(
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

/// The model a session's standing park names, or `None` when the session
/// is not parked on a limit.
async fn parked_model(db: &Db, session_id: &str) -> Option<String> {
    let ev = db
        .latest_event_of_kinds(session_id, &[LIMIT_PARKED_KIND, LIMIT_RESUMED_KIND])
        .await
        .ok()
        .flatten()?;
    if ev.kind != LIMIT_PARKED_KIND {
        return None;
    }
    let data: serde_json::Value = serde_json::from_str(&ev.data).ok()?;
    Some(
        data.get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string(),
    )
}

/// Whether the queue drain must leave `session_id`'s turn alone: it is
/// parked on a limit and the hold on its model still stands. A park whose
/// hold has ended (reset passed, or dropped by a user) is lifted right
/// here, so whichever drain looks first delivers the turn.
pub async fn is_holding(state: &Arc<AppState>, session_id: &str) -> bool {
    let Some(model) = parked_model(&state.db, session_id).await else {
        return false;
    };
    let model = (!model.is_empty()).then_some(model);
    if state
        .provider_registry
        .rate_limits
        .is_limited(model.as_deref(), Utc::now())
    {
        return true;
    }
    append_marker(
        state,
        session_id,
        LIMIT_RESUMED_KIND,
        serde_json::json!({ "trigger": "reset" }),
    )
    .await;
    false
}

/// Deliver every parked turn whose hold has ended. Called on the
/// orchestrator's 5-second tick; a bounded scan of the queue table and no
/// work beyond that when nothing is parked.
pub async fn release_due(state: &Arc<AppState>) {
    let queued = state
        .db
        .sessions_with_queued_messages()
        .await
        .unwrap_or_default();
    for session_id in queued {
        if parked_model(&state.db, &session_id).await.is_none() {
            continue;
        }
        // `is_holding` lifts an expired park; the drain then delivers.
        if is_holding(state, &session_id).await {
            continue;
        }
        tracing::info!(session_id = %session_id, "Usage limit reset; replaying the parked turn");
        if let Err(e) =
            crate::worker::orchestrator::drain_queue_for_session(state, &session_id).await
        {
            tracing::warn!(session_id = %session_id, "Limit release: drain failed: {e}");
        }
    }
}

/// Release one session on the user's say-so: drop the hold on its model and
/// deliver the parked turn. Returns false when the session wasn't parked.
pub async fn retry_now(state: &Arc<AppState>, session_id: &str) -> bool {
    let Some(model) = parked_model(&state.db, session_id).await else {
        return false;
    };
    if !model.is_empty() {
        state.provider_registry.rate_limits.clear(Some(&model));
    }
    append_marker(
        state,
        session_id,
        LIMIT_RESUMED_KIND,
        serde_json::json!({ "trigger": "manual" }),
    )
    .await;
    if let Err(e) = crate::worker::orchestrator::drain_queue_for_session(state, session_id).await {
        tracing::warn!(session_id = %session_id, "Manual limit retry: drain failed: {e}");
    }
    true
}

/// True when the newest `agent-end` in `events` (ascending `seq`) was a
/// usage-limit stop: either stamped `errorKind: rate_limit`, or — for turns
/// recorded before limit notices were recognised — a clean completion whose
/// last reply is a limit notice.
pub fn last_turn_hit_limit(events: &[Event]) -> bool {
    let Some(end_at) = events.iter().rposition(|e| e.kind == "agent-end") else {
        return false;
    };
    let data = serde_json::from_str::<serde_json::Value>(&events[end_at].data).ok();
    let kind = data
        .as_ref()
        .and_then(|d| d.get("errorKind"))
        .and_then(|k| k.as_str());
    if kind == Some(CrashKind::RateLimit.as_str()) {
        return true;
    }
    if kind.is_some() {
        return false;
    }
    events[..end_at]
        .iter()
        .rev()
        .take_while(|e| e.kind != "agent-start")
        .find(|e| e.kind == "agent-text")
        .and_then(|e| serde_json::from_str::<serde_json::Value>(&e.data).ok())
        .and_then(|d| d.get("text").and_then(|t| t.as_str()).map(str::to_string))
        .is_some_and(|text| crate::provider::rate_limits::is_limit_stop(&text))
}

/// Unblock cards that were blocked automatically (crash guard or the
/// no-progress guard) when the failure behind the block was a usage limit.
/// A limit lifts by itself, so such a card must not wait for a human; the
/// orchestrator holds it while its model is limited and dispatches it at
/// the reset. Blocks a person set, and automatic blocks with any other
/// last failure, are left alone. Runs at boot: it heals cards an older
/// release blocked before limit notices were recognised.
pub async fn heal_limit_blocked_cards(state: &Arc<AppState>) {
    let Ok(projects) = state.db.list_projects().await else {
        return;
    };
    for project in projects {
        let Ok(cards) = state.db.list_cards_by_project(&project.id).await else {
            continue;
        };
        for card in cards.iter().filter(|c| c.blocked) {
            let Some(session_id) = card.last_worker_session_id.as_deref() else {
                continue;
            };
            let Ok(events) = state
                .db
                .list_events_by_session_before(session_id, None, 40)
                .await
            else {
                continue;
            };
            if last_turn_hit_limit(&events)
                && crate::worker::orchestrator::clear_automatic_block(state, card).await
            {
                tracing::info!(
                    card_id = %card.id,
                    "Unblocked a card that had been blocked over a usage limit"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, data: &str) -> Event {
        Event {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: "s".into(),
            seq: 0,
            ts: 0,
            kind: kind.into(),
            data: data.into(),
        }
    }

    #[test]
    fn a_limit_stop_is_recognised_stamped_or_not() {
        // Stamped by the current host.
        assert!(last_turn_hit_limit(&[
            event("agent-start", "{}"),
            event(
                "agent-end",
                r#"{"status":"complete","error":"x","errorKind":"rate_limit"}"#
            ),
        ]));
        // Recorded by an older release: a clean completion after the notice.
        assert!(last_turn_hit_limit(&[
            event("agent-start", "{}"),
            event(
                "agent-text",
                r#"{"text":"You've hit your weekly limit · resets 8am (UTC)"}"#
            ),
            event("agent-end", r#"{"status":"complete"}"#),
            event("worker-no-progress", "{}"),
        ]));
        // A real answer, and a different failure, are not limit stops.
        assert!(!last_turn_hit_limit(&[
            event("agent-start", "{}"),
            event("agent-text", r#"{"text":"Done."}"#),
            event("agent-end", r#"{"status":"complete"}"#),
        ]));
        assert!(!last_turn_hit_limit(&[
            event(
                "agent-text",
                r#"{"text":"You've hit your weekly limit · resets 8am (UTC)"}"#
            ),
            event(
                "agent-end",
                r#"{"status":"crashed","errorKind":"auth_expired"}"#
            ),
        ]));
        // The notice belongs to an earlier turn, not the last one.
        assert!(!last_turn_hit_limit(&[
            event(
                "agent-text",
                r#"{"text":"You've hit your weekly limit · resets 8am (UTC)"}"#
            ),
            event("agent-end", r#"{"status":"complete"}"#),
            event("agent-start", "{}"),
            event("agent-text", r#"{"text":"Back at work."}"#),
            event("agent-end", r#"{"status":"complete"}"#),
        ]));
    }
}

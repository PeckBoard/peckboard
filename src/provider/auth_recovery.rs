//! Automatic recovery from expired or revoked provider credentials.
//!
//! A 401 is the one agent failure a retry can genuinely fix — but only
//! from a process that reads the *current* credential, and only once
//! something has changed. Both halves used to be missing:
//!
//! * the CLI reads its token from the env at spawn, so a long-lived child
//!   kept presenting a revoked token for up to the idle-reap window even
//!   after the user pasted a fresh one. The dispatcher now fingerprints
//!   the credential each turn resolves and recycles a child whose
//!   fingerprint no longer matches (see
//!   `ClaudeRun::credential_fingerprint`), and the Claude stream loop
//!   winds its child down the moment a turn fails to authenticate;
//! * nothing replayed the turn the failed dispatch had already consumed.
//!
//! This module is that second half. On an `auth_expired` completion it
//! parks the user's turn in the durable queue and marks the session with
//! an [`AUTH_PARKED_KIND`] event. The queue drain refuses to deliver
//! while that marker stands, so a dead token can't tarpit the session in
//! a park-drain-401 spin. The park is lifted:
//!
//! * immediately, once per credential version — the common shape of this
//!   failure is a stored token that was already fine and a live child
//!   that was stale, and that heals with no user action at all;
//! * when the account's credential changes (re-login, pasted secret,
//!   silent refresh), which also resumes any project that auto-paused on
//!   the same failure — see [`release_for_account`];
//! * on demand, from the "Retry now" button in the chat's auth banner —
//!   see [`retry_now`].
//!
//! Workers park nothing: the orchestrator owns their prompt and
//! re-dispatches the card by itself. What a worker needs is its project
//! un-paused, which [`release_for_account`] does.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use tokio::sync::Mutex;

use crate::db::Db;
use crate::db::models::Event;
use crate::provider::agent::ProcessCompletion;
use crate::provider::manager::DispatchedTurn;
use crate::provider::stream::CrashKind;
use crate::state::AppState;
use crate::ws::broadcaster::WsEvent;

/// Appended to a session when an auth failure parked its turn. While this
/// is the newer of the two markers the queue drain leaves the turn alone.
pub const AUTH_PARKED_KIND: &str = "auth-parked";

/// Appended when the park is lifted. Carries `trigger` — `auto-retry`,
/// `account-updated` or `manual` — so the transcript says why the turn
/// started moving again.
pub const AUTH_RESUMED_KIND: &str = "auth-resumed";

/// Session id → the credential version its automatic replay was spent on.
///
/// Budgets the free retry to one per session per credential: a genuinely
/// dead token fails the replay too, and that second failure finds the
/// budget already spent and leaves the turn parked instead of looping. A
/// new credential (or a manual retry) renews it. In-memory on purpose —
/// after a restart nothing is running to retry, and the park itself is
/// durable.
static AUTO_RETRIED: LazyLock<Mutex<HashMap<String, i64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Completion-listener entry point. Called for every settled turn.
///
/// Non-auth outcomes just drop the replay snapshot the dispatcher kept;
/// an `auth_expired` one goes through [`handle_auth_failure`].
pub async fn handle_completion(state: &Arc<AppState>, completion: &ProcessCompletion) {
    // A drain-only turn-end signal means the child is still alive and the
    // turn it reports on hasn't settled — nothing to recover from.
    if completion.turn_end_only {
        return;
    }
    let session_id = &completion.session_id;
    if completion.error_kind != Some(CrashKind::AuthExpired) {
        state
            .session_manager
            .forget_dispatched_turn(session_id)
            .await;
        return;
    }
    handle_auth_failure(state, session_id).await;
}

async fn handle_auth_failure(state: &Arc<AppState>, session_id: &str) {
    let Ok(Some(session)) = state.db.get_session(session_id).await else {
        return;
    };

    // Defensive: the Claude loop already exits on an auth failure, so by
    // now there is usually nothing to wind down. A provider that keeps its
    // child alive across a failed turn would otherwise take the replay
    // straight back into the process holding the dead token.
    crate::provider::manager::terminate_after_turn_via_registry(
        &state.provider_registry,
        session_id,
    )
    .await;

    let replay = state.session_manager.last_dispatched_turn(session_id).await;
    state
        .session_manager
        .forget_dispatched_turn(session_id)
        .await;

    if session.is_worker {
        // The orchestrator owns a worker's prompt: it re-dispatches the
        // card on its next tick (that IS the automatic restart, and it now
        // spawns under a current credential), and the crash counter
        // auto-pauses the project once the failure repeats. Nothing to
        // park; `release_for_account` un-pauses when a login lands.
        tracing::warn!(
            session_id = %session_id,
            card_id = session.card_id.as_deref().unwrap_or("-"),
            "Worker turn failed to authenticate"
        );
        return;
    }

    let Some(replay) = replay else {
        tracing::warn!(
            session_id = %session_id,
            "Turn failed to authenticate but no dispatched turn was recorded; nothing to replay"
        );
        return;
    };

    if let Err(e) = park_turn(state, session_id, &replay).await {
        tracing::error!(
            session_id = %session_id,
            "Failed to park a turn after an auth failure: {e}"
        );
        return;
    }
    tracing::warn!(
        session_id = %session_id,
        model = %replay.model,
        "Turn failed to authenticate; parked it pending a working login"
    );

    let version = credential_version(&state.db, &replay.model).await;
    let budget_free = {
        let mut retried = AUTO_RETRIED.lock().await;
        if retried.get(session_id) == Some(&version) {
            false
        } else {
            retried.insert(session_id.to_string(), version);
            true
        }
    };
    if budget_free {
        // Released in the same listener pass, so the drain that runs right
        // after this delivers the replay into a freshly spawned child.
        release_session(state, session_id, "auto-retry").await;
    }
}

/// Persist the turn in the durable queue so it survives a restart and can
/// be delivered by the ordinary drain once the park lifts.
async fn park_turn(
    state: &Arc<AppState>,
    session_id: &str,
    turn: &DispatchedTurn,
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
            queued_at: chrono::Utc::now().to_rfc3339(),
            // The full model id, `@account` suffix intact — a replay must
            // land on the account the turn was billed to.
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
        AUTH_PARKED_KIND,
        serde_json::json!({ "model": turn.model }),
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

/// Whether `session_id`'s turn is parked waiting on a working login.
///
/// Read by the queue drain before it delivers anything. `false` for every
/// session that has never hit an auth failure — the lookup is one indexed
/// row, not a transcript scan.
pub async fn is_parked(db: &Db, session_id: &str) -> bool {
    matches!(
        db.latest_event_of_kinds(session_id, &[AUTH_PARKED_KIND, AUTH_RESUMED_KIND])
            .await,
        Ok(Some(ev)) if ev.kind == AUTH_PARKED_KIND
    )
}

/// Lift the park so the next drain delivers. Does not dispatch — the
/// caller decides whether the drain runs now (a release triggered outside
/// the completion listener) or on the pass already underway.
pub async fn release_session(state: &Arc<AppState>, session_id: &str, trigger: &str) {
    append_marker(
        state,
        session_id,
        AUTH_RESUMED_KIND,
        serde_json::json!({ "trigger": trigger }),
    )
    .await;
}

/// Release one session on the user's say-so and deliver its parked turn.
/// Returns false when the session wasn't parked, which the route reports
/// as a 409 rather than pretending it retried something.
pub async fn retry_now(state: &Arc<AppState>, session_id: &str) -> bool {
    if !is_parked(&state.db, session_id).await {
        return false;
    }
    // An explicit retry renews the automatic budget: the user is telling
    // us something changed that we can't see (a host login, a proxy fix).
    AUTO_RETRIED.lock().await.remove(session_id);
    release_session(state, session_id, "manual").await;
    if let Err(e) = crate::worker::orchestrator::drain_queue_for_session(state, session_id).await {
        tracing::warn!(session_id = %session_id, "Manual auth retry: drain failed: {e}");
    }
    true
}

/// A credential for `account_id` on `provider_id` just changed: replay
/// every turn parked against it, and un-pause every project that stopped
/// for the same reason.
///
/// Called from the account create/update routes and from the silent OAuth
/// refresh. Safe to call when nothing is parked — it does a bounded scan
/// of the queue table and the paused projects, and no work beyond that.
pub async fn release_for_account(state: &Arc<AppState>, provider_id: &str, account_id: &str) {
    let parked: Vec<String> = state
        .db
        .sessions_with_queued_messages()
        .await
        .unwrap_or_default();
    for session_id in parked {
        if !is_parked(&state.db, &session_id).await {
            continue;
        }
        let Ok(Some(session)) = state.db.get_session(&session_id).await else {
            continue;
        };
        if !session_uses(session.model.as_deref(), provider_id, account_id) {
            continue;
        }
        AUTO_RETRIED.lock().await.remove(&session_id);
        release_session(state, &session_id, "account-updated").await;
        tracing::info!(
            session_id = %session_id,
            account = %account_id,
            "Credential updated; replaying the turn parked on it"
        );
        if let Err(e) =
            crate::worker::orchestrator::drain_queue_for_session(state, &session_id).await
        {
            tracing::warn!(session_id = %session_id, "Auth release: drain failed: {e}");
        }
    }
    resume_auth_paused_projects(state, provider_id, account_id).await;
}

/// Un-pause projects whose auto-pause traces back to an auth failure on
/// this account. The orchestrator picks their cards back up on its next
/// tick.
async fn resume_auth_paused_projects(state: &Arc<AppState>, provider_id: &str, account_id: &str) {
    let Ok(projects) = state.db.list_projects().await else {
        return;
    };
    for project in projects.into_iter().filter(|p| p.status == "paused") {
        if !paused_on_auth(state, &project.id, provider_id, account_id).await {
            continue;
        }
        match crate::routes::projects::resume_project_inner(state, &project.id).await {
            Ok(Some(_)) => tracing::info!(
                project_id = %project.id,
                account = %account_id,
                "Resumed a project that had auto-paused on expired credentials"
            ),
            Ok(None) => {}
            Err(e) => tracing::warn!(
                project_id = %project.id,
                "Auth release: failed to resume project: {e}"
            ),
        }
    }
}

/// Whether this project's pause traces back to an auth failure on
/// `account_id`.
///
/// Derived from the event log rather than stored on the project: the last
/// turn of one of its worker sessions ended `errorKind: auth_expired`, and
/// that session runs on this account. A pause with any other last failure
/// is left alone — a new login is not evidence that a crashing card is
/// fixed.
async fn paused_on_auth(
    state: &Arc<AppState>,
    project_id: &str,
    provider_id: &str,
    account_id: &str,
) -> bool {
    let Ok(sessions) = state.db.list_worker_sessions_by_project(project_id).await else {
        return false;
    };
    for session in sessions {
        if !session_uses(session.model.as_deref(), provider_id, account_id) {
            continue;
        }
        let Ok(events) = state
            .db
            .list_events_by_session_before(&session.id, None, 40)
            .await
        else {
            continue;
        };
        if last_turn_failed_auth(&events) {
            return true;
        }
    }
    false
}

/// True when the newest `agent-end` in `events` reported an auth failure.
/// `events` is in ascending `seq` order, as every listing helper returns.
pub fn last_turn_failed_auth(events: &[Event]) -> bool {
    events
        .iter()
        .rev()
        .find(|e| e.kind == "agent-end")
        .and_then(|e| serde_json::from_str::<serde_json::Value>(&e.data).ok())
        .and_then(|d| {
            d.get("errorKind")
                .and_then(|k| k.as_str())
                .map(str::to_string)
        })
        .is_some_and(|kind| kind == CrashKind::AuthExpired.as_str())
}

/// Whether a session on `model` authenticates with this account.
///
/// A model with no `@account` suffix runs on the provider's Default/host
/// credentials, which have no row to compare against — so any credential
/// change on that provider counts as a possible fix for it. Guessing wrong
/// there costs one extra attempt; guessing the other way strands the
/// session.
fn session_uses(model: Option<&str>, provider_id: &str, account_id: &str) -> bool {
    let Some(model) = model else {
        return false;
    };
    if model_provider(model) != provider_id {
        return false;
    }
    match crate::provider::registry::split_model_account(model).1 {
        Some(acct) => acct == account_id,
        None => true,
    }
}

/// Provider id a model string routes to — the `provider:` prefix, or the
/// default when it has none (the same rule the dispatcher applies).
fn model_provider(model: &str) -> &str {
    model
        .split_once(':')
        .map(|(p, _)| p)
        .unwrap_or(crate::provider::manager::DEFAULT_PROVIDER)
}

/// Current version of the credential a model authenticates with: the
/// account row's `updated_at`, which every login, pasted secret and silent
/// refresh bumps.
///
/// `0` for the Default/host account and for an account that no longer
/// exists — nothing observable changes there, so the automatic replay is
/// spent once and not renewed.
async fn credential_version(db: &Db, model: &str) -> i64 {
    let Some(account_id) = crate::provider::registry::split_model_account(model).1 else {
        return 0;
    };
    match model_provider(model) {
        "grok" => db
            .get_grok_account(account_id)
            .await
            .ok()
            .flatten()
            .map(|a| a.updated_at),
        "kimi" => db
            .get_kimi_account(account_id)
            .await
            .ok()
            .flatten()
            .map(|a| a.updated_at),
        _ => db
            .get_claude_account(account_id)
            .await
            .ok()
            .flatten()
            .map(|a| a.updated_at),
    }
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, data: &str) -> Event {
        Event {
            id: format!("ev_{kind}"),
            session_id: "s1".into(),
            seq: 1,
            ts: 0,
            kind: kind.into(),
            data: data.into(),
        }
    }

    #[test]
    fn last_turn_failed_auth_reads_the_newest_agent_end() {
        // Newest agent-end wins: an auth failure followed by a clean turn
        // is a project that recovered on its own, not one to resume.
        let events = vec![
            event(
                "agent-end",
                r#"{"status":"complete","errorKind":"auth_expired"}"#,
            ),
            event("agent-end", r#"{"status":"complete"}"#),
        ];
        assert!(!last_turn_failed_auth(&events));

        let events = vec![
            event("agent-end", r#"{"status":"complete"}"#),
            event(
                "agent-end",
                r#"{"status":"complete","errorKind":"auth_expired"}"#,
            ),
        ];
        assert!(last_turn_failed_auth(&events));
    }

    #[test]
    fn last_turn_failed_auth_ignores_other_failures() {
        // A rate limit or a plain crash is not something a new login
        // fixes — resuming those would re-enter the crash loop the
        // auto-pause exists to stop.
        let events = vec![event(
            "agent-end",
            r#"{"status":"crashed","errorKind":"rate_limit"}"#,
        )];
        assert!(!last_turn_failed_auth(&events));
        assert!(!last_turn_failed_auth(&[]));
        assert!(!last_turn_failed_auth(&[event("user", r#"{"text":"hi"}"#)]));
    }

    #[test]
    fn session_uses_matches_the_account_the_model_binds_to() {
        assert!(session_uses(
            Some("claude:claude-opus-4-8@acc_1"),
            "claude",
            "acc_1"
        ));
        assert!(!session_uses(
            Some("claude:claude-opus-4-8@acc_2"),
            "claude",
            "acc_1"
        ));
        // Bare model id — the implicit Default account, released by any
        // credential change on the same provider.
        assert!(session_uses(Some("claude-opus-4-8"), "claude", "acc_1"));
        // Different provider entirely.
        assert!(!session_uses(Some("grok:grok-4@acc_1"), "claude", "acc_1"));
        assert!(!session_uses(None, "claude", "acc_1"));
    }
}

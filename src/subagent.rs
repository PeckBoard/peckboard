//! Subagent sessions: any provider's session can spawn a child session via
//! the `spawn_subagent` MCP tool and get the child's final message posted
//! back automatically when it completes.
//!
//! Claude sessions also have the CLI's native Task tool; this path is the
//! provider-independent equivalent (grok / ollama / cursor have no native
//! subagent mechanism) and, unlike Task, the child is an ordinary persisted
//! session — readable with `read_worker_session`, terminable, restartable.
//!
//! Lifecycle:
//! 1. `spawn_subagent` (`service/mcp_server/handlers/subagents.rs`) creates
//!    the child row (`expert_kind = "subagent"`, `parent_session_id` set),
//!    persists the prompt as the child's first `user` event, and returns a
//!    `_dispatch_session` marker that the `mcp` route (which holds the
//!    `AppState`) turns into an `ExpertDispatcher::resume_session` call.
//! 2. Every provider emits a `ProcessCompletion`; the completion listener in
//!    `main.rs` calls [`handle_subagent_done`] for sessions with a parent
//!    link.
//! 3. [`handle_subagent_done`] claims the completion (idempotent), pulls the
//!    child's final reply, and delivers it to the parent exactly like a user
//!    message (spawn if idle, queue/inject if running). A child that ended
//!    its turn with `run_background` tasks still running is not done: the
//!    claim waits for the turn the last task's exit report resumes.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use crate::state::AppState;
use crate::ws::broadcaster::WsEvent;

/// `sessions.expert_kind` value marking a subagent session.
pub const SUBAGENT_EXPERT_KIND: &str = "subagent";

/// Default max subagents a parent may have in flight at once (rows with
/// `subagent_completed_at IS NULL`), when no `subagent_limits` override is
/// stored. See [`load_limits`].
pub const DEFAULT_MAX_CONCURRENT_SUBAGENTS: i64 = 25;

/// Prefix for subagent session names, so they read as children in listings.
pub const SUBAGENT_NAME_PREFIX: &str = "sub: ";

/// Default cap on the result text reported back to the parent; longer
/// finals are tail-truncated (the parent can read the full transcript with
/// `read_worker_session`). See [`load_limits`].
pub const DEFAULT_RESULT_CHAR_CAP: usize = 12_000;

/// Plugin-store namespace/collection/key for the subagent limits override
/// (`{"max_concurrent": i64, "result_char_cap": usize}`, both optional;
/// missing/rotted falls back to the `DEFAULT_*` constants). Same store the
/// other app-wide settings (`default_model`, caveman mode, ...) live in.
const SUBAGENT_LIMITS_KEY: &str = "subagent_limits";

/// Effective subagent limits: the concurrent-subagent cap and the
/// result-text truncation cap. Configurable via the `subagent_limits`
/// plugin-store setting; no UI yet (settings JSON only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubagentLimits {
    pub max_concurrent: i64,
    pub result_char_cap: usize,
}

impl Default for SubagentLimits {
    fn default() -> Self {
        Self {
            max_concurrent: DEFAULT_MAX_CONCURRENT_SUBAGENTS,
            result_char_cap: DEFAULT_RESULT_CHAR_CAP,
        }
    }
}

/// Load the effective subagent limits, clamping stored overrides to sane
/// bounds so a bad manual edit can't zero out the cap or blow up the result
/// text. Missing setting, rotted JSON, or a store error all fall back to
/// the defaults.
pub async fn load_limits(db: &crate::db::Db) -> SubagentLimits {
    let db = db.clone();
    let raw = tokio::task::spawn_blocking(move || {
        db.plugin_store_get_blocking(
            crate::routes::settings::SETTINGS_NS,
            crate::routes::settings::SETTINGS_COLLECTION,
            SUBAGENT_LIMITS_KEY,
        )
    })
    .await;
    let Ok(Ok(Some(json))) = raw else {
        return SubagentLimits::default();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&json) else {
        return SubagentLimits::default();
    };
    let defaults = SubagentLimits::default();
    let max_concurrent = value
        .get("max_concurrent")
        .and_then(|v| v.as_i64())
        .filter(|n| *n >= 1)
        .unwrap_or(defaults.max_concurrent);
    let result_char_cap = value
        .get("result_char_cap")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .filter(|n| (500..=200_000).contains(n))
        .unwrap_or(defaults.result_char_cap);
    SubagentLimits {
        max_concurrent,
        result_char_cap,
    }
}

/// Serialises the cap check and the child insert of [`create_capped`]:
/// without it, concurrent `spawn_subagent` calls all read `active < max`
/// before any of them inserts, and the parent ends up over its cap. Global
/// rather than per-parent — both statements are quick single-row queries.
static SPAWN_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Create the subagent row `new` (its `parent_session_id` names the parent)
/// only if that parent has fewer than `max_concurrent` subagents in flight.
/// The count and the insert run under one lock, so concurrent spawns can't
/// overshoot the cap.
pub async fn create_capped(
    db: &crate::db::Db,
    new: crate::db::models::NewSession,
    max_concurrent: i64,
) -> anyhow::Result<crate::db::models::Session> {
    let parent = new
        .parent_session_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("subagent row has no parent_session_id"))?;
    let _guard = SPAWN_LOCK.lock().await;
    let active = db.count_active_subagents(&parent).await?;
    if active >= max_concurrent {
        anyhow::bail!(
            "subagent limit reached ({active} in flight, max {max_concurrent}). Results arrive \
             automatically as they finish; peek with list_sessions / read_worker_session."
        );
    }
    db.create_session(new).await
}

/// Preamble + task for a subagent's first turn. Restates the standing
/// Peckboard rules so non-Claude subagents get them even though their
/// provider has no hook mechanism (the child's own spawn also carries the
/// full Peckboard system prompt; on Claude the SubagentStart hook covers
/// nested Task agents).
pub fn build_subagent_prompt(name: &str, parent_session_id: &str, task: &str) -> String {
    format!(
        "You are subagent \"{name}\", spawned by session {parent_session_id}. \
         Work the task below to completion, then STOP: your final message is \
         the deliverable and is posted back to the session that spawned you. \
         Do not ask the user questions — put open questions in the final \
         message. Never use terminal/shell tools (use `run_command` / \
         `run_tests` / `git`), never use `grep`/`sed` (use `search_files` and \
         the code tools), stay inside the project folder, and do not spawn \
         further subagents.\n\n# Task\n\n{task}"
    )
}
/// How long a subagent whose turn failed to authenticate gets to resume
/// (auth recovery's auto-retry, a re-login) before the parent is told it
/// crashed. Auto-retry replays within a second or two.
pub const AUTH_CRASH_GRACE: Duration = Duration::from_secs(60);

/// Children reported CRASHED for an auth failure, so a later release of
/// their park knows to un-claim them and announce the resume. In-memory:
/// after a restart `crate::restart_resume` re-decides every incomplete child.
static AUTH_CRASH_REPORTED: LazyLock<std::sync::Mutex<HashSet<String>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashSet::new()));

/// Report a completed (or crashed) subagent back to its parent session.
/// Idempotent via `claim_subagent_completion`; called from the completion
/// listener for every session that carries a `parent_session_id`.
///
/// A completed turn with background tasks still running is skipped (no
/// claim, slot kept): each task's exit report resumes the child, and the
/// completion of that turn re-runs this check. A crash reports at once and
/// silently stops the child's tasks, so a later exit can't wake a child
/// whose result has already been delivered.
///
/// `auth_failed` (the turn died on an expired/revoked credential) defers
/// the crash report: auth recovery parks the turn and usually replays it
/// straight away, so the child is not dead. The report goes out only if
/// the child has not started another turn within [`AUTH_CRASH_GRACE`].
pub async fn handle_subagent_done(
    state: &Arc<AppState>,
    session: &crate::db::models::Session,
    completed: bool,
    auth_failed: bool,
    error: Option<&str>,
) {
    if !completed && auth_failed {
        defer_auth_crash(state, session, error).await;
        return;
    }
    let background = crate::background::global();
    if !completed && let Some(bg) = &background {
        bg.stop_session_silently(&session.id);
    }
    // Pending work = a running background task, or a message (typically a
    // task's exit report that landed mid-turn) waiting in the durable queue:
    // the drain after this listener starts another turn, whose result must
    // not be dropped by an early claim.
    let has_running_tasks = completed
        && (background.is_some_and(|bg| bg.has_running_for_session(&session.id))
            || matches!(state.db.next_queued_message(&session.id).await, Ok(Some(_))));
    let Some((parent_id, text)) =
        claim_and_compose(&state.db, session, completed, error, has_running_tasks).await
    else {
        return;
    };
    deliver_to_parent(state, &session.id, &parent_id, &text).await;
}

/// Spawn the grace-window check behind an auth-failed subagent turn. The
/// baseline is read here, before the listener's auth recovery step parks
/// and replays the turn, so the replay's `agent-start` counts as a resume.
async fn defer_auth_crash(
    state: &Arc<AppState>,
    session: &crate::db::models::Session,
    error: Option<&str>,
) {
    let baseline = auth_failure_baseline(&state.db, &session.id).await;
    let state = state.clone();
    let session_id = session.id.clone();
    let error = error.map(str::to_string);
    tokio::spawn(async move {
        tokio::time::sleep(AUTH_CRASH_GRACE).await;
        let Ok(Some(session)) = state.db.get_session(&session_id).await else {
            return;
        };
        let Some((parent_id, text)) =
            claim_after_auth_grace(&state.db, &session, baseline, error.as_deref()).await
        else {
            return;
        };
        if let Some(bg) = crate::background::global() {
            bg.stop_session_silently(&session_id);
        }
        if let Ok(mut set) = AUTH_CRASH_REPORTED.lock() {
            set.insert(session_id.clone());
        }
        deliver_to_parent(&state, &session_id, &parent_id, &text).await;
    });
}

/// Broadcast the child's updated row (its `subagent_completed_at` changed),
/// then persist `text` on the parent and resume it like a user message.
async fn deliver_to_parent(state: &Arc<AppState>, child_id: &str, parent_id: &str, text: &str) {
    if let Ok(Some(child)) = state.db.get_session(child_id).await {
        state.broadcaster.broadcast(WsEvent {
            event_type: "session-updated".into(),
            session_id: child.id.clone(),
            data: serde_json::to_value(&child).unwrap_or(serde_json::Value::Null),
        });
    }
    let dispatcher = crate::service::mcp_server::AppExpertDispatcher::new(state.clone());
    if let Err(e) = crate::service::session_notify::notify_session(
        &state.db,
        &state.broadcaster,
        Some(&dispatcher),
        parent_id,
        text,
        serde_json::json!({ "source": "subagent-result" }),
    )
    .await
    {
        tracing::warn!(parent_session_id = %parent_id, "subagent result event append failed: {e}");
    }
}

/// Report a subagent's *first-turn dispatch* failure (the async
/// `ExpertDispatcher::resume_session` call the `mcp` route fires off right
/// after `spawn_subagent` returns). Without this, a dispatch failure only
/// logged a `tracing::warn!` and left `subagent_completed_at` NULL forever,
/// so `count_active_subagents` counted the row until server restart
/// (`crate::restart_resume::plan`) — eventually exhausting the parent's
/// concurrency slots. Delegates to [`handle_subagent_done`] so the slot is
/// freed and the parent gets the same CRASHED report shape as a real crash.
pub async fn fail_subagent_dispatch(state: &Arc<AppState>, child_id: &str, error: &str) {
    let session = match state.db.get_session(child_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            tracing::warn!(session_id = %child_id, "subagent dispatch failure: child session not found");
            return;
        }
        Err(e) => {
            tracing::warn!(session_id = %child_id, "subagent dispatch failure: session lookup failed: {e}");
            return;
        }
    };
    handle_subagent_done(state, &session, false, false, Some(error)).await;
}

/// The DB half of [`handle_subagent_done`], separated so it is testable
/// without an `AppState`: claim the completion (idempotent) and compose the
/// report text. Returns `(parent_session_id, text)` only for the call that
/// won the claim while the parent still exists. `has_running_tasks` (the
/// child still owns running background tasks) defers a *completed* turn:
/// nothing is claimed, so a later completion can still report.
pub async fn claim_and_compose(
    db: &crate::db::Db,
    session: &crate::db::models::Session,
    completed: bool,
    error: Option<&str>,
    has_running_tasks: bool,
) -> Option<(String, String)> {
    let parent_id = session.parent_session_id.as_deref()?;
    if completed && has_running_tasks {
        return None; // not done: a task's exit report will resume it
    }
    let now = chrono::Utc::now().to_rfc3339();
    match db.claim_subagent_completion(&session.id, &now).await {
        Ok(true) => {}
        Ok(false) => return None, // already reported (listener re-fire)
        Err(e) => {
            tracing::warn!(session_id = %session.id, "subagent completion claim failed: {e}");
            return None;
        }
    }

    // The parent may have been deleted while the child ran.
    if !matches!(db.get_session(parent_id).await, Ok(Some(_))) {
        tracing::warn!(
            session_id = %session.id,
            parent_session_id = %parent_id,
            "subagent finished but its parent session is gone; dropping result"
        );
        return None;
    }

    let name = session
        .name
        .strip_prefix(SUBAGENT_NAME_PREFIX)
        .unwrap_or(&session.name);
    let text = if completed {
        let reply = final_reply(db, &session.id).await;
        let body = if reply.is_empty() {
            "(no final message; read its transcript with read_worker_session)".to_string()
        } else {
            reply
        };
        format!(
            "[subagent \"{name}\" ({id}) finished]\n\n{body}",
            id = session.id
        )
    } else {
        let detail = crash_detail(
            error,
            last_crashed_agent_end(db, &session.id).await.as_ref(),
        );
        format!(
            "[subagent \"{name}\" ({id}) CRASHED]\n\n{detail}\n\nRead its transcript with \
             read_worker_session, then re-spawn it or continue without it.",
            id = session.id,
        )
    };
    Some((parent_id.to_string(), text))
}

/// Data of the child's most recent crashed `agent-end`, if any.
async fn last_crashed_agent_end(db: &crate::db::Db, session_id: &str) -> Option<serde_json::Value> {
    let events = db.events_tail(session_id, 16).await.ok()?;
    events
        .iter()
        .rev()
        .filter(|e| e.kind == "agent-end")
        .filter_map(|e| serde_json::from_str::<serde_json::Value>(&e.data).ok())
        .find(|d| d.get("status").and_then(|s| s.as_str()) == Some("crashed"))
}

/// Max stderr characters quoted in a crash report (the tail is kept).
const CRASH_STDERR_TAIL: usize = 2000;

/// The crash report body: the completion's `error`, else the reason on the
/// child's last crashed `agent-end` (an interrupt or server shutdown has no
/// completion error), plus that event's exit code and stderr tail.
fn crash_detail(error: Option<&str>, agent_end: Option<&serde_json::Value>) -> String {
    let field = |k: &str| agent_end.and_then(|d| d.get(k));
    let mut out = error
        .or_else(|| field("reason").and_then(|v| v.as_str()))
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("no error detail")
        .to_string();
    if let Some(code) = field("exitCode").and_then(|v| v.as_i64()) {
        out.push_str(&format!("\n\nexit code: {code}"));
    }
    let stderr = field("stderr")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    if !stderr.is_empty() && !out.contains(stderr) {
        let skip = stderr.chars().count().saturating_sub(CRASH_STDERR_TAIL);
        let tail: String = stderr.chars().skip(skip).collect();
        out.push_str(&format!("\n\nstderr (tail):\n```\n{tail}\n```"));
    }
    out
}

/// The child's final reply: every `agent-text` event after the last `user`
/// Seq of the child's newest `agent-start` at the moment an auth failure
/// ended its turn. [`claim_after_auth_grace`] treats any later start as
/// proof the child resumed.
pub async fn auth_failure_baseline(db: &crate::db::Db, session_id: &str) -> Option<i32> {
    db.latest_event_of_kinds(session_id, &["agent-start"])
        .await
        .ok()
        .flatten()
        .map(|e| e.seq)
}

/// The DB half of the deferred auth-crash report, run once
/// [`AUTH_CRASH_GRACE`] has lapsed: `None` when the child started another
/// turn since `baseline_start_seq` (auto-retry resumed it — its own
/// completion reports), otherwise the usual crash claim + CRASHED text.
pub async fn claim_after_auth_grace(
    db: &crate::db::Db,
    session: &crate::db::models::Session,
    baseline_start_seq: Option<i32>,
    error: Option<&str>,
) -> Option<(String, String)> {
    if auth_failure_baseline(db, &session.id).await > baseline_start_seq {
        return None;
    }
    claim_and_compose(db, session, false, error, false).await
}

/// Auth recovery just lifted `session_id`'s park. If that child was
/// already reported CRASHED for the auth failure, un-claim it (so its
/// eventual result reports as `finished`) and tell the parent it is alive
/// again — otherwise the parent may re-spawn a child that is still
/// working.
pub async fn on_auth_released(state: &Arc<AppState>, session_id: &str) {
    let reported = AUTH_CRASH_REPORTED
        .lock()
        .map(|mut set| set.remove(session_id))
        .unwrap_or(false);
    if !reported {
        return;
    }
    let Ok(Some(session)) = state.db.get_session(session_id).await else {
        return;
    };
    let Some(parent_id) = session.parent_session_id.clone() else {
        return;
    };
    if !matches!(
        state.db.unclaim_subagent_completion(session_id).await,
        Ok(true)
    ) {
        return;
    }
    if !matches!(state.db.get_session(&parent_id).await, Ok(Some(_))) {
        return;
    }
    let name = session
        .name
        .strip_prefix(SUBAGENT_NAME_PREFIX)
        .unwrap_or(&session.name);
    let text = format!(
        "[subagent \"{name}\" ({id}) resumed]\n\nIts CRASHED report above was an \
         authentication failure; the turn has been retried and the subagent is \
         running again. Do not re-spawn it — its result is posted here when it \
         finishes.",
        id = session.id
    );
    deliver_to_parent(state, session_id, &parent_id, &text).await;
}

/// event, folded the way the chat UI folds them: consecutive rows are
/// chunks of ONE streamed message (the Claude parser flushes coalesced
/// deltas as separate rows) and concatenate verbatim; a paragraph break is
/// added only where a non-text event (tool call, thinking) split the reply
/// into separate segments. Empty when the child never produced text.
async fn final_reply(db: &crate::db::Db, session_id: &str) -> String {
    let reply = last_reply_text(db, session_id).await;
    let result_char_cap = load_limits(db).await.result_char_cap;
    if reply.chars().count() > result_char_cap {
        let tail: String = reply
            .chars()
            .skip(reply.chars().count() - result_char_cap)
            .collect();
        format!("(truncated…)\n{tail}")
    } else {
        reply
    }
}

/// Uncapped fold behind [`final_reply`]: the session's agent text since its
/// last `user` event. Also used by the voice relay's update messages.
pub(crate) async fn last_reply_text(db: &crate::db::Db, session_id: &str) -> String {
    let events = match db.events_tail(session_id, 200).await {
        Ok(events) => events,
        Err(e) => {
            tracing::warn!(session_id, "subagent transcript read failed: {e}");
            return String::new();
        }
    };
    let after_last_user = events
        .iter()
        .rposition(|e| e.kind == "user")
        .map_or(0, |i| i + 1);
    let mut reply = String::new();
    let mut prev_was_text = false;
    for e in events.iter().skip(after_last_user) {
        if e.kind != "agent-text" {
            prev_was_text = false;
            continue;
        }
        let Some(text) = serde_json::from_str::<serde_json::Value>(&e.data)
            .ok()
            .and_then(|d| d.get("text").and_then(|t| t.as_str()).map(str::to_string))
        else {
            continue;
        };
        if !prev_was_text && !reply.is_empty() {
            reply.push_str("\n\n");
        }
        reply.push_str(&text);
        prev_was_text = true;
    }
    reply
}

/// A message is about to start a new turn on `session_id`. If it is a
/// subagent whose result was already reported, clear the claim so this
/// turn's result reports to the parent too — the claim is one-shot, so a
/// follow-up sent to a finished child otherwise finished silently and the
/// parent waited forever. Also puts the child back in the parent's active
/// count while it works. Returns true iff a claim was cleared.
pub async fn rearm_for_follow_up(db: &crate::db::Db, session_id: &str) -> bool {
    let Ok(Some(session)) = db.get_session(session_id).await else {
        return false;
    };
    if session.parent_session_id.is_none() || session.subagent_completed_at.is_none() {
        return false;
    }
    match db.unclaim_subagent_completion(session_id).await {
        Ok(cleared) => cleared,
        Err(e) => {
            tracing::warn!(session_id, "subagent re-arm failed: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_detail_falls_back_to_the_agent_end_reason_and_stderr() {
        let end = serde_json::json!({
            "status": "crashed",
            "reason": "interrupted",
            "exitCode": 1,
            "stderr": "Reading additional input from stdin...\n",
        });
        let detail = crash_detail(None, Some(&end));
        assert!(
            detail.starts_with("interrupted\n\nexit code: 1"),
            "{detail}"
        );
        assert!(
            detail.contains("```\nReading additional input from stdin...\n```"),
            "{detail}"
        );
        // stderr already quoted by the error is not repeated.
        let detail = crash_detail(
            Some("boom: Reading additional input from stdin..."),
            Some(&end),
        );
        assert!(!detail.contains("stderr (tail)"), "{detail}");
        assert_eq!(crash_detail(None, None), "no error detail");
    }

    #[test]
    fn subagent_prompt_carries_rules_and_task() {
        let p = build_subagent_prompt("scout", "parent-1", "map the repo");
        assert!(p.contains("subagent \"scout\""));
        assert!(p.contains("parent-1"));
        assert!(p.ends_with("# Task\n\nmap the repo"));
        assert!(p.contains("do not spawn further subagents"));
        assert!(p.contains("`run_command`"));
        assert!(p.contains("`search_files`"));
    }

    async fn child_of(db: &crate::db::Db, parent: &str, id: &str) -> crate::db::models::Session {
        let now = chrono::Utc::now().to_rfc3339();
        db.create_session(crate::db::models::NewSession {
            id: id.to_string(),
            name: format!("{SUBAGENT_NAME_PREFIX}{id}"),
            folder_id: "f1".into(),
            created_at: now.clone(),
            last_activity: now,
            parent_session_id: Some(parent.to_string()),
            ..Default::default()
        })
        .await
        .unwrap()
    }

    /// An auth failure that auto-retry parks and releases is not a crash:
    /// no CRASHED report, and the resumed turn's result still reports as
    /// `finished`. One that stays parked through the grace window is.
    #[tokio::test]
    async fn auth_failure_reports_crash_only_when_the_child_never_resumes() {
        let db = crate::db::Db::in_memory().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        db.create_folder(crate::db::models::NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: now.clone(),
        })
        .await
        .unwrap();
        db.create_session(crate::db::models::NewSession {
            id: "parent".into(),
            name: "parent".into(),
            folder_id: "f1".into(),
            created_at: now.clone(),
            last_activity: now,
            ..Default::default()
        })
        .await
        .unwrap();
        let ev = |kind: &'static str| serde_json::json!({ "kind": kind });

        // Parked, auto-retried, and running again: no crash report.
        let resumed = child_of(&db, "parent", "resumed").await;
        db.append_event("resumed", "agent-start", ev("s"))
            .await
            .unwrap();
        db.append_event("resumed", "agent-end", ev("e"))
            .await
            .unwrap();
        let baseline = auth_failure_baseline(&db, "resumed").await;
        db.append_event("resumed", "auth-parked", ev("p"))
            .await
            .unwrap();
        db.append_event("resumed", "auth-resumed", ev("r"))
            .await
            .unwrap();
        db.append_event("resumed", "agent-start", ev("s"))
            .await
            .unwrap();
        let err = Some("Failed to authenticate. API Error: 401");
        assert!(
            claim_after_auth_grace(&db, &resumed, baseline, err)
                .await
                .is_none()
        );
        db.append_event(
            "resumed",
            "agent-text",
            serde_json::json!({ "text": "done" }),
        )
        .await
        .unwrap();
        let (_, text) = claim_and_compose(&db, &resumed, true, None, false)
            .await
            .expect("resumed child still reports its result");
        assert!(
            text.contains("finished]") && text.ends_with("done"),
            "{text}"
        );

        // Parked and never resumed: CRASHED once the grace window lapses.
        let stuck = child_of(&db, "parent", "stuck").await;
        db.append_event("stuck", "agent-start", ev("s"))
            .await
            .unwrap();
        db.append_event("stuck", "agent-end", ev("e"))
            .await
            .unwrap();
        let baseline = auth_failure_baseline(&db, "stuck").await;
        db.append_event("stuck", "auth-parked", ev("p"))
            .await
            .unwrap();
        let (parent, text) = claim_after_auth_grace(&db, &stuck, baseline, err)
            .await
            .expect("a child that never resumed is reported");
        assert_eq!(parent, "parent");
        assert!(text.contains("CRASHED]") && text.contains("401"), "{text}");

        // A later release un-claims so the eventual result reports again.
        assert!(db.unclaim_subagent_completion("stuck").await.unwrap());
        assert!(
            claim_and_compose(&db, &stuck, true, None, false)
                .await
                .is_some()
        );
    }
    /// A finished child given a follow-up message reports again when that
    /// turn ends (regression: the one-shot claim dropped the second report
    /// and the parent sat idle). Plain sessions are left alone.
    #[tokio::test]
    async fn follow_up_message_rearms_the_report() {
        let db = crate::db::Db::in_memory().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        db.create_folder(crate::db::models::NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: now.clone(),
        })
        .await
        .unwrap();
        db.create_session(crate::db::models::NewSession {
            id: "parent".into(),
            name: "parent".into(),
            folder_id: "f1".into(),
            created_at: now.clone(),
            last_activity: now,
            ..Default::default()
        })
        .await
        .unwrap();
        let kid = child_of(&db, "parent", "kid").await;
        assert!(
            claim_and_compose(&db, &kid, true, None, false)
                .await
                .is_some()
        );
        // Without a re-arm the second completion is swallowed.
        assert!(
            claim_and_compose(&db, &kid, true, None, false)
                .await
                .is_none()
        );

        assert!(rearm_for_follow_up(&db, "kid").await);
        assert!(
            claim_and_compose(&db, &kid, true, None, false)
                .await
                .is_some(),
            "the follow-up turn reports to the parent"
        );
        assert!(!rearm_for_follow_up(&db, "parent").await);
    }

    /// Concurrent spawns can't overshoot the cap: count + insert are atomic.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn create_capped_holds_the_cap_under_concurrency() {
        let db = crate::db::Db::in_memory().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        db.create_folder(crate::db::models::NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: now.clone(),
        })
        .await
        .unwrap();
        db.create_session(crate::db::models::NewSession {
            id: "parent".into(),
            name: "parent".into(),
            folder_id: "f1".into(),
            created_at: now.clone(),
            last_activity: now.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
        let tasks: Vec<_> = (0..8)
            .map(|i| {
                let db = db.clone();
                let now = now.clone();
                tokio::spawn(async move {
                    create_capped(
                        &db,
                        crate::db::models::NewSession {
                            id: format!("kid{i}"),
                            name: format!("{SUBAGENT_NAME_PREFIX}kid{i}"),
                            folder_id: "f1".into(),
                            created_at: now.clone(),
                            last_activity: now,
                            parent_session_id: Some("parent".into()),
                            ..Default::default()
                        },
                        2,
                    )
                    .await
                    .is_ok()
                })
            })
            .collect();
        let mut created = 0;
        for t in tasks {
            created += t.await.unwrap() as i64;
        }
        assert_eq!(created, 2);
        assert_eq!(db.count_active_subagents("parent").await.unwrap(), 2);
    }
}

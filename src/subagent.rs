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
//!    message (spawn if idle, queue/inject if running).

use std::sync::Arc;

use crate::state::AppState;

/// `sessions.expert_kind` value marking a subagent session.
pub const SUBAGENT_EXPERT_KIND: &str = "subagent";

/// Default max subagents a parent may have in flight at once (rows with
/// `subagent_completed_at IS NULL`), when no `subagent_limits` override is
/// stored. See [`load_limits`].
pub const DEFAULT_MAX_CONCURRENT_SUBAGENTS: i64 = 5;

/// Prefix for subagent session names, so they read as children in listings.
pub const SUBAGENT_NAME_PREFIX: &str = "sub: ";

/// Default cap on the result text reported back to the parent; longer
/// finals are tail-truncated (the parent can read the full transcript with
/// `read_worker_session`). See [`load_limits`].
pub const DEFAULT_RESULT_CHAR_CAP: usize = 12_000;

/// Reported to the parent when a subagent's completion is only discovered by
/// the startup reconcile pass (server restarted while the child was still
/// running, so it never got the chance to finish or crash cleanly).
pub const RESTART_REASON: &str = "subagent terminated by server restart";

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

/// Report a completed (or crashed) subagent back to its parent session.
/// Idempotent via `claim_subagent_completion`; called from the completion
/// listener for every session that carries a `parent_session_id`.
pub async fn handle_subagent_done(
    state: &Arc<AppState>,
    session: &crate::db::models::Session,
    completed: bool,
    error: Option<&str>,
) {
    let Some((parent_id, text)) = claim_and_compose(&state.db, session, completed, error).await
    else {
        return;
    };

    // Persist on the parent first (the dispatcher only drives the agent),
    // then resume it exactly like an incoming user message.
    if let Err(e) = state
        .db
        .append_event(
            &parent_id,
            "user",
            serde_json::json!({ "text": &text, "source": "subagent-result" }),
        )
        .await
    {
        tracing::warn!(parent_session_id = %parent_id, "subagent result event append failed: {e}");
    }
    let dispatcher = crate::service::mcp_server::AppExpertDispatcher::new(state.clone());
    if let Err(e) =
        crate::service::mcp_server::ExpertDispatcher::resume_session(&dispatcher, &parent_id, &text)
            .await
    {
        tracing::warn!(
            parent_session_id = %parent_id,
            "subagent result delivery failed (parent sees the persisted event on its next turn): {e}"
        );
    }
}

/// Report a subagent's *first-turn dispatch* failure (the async
/// `ExpertDispatcher::resume_session` call the `mcp` route fires off right
/// after `spawn_subagent` returns). Without this, a dispatch failure only
/// logged a `tracing::warn!` and left `subagent_completed_at` NULL forever,
/// so `count_active_subagents` counted the row until server restart
/// (`reconcile_orphan_subagents`) — eventually exhausting the parent's
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
    handle_subagent_done(state, &session, false, Some(error)).await;
}

/// The DB half of [`handle_subagent_done`], separated so it is testable
/// without an `AppState`: claim the completion (idempotent) and compose the
/// report text. Returns `(parent_session_id, text)` only for the call that
/// won the claim while the parent still exists.
pub async fn claim_and_compose(
    db: &crate::db::Db,
    session: &crate::db::models::Session,
    completed: bool,
    error: Option<&str>,
) -> Option<(String, String)> {
    let parent_id = session.parent_session_id.as_deref()?;
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
        format!(
            "[subagent \"{name}\" ({id}) CRASHED]\n\n{err}\n\nRead its transcript with \
             read_worker_session, then re-spawn it or continue without it.",
            id = session.id,
            err = error.unwrap_or("no error detail"),
        )
    };
    Some((parent_id.to_string(), text))
}

/// The child's final reply: every `agent-text` event after the last `user`
/// event, joined. Empty when the child never produced text.
async fn final_reply(db: &crate::db::Db, session_id: &str) -> String {
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
    let parts: Vec<String> = events
        .iter()
        .skip(after_last_user)
        .filter(|e| e.kind == "agent-text")
        .filter_map(|e| {
            serde_json::from_str::<serde_json::Value>(&e.data)
                .ok()
                .and_then(|d| d.get("text").and_then(|t| t.as_str()).map(str::to_string))
        })
        .collect();
    let reply = parts.join("\n\n");
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

/// Startup reconcile pass: find subagent sessions still marked
/// `subagent_completed_at IS NULL` whose parent link exists but whose
/// completion was never claimed — orphaned by a server restart, since the
/// only writer of `subagent_completed_at` is the in-process completion
/// listener in `main.rs`, which never ran for them. Left alone these rows
/// permanently occupy the parent's concurrent-subagent slot and the
/// parent waits forever for a result that will never arrive.
///
/// Call once at boot, before any provider/session traffic starts (nothing
/// can be genuinely "running" yet at that point, so every incomplete row
/// found here is by definition orphaned — no liveness probe needed). Each
/// orphan is claimed (freeing its parent's slot) and reported to its parent
/// as a crashed subagent via [`claim_and_compose`], same as a real crash,
/// with [`RESTART_REASON`] as the error detail. Deliberately does not
/// resume the parent's agent turn (unlike [`handle_subagent_done`]) — the
/// parent's own process is equally dead at boot, so driving it here would
/// mean auto-running arbitrary sessions on startup; the persisted event is
/// picked up the next time the parent session runs. Returns the number of
/// orphans reconciled.
pub async fn reconcile_orphan_subagents(db: &crate::db::Db) -> usize {
    let orphans = match db.list_incomplete_subagents().await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!("subagent reconcile: list_incomplete_subagents failed: {e}");
            return 0;
        }
    };

    let mut reconciled = 0usize;
    for session in &orphans {
        let Some((parent_id, text)) =
            claim_and_compose(db, session, false, Some(RESTART_REASON)).await
        else {
            continue;
        };
        if let Err(e) = db
            .append_event(
                &parent_id,
                "user",
                serde_json::json!({ "text": &text, "source": "subagent-result" }),
            )
            .await
        {
            tracing::warn!(
                parent_session_id = %parent_id,
                "subagent reconcile: result event append failed: {e}"
            );
            continue;
        }
        reconciled += 1;
    }
    if reconciled > 0 {
        tracing::info!(
            count = reconciled,
            "subagent reconcile: orphans reported to parents"
        );
    }
    reconciled
}
#[cfg(test)]
mod tests {
    use super::*;

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
}

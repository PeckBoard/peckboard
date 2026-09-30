//! Boot recovery for turns a server restart killed mid-flight.
//!
//! A restart (an upgrade, a crash, the admin restart button) kills every
//! agent process. Before this pass the only boot handling was
//! [`crate::security::repair_dangling_sessions`] (closes the dead turn with a
//! synthetic `agent-end{crashed, server-shutdown}`) plus a subagent reconcile
//! that reported every in-flight child to its parent as CRASHED without
//! waking the parent — so a parent waiting on a helper sat idle until a user
//! happened to look at it, and the interrupted sessions themselves never
//! came back.
//!
//! Now, for every session whose turn the restart killed:
//!
//! - resumable (its provider holds a conversation to continue, the user
//!   didn't stop it, its project isn't paused, and no other boot path owns
//!   it) → it is sent [`CONTINUE_TEXT`] through the normal locked dispatch
//!   path. A subagent child stays unclaimed, so its eventual completion
//!   still reports `finished` to the parent;
//! - not resumable → logged; a subagent child is claimed and its parent is
//!   woken with [`cannot_resume_notice`] so it can re-spawn.
//!
//! Resumes are staggered ([`STAGGER`], at most [`MAX_CONCURRENT`]
//! dispatches in flight) so boot is not a thundering herd.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use crate::db::Db;
use crate::db::models::Session;
use crate::service::mcp_server::ExpertDispatcher;
use crate::state::AppState;
use crate::ws::broadcaster::WsEvent;

/// The message a resumed session receives.
pub const CONTINUE_TEXT: &str = "[system] The Peckboard server restarted while you were working. \
     Continue where you left off; re-run any command that was interrupted (its output was lost).";

/// `source` tag on the persisted `user` event of a restart continuation.
pub const EVENT_SOURCE: &str = "restart-resume";

/// Delay between successive resume dispatches.
pub const STAGGER: Duration = Duration::from_secs(3);

/// Max resume dispatches in flight at once (a CLI spawn can take minutes).
pub const MAX_CONCURRENT: usize = 3;
/// A killed turn silent for longer than this before the restart was already
/// dead or hung (or is a years-old dangling start the repair just closed);
/// waking it would surprise the user.
pub const STALE_AFTER: Duration = Duration::from_secs(6 * 3600);

/// What the parent of a subagent the restart stopped for good is told.
pub fn cannot_resume_notice(child: &Session, reason: &str) -> String {
    let name = child
        .name
        .strip_prefix(crate::subagent::SUBAGENT_NAME_PREFIX)
        .unwrap_or(&child.name);
    format!(
        "[subagent \"{name}\" ({id}) was stopped by a server restart and could not resume — \
         re-spawn it if still needed]\n\nReason: {reason}.",
        id = child.id
    )
}

/// One session to resume.
#[derive(Debug, Clone, PartialEq)]
pub struct ResumeItem {
    pub session_id: String,
    /// Seq of the boot repair's synthetic `agent-end`: if a newer lifecycle
    /// event exists at dispatch time something else already woke it.
    pub repair_seq: i32,
    /// Subagent notices for this session (as a parent), folded into its
    /// continuation so it is woken once, not once per notice.
    pub notices: Vec<String>,
}

/// Notices for a parent that is not itself being resumed.
#[derive(Debug, Clone, PartialEq)]
pub struct ParentNotice {
    pub parent_id: String,
    pub text: String,
    /// False when the parent's project is paused: persist, don't wake.
    pub wake: bool,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct RestartPlan {
    pub resume: Vec<ResumeItem>,
    pub parent_notices: Vec<ParentNotice>,
    /// `(session_id, reason)` for every interrupted session left alone.
    pub skipped: Vec<(String, String)>,
}

/// Why `session`'s project forbids waking it, if it does. Shared with the
/// lost-background-task report so neither wakes a paused project's session.
pub async fn wake_blocked_by_project(db: &Db, session: &Session) -> Option<String> {
    let project_id = session.project_id.as_deref()?;
    match db.get_project(project_id).await {
        Ok(Some(p)) if p.status != "active" => Some(format!("project is {}", p.status)),
        _ => None,
    }
}

/// Whether the user stopped the turn (interrupt / terminate) after it
/// started — such a session must stay stopped.
async fn stopped_by_user(db: &Db, session_id: &str) -> bool {
    let start = db
        .latest_event_of_kinds(session_id, &["agent-start"])
        .await
        .ok()
        .flatten()
        .map_or(0, |e| e.seq);
    if let Ok(Some(e)) = db.latest_event_of_kinds(session_id, &["interrupt"]).await
        && e.seq > start
    {
        return true;
    }
    // Terminate leaves a `system` notice rather than a lifecycle event.
    db.latest_event_of_kinds(session_id, &["system"])
        .await
        .ok()
        .flatten()
        .is_some_and(|e| {
            e.seq > start
                && serde_json::from_str::<serde_json::Value>(&e.data)
                    .ok()
                    .and_then(|d| d.get("text")?.as_str().map(str::to_string))
                    .is_some_and(|t| t.starts_with("Agent terminated"))
        })
}

/// How long the turn had been silent before the boot repair closed it: the
/// age of the newest event preceding the repair's synthetic `agent-end`.
async fn idle_before_restart(db: &Db, session_id: &str) -> Option<Duration> {
    let tail = db.events_tail(session_id, 2).await.ok()?;
    let last = tail
        .iter()
        .filter(|e| e.kind != "agent-end")
        .max_by_key(|e| e.seq)?;
    let age_ms = chrono::Utc::now().timestamp_millis() - last.ts;
    Some(Duration::from_millis(age_ms.max(0) as u64))
}

/// Why `session` (whose turn the restart killed) must not be auto-resumed.
async fn resume_blocker(db: &Db, session: &Session) -> Option<String> {
    if session.card_id.is_some() {
        return Some("card worker: the orchestrator re-spawns it".into());
    }
    if session.expert_kind.as_deref() == Some(crate::service::doc_reviews::EXPERT_KIND) {
        return Some("doc review: resumed by its own boot path".into());
    }
    // The voice assistant must never start talking on its own; the user
    // picks the conversation back up when they reopen the panel.
    if session.expert_kind.as_deref() == Some("voice") {
        return Some("voice assistant: never speaks unprompted".into());
    }
    if session.handover_to_model.is_some() {
        return Some("model handover parked: handled by the handover reconcile".into());
    }
    if stopped_by_user(db, &session.id).await {
        return Some("the user stopped the turn".into());
    }
    if let Some(idle) = idle_before_restart(db, &session.id).await
        && idle > STALE_AFTER
    {
        return Some(format!(
            "stale: no activity for {}h before the restart",
            idle.as_secs() / 3600
        ));
    }
    if session.conversation_id.is_none() {
        return Some("provider has no conversation to resume".into());
    }
    wake_blocked_by_project(db, session).await
}

/// Decide, from the DB alone, what to do about the restart. `interrupted`
/// is the set of sessions the boot repair found mid-turn;
/// `lost_task_sessions` own a background task the restart lost (the lost
/// report wakes them, and a subagent's completion then reports as usual).
///
/// Side effect: subagent children that will not resume are claimed here
/// (freeing the parent's concurrency slot), so a later completion can't
/// report them twice.
pub async fn plan(
    db: &Db,
    interrupted: &[String],
    lost_task_sessions: &HashSet<String>,
) -> RestartPlan {
    let mut plan = RestartPlan::default();
    let mut notices: HashMap<String, Vec<String>> = HashMap::new();
    let interrupted_set: HashSet<&str> = interrupted.iter().map(String::as_str).collect();

    for id in interrupted {
        let Ok(Some(session)) = db.get_session(id).await else {
            continue;
        };
        let is_child =
            session.parent_session_id.is_some() && session.subagent_completed_at.is_none();
        match resume_blocker(db, &session).await {
            None => {
                let repair_seq = db
                    .latest_event_of_kinds(id, &["agent-start", "agent-end"])
                    .await
                    .ok()
                    .flatten()
                    .map_or(0, |e| e.seq);
                plan.resume.push(ResumeItem {
                    session_id: id.clone(),
                    repair_seq,
                    notices: Vec::new(),
                });
            }
            Some(reason) => {
                if is_child
                    && let Some((parent, text)) = claim_for_notice(db, &session, &reason).await
                {
                    notices.entry(parent).or_default().push(text);
                }
                plan.skipped.push((id.clone(), reason));
            }
        }
    }

    // Children whose turn had already ended but whose completion was never
    // claimed (the listener died with the process, or the child was waiting
    // on a background task).
    for child in db.list_incomplete_subagents().await.unwrap_or_default() {
        if interrupted_set.contains(child.id.as_str()) || lost_task_sessions.contains(&child.id) {
            continue;
        }
        let last_end = db
            .latest_event_of_kinds(&child.id, &["agent-end"])
            .await
            .ok()
            .flatten();
        let crashed = last_end
            .as_ref()
            .and_then(|e| serde_json::from_str::<serde_json::Value>(&e.data).ok())
            .is_some_and(|d| d.get("status").and_then(|s| s.as_str()) == Some("crashed"));
        let composed = if last_end.is_none() {
            claim_for_notice(db, &child, "its first turn never started").await
        } else if crashed {
            claim_for_notice(db, &child, "its last turn crashed before the restart").await
        } else {
            // Clean last turn: deliver its result as a normal `finished`.
            crate::subagent::claim_and_compose(db, &child, true, None, false).await
        };
        if let Some((parent, text)) = composed {
            tracing::info!(session_id = %child.id, parent_session_id = %parent, crashed, "restart resume: reporting unclaimed subagent to its parent");
            notices.entry(parent).or_default().push(text);
        }
    }

    for (parent_id, texts) in notices {
        if let Some(item) = plan.resume.iter_mut().find(|r| r.session_id == parent_id) {
            item.notices.extend(texts);
            continue;
        }
        let wake = match db.get_session(&parent_id).await {
            Ok(Some(parent)) => wake_blocked_by_project(db, &parent).await.is_none(),
            _ => continue,
        };
        plan.parent_notices.push(ParentNotice {
            parent_id,
            text: texts.join("\n\n"),
            wake,
        });
    }

    for item in &plan.resume {
        tracing::info!(session_id = %item.session_id, notices = item.notices.len(), "restart resume: will resume");
    }
    for (id, reason) in &plan.skipped {
        tracing::info!(session_id = %id, reason = %reason, "restart resume: skipped");
    }
    plan
}

/// Claim a child that won't resume and compose its parent notice. `None`
/// when already claimed or the parent is gone.
async fn claim_for_notice(db: &Db, child: &Session, reason: &str) -> Option<(String, String)> {
    let parent_id = child.parent_session_id.clone()?;
    let now = chrono::Utc::now().to_rfc3339();
    if !matches!(
        db.claim_subagent_completion(&child.id, &now).await,
        Ok(true)
    ) {
        return None;
    }
    if !matches!(db.get_session(&parent_id).await, Ok(Some(_))) {
        tracing::warn!(session_id = %child.id, parent_session_id = %parent_id, "restart resume: subagent's parent is gone");
        return None;
    }
    Some((parent_id, cannot_resume_notice(child, reason)))
}

/// Carry out `plan`: wake parents with their notices, then resume the
/// interrupted sessions. Every wake waits [`STAGGER`] first (the first one
/// also gives the HTTP listener time to bind). Returns once every dispatch
/// finished.
pub async fn execute(state: Arc<AppState>, plan: RestartPlan) {
    execute_with_stagger(state, plan, STAGGER).await;
}

pub(crate) async fn execute_with_stagger(
    state: Arc<AppState>,
    plan: RestartPlan,
    stagger: Duration,
) {
    let dispatcher = crate::service::mcp_server::AppExpertDispatcher::new(state.clone());
    for n in &plan.parent_notices {
        if n.wake {
            tokio::time::sleep(stagger).await;
        }
        tracing::info!(parent_session_id = %n.parent_id, wake = n.wake, "restart resume: notifying parent of stopped subagent(s)");
        let d: Option<&dyn ExpertDispatcher> = if n.wake { Some(&dispatcher) } else { None };
        if let Err(e) = crate::service::session_notify::notify_session(
            &state.db,
            &state.broadcaster,
            d,
            &n.parent_id,
            &n.text,
            serde_json::json!({ "source": "subagent-result" }),
        )
        .await
        {
            tracing::warn!(parent_session_id = %n.parent_id, "restart resume: parent notice failed: {e}");
        }
    }

    let permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT));
    let mut handles = Vec::new();
    for item in plan.resume {
        tokio::time::sleep(stagger).await;
        let Ok(permit) = permits.clone().acquire_owned().await else {
            break;
        };
        let state = state.clone();
        handles.push(tokio::spawn(async move {
            resume_one(&state, &item).await;
            drop(permit);
        }));
    }
    for h in handles {
        let _ = h.await;
    }
}
async fn resume_one(state: &Arc<AppState>, item: &ResumeItem) {
    let id = item.session_id.as_str();
    // Something else (a user message, a lost-task report) may have woken
    // the session since boot — that turn already continues it.
    let latest = state
        .db
        .latest_event_of_kinds(id, &["agent-start", "agent-end"])
        .await
        .ok()
        .flatten()
        .map_or(0, |e| e.seq);
    if latest != item.repair_seq || state.session_manager.is_running(id).await {
        tracing::info!(session_id = %id, "restart resume: skipped (already woken since boot)");
        return;
    }
    let mut text = CONTINUE_TEXT.to_string();
    for n in &item.notices {
        text.push_str("\n\n");
        text.push_str(n);
    }
    tracing::info!(session_id = %id, "restart resume: resuming");
    if let Err(e) = dispatch(state, id, &text).await {
        tracing::warn!(session_id = %id, "restart resume: dispatch failed: {e}");
        on_resume_failed(state, id, &e.to_string()).await;
    }
}

/// Persist + broadcast the continuation, then send it like a user message
/// (`send_or_queue` under the per-session lock).
async fn dispatch(state: &Arc<AppState>, session_id: &str, text: &str) -> anyhow::Result<()> {
    let data = serde_json::json!({ "text": text, "source": EVENT_SOURCE });
    let event = state
        .db
        .append_event(session_id, "user", data.clone())
        .await?;
    state.broadcaster.broadcast(WsEvent {
        event_type: "event".into(),
        session_id: session_id.to_string(),
        data: serde_json::json!({
            "id": event.id,
            "seq": event.seq,
            "ts": event.ts,
            "kind": event.kind,
            "data": data,
        }),
    });
    crate::service::mcp_server::AppExpertDispatcher::new(state.clone())
        .resume_session_appended(session_id, text)
        .await
}

/// A resume that could not even start: a subagent's parent is told (and
/// woken) so it can re-spawn.
async fn on_resume_failed(state: &Arc<AppState>, session_id: &str, error: &str) {
    let Ok(Some(session)) = state.db.get_session(session_id).await else {
        return;
    };
    let reason = format!("resume dispatch failed: {error}");
    let Some((parent_id, text)) = claim_for_notice(&state.db, &session, &reason).await else {
        return;
    };
    let dispatcher = crate::service::mcp_server::AppExpertDispatcher::new(state.clone());
    let wake = match state.db.get_session(&parent_id).await {
        Ok(Some(parent)) => wake_blocked_by_project(&state.db, &parent).await.is_none(),
        _ => false,
    };
    let d: Option<&dyn ExpertDispatcher> = if wake { Some(&dispatcher) } else { None };
    if let Err(e) = crate::service::session_notify::notify_session(
        &state.db,
        &state.broadcaster,
        d,
        &parent_id,
        &text,
        serde_json::json!({ "source": "subagent-result" }),
    )
    .await
    {
        tracing::warn!(parent_session_id = %parent_id, "restart resume: parent notice failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{NewFolder, NewProject, NewSession};

    async fn fixture(dir: &std::path::Path) -> Arc<AppState> {
        let state = crate::auth::middleware::tests::test_state(dir);
        crate::provider::test_double::register_mock_provider(&state.provider_registry).await;
        state
            .db
            .create_folder(NewFolder {
                id: "f1".into(),
                name: "F".into(),
                path: dir.to_string_lossy().to_string(),
                created_at: chrono::Utc::now().to_rfc3339(),
            })
            .await
            .unwrap();
        state
    }

    /// A session whose turn a restart killed mid-tool.
    async fn seed_killed(
        state: &Arc<AppState>,
        id: &str,
        conversation: Option<&str>,
        parent: Option<&str>,
        project: Option<&str>,
    ) {
        let now = chrono::Utc::now().to_rfc3339();
        state
            .db
            .create_session(NewSession {
                id: id.into(),
                name: format!("{}{id}", if parent.is_some() { "sub: " } else { "" }),
                folder_id: "f1".into(),
                model: Some("mock:echo".into()),
                conversation_id: conversation.map(str::to_string),
                parent_session_id: parent.map(str::to_string),
                project_id: project.map(str::to_string),
                created_at: now.clone(),
                last_activity: now,
                ..Default::default()
            })
            .await
            .unwrap();
        for kind in ["user", "agent-start", "agent-tool-end"] {
            state
                .db
                .append_event(id, kind, serde_json::json!({ "text": "work" }))
                .await
                .unwrap();
        }
    }

    async fn texts(state: &Arc<AppState>, id: &str, kind: &str) -> Vec<String> {
        state
            .db
            .list_events_by_session(id, None)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == kind)
            .filter_map(|e| serde_json::from_str::<serde_json::Value>(&e.data).ok())
            .filter_map(|d| d.get("text")?.as_str().map(str::to_string))
            .collect()
    }

    async fn wait_for_text(state: &Arc<AppState>, id: &str, needle: &str) -> bool {
        for _ in 0..100 {
            if texts(state, id, "agent-text")
                .await
                .iter()
                .any(|t| t.contains(needle))
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    async fn boot(state: &Arc<AppState>) -> RestartPlan {
        let interrupted = crate::security::repair_dangling_sessions(&state.db)
            .await
            .unwrap();
        let plan = plan(&state.db, &interrupted, &HashSet::new()).await;
        execute_with_stagger(state.clone(), plan.clone(), Duration::ZERO).await;
        plan
    }

    #[tokio::test]
    async fn a_session_killed_mid_turn_is_resumed_with_the_continuation() {
        let dir = tempfile::tempdir().unwrap();
        let state = fixture(dir.path()).await;
        seed_killed(&state, "s1", Some("conv-1"), None, None).await;

        let plan = boot(&state).await;

        assert_eq!(plan.resume.len(), 1, "{plan:?}");
        let users = texts(&state, "s1", "user").await;
        assert_eq!(users.last().map(String::as_str), Some(CONTINUE_TEXT));
        assert!(
            wait_for_text(&state, "s1", "server restarted").await,
            "the continuation reached the provider"
        );
    }

    #[tokio::test]
    async fn a_turn_the_user_stopped_is_not_resumed() {
        let dir = tempfile::tempdir().unwrap();
        let state = fixture(dir.path()).await;
        seed_killed(&state, "s1", Some("conv-1"), None, None).await;
        state
            .db
            .append_event(
                "s1",
                "interrupt",
                serde_json::json!({ "reason": "user-interrupt" }),
            )
            .await
            .unwrap();

        let plan = boot(&state).await;

        assert!(plan.resume.is_empty());
        assert_eq!(plan.skipped[0].1, "the user stopped the turn");
        assert!(
            !texts(&state, "s1", "user")
                .await
                .iter()
                .any(|t| t == CONTINUE_TEXT)
        );
    }

    #[tokio::test]
    async fn a_paused_projects_session_is_not_woken() {
        let dir = tempfile::tempdir().unwrap();
        let state = fixture(dir.path()).await;
        let now = chrono::Utc::now().to_rfc3339();
        state
            .db
            .create_project(NewProject {
                id: "p1".into(),
                name: "P".into(),
                context: "".into(),
                folder_id: "f1".into(),
                worker_count: 1,
                status: "paused".into(),
                workflow: "default".into(),
                model: None,
                effort: None,
                parallel_instructions: false,
                auto_notify_changes: false,
                worker_communication: false,
                created_at: now.clone(),
                last_accessed_at: now,
                budget_usd_cents: None,
                budget_period: None,
                worktree_isolation: false,
            })
            .await
            .unwrap();
        seed_killed(&state, "w1", Some("conv-1"), None, Some("p1")).await;

        let plan = boot(&state).await;

        assert!(plan.resume.is_empty());
        assert_eq!(plan.skipped[0].1, "project is paused");
        assert!(texts(&state, "w1", "agent-text").await.is_empty());
    }

    #[tokio::test]
    async fn a_resumed_subagent_still_reports_finished_to_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let state = fixture(dir.path()).await;
        seed_killed(&state, "parent", Some("conv-p"), None, None).await;
        // The parent was idle (waiting on its child), not mid-turn.
        state
            .db
            .append_event("parent", "agent-end", serde_json::json!({ "status": "ok" }))
            .await
            .unwrap();
        seed_killed(&state, "kid", Some("conv-k"), Some("parent"), None).await;

        let plan = boot(&state).await;

        assert_eq!(plan.resume.len(), 1);
        assert_eq!(plan.resume[0].session_id, "kid");
        assert!(plan.parent_notices.is_empty(), "no premature notice");
        assert!(wait_for_text(&state, "kid", "server restarted").await);
        let kid = state.db.get_session("kid").await.unwrap().unwrap();
        assert!(
            kid.subagent_completed_at.is_none(),
            "the resume keeps the slot"
        );

        // What the completion listener does when the resumed turn ends.
        crate::subagent::handle_subagent_done(&state, &kid, true, false, None).await;
        let parent_users = texts(&state, "parent", "user").await;
        assert!(
            parent_users
                .iter()
                .any(|t| t.starts_with("[subagent \"kid\" (kid) finished]")),
            "{parent_users:?}"
        );
    }

    #[tokio::test]
    async fn a_subagent_that_cannot_resume_wakes_its_parent_with_a_notice() {
        let dir = tempfile::tempdir().unwrap();
        let state = fixture(dir.path()).await;
        seed_killed(&state, "parent", Some("conv-p"), None, None).await;
        state
            .db
            .append_event("parent", "agent-end", serde_json::json!({ "status": "ok" }))
            .await
            .unwrap();
        // No conversation id: its provider has nothing to resume.
        seed_killed(&state, "kid", None, Some("parent"), None).await;

        let plan = boot(&state).await;

        assert!(plan.resume.is_empty());
        let kid = state.db.get_session("kid").await.unwrap().unwrap();
        assert!(kid.subagent_completed_at.is_some(), "claimed: slot freed");
        let notice = "[subagent \"kid\" (kid) was stopped by a server restart and could not resume";
        assert!(
            texts(&state, "parent", "user")
                .await
                .iter()
                .any(|t| t.starts_with(notice))
        );
        assert!(
            wait_for_text(&state, "parent", notice).await,
            "the parent is woken with the notice"
        );
        assert!(
            !texts(&state, "parent", "user")
                .await
                .iter()
                .any(|t| t.contains("CRASHED")),
            "no second, CRASHED report"
        );
    }
}

//! What a server restart would interrupt, and "restart when idle".
//!
//! [`collect_activity`] snapshots every piece of in-flight work a restart
//! kills — sessions mid-turn, running subagents, card workers, and
//! Peckboard-managed background tasks — from the live registries
//! (`SessionManager` run state, `BackgroundRegistry`). The admin restart
//! routes show it to the user before restarting.
//!
//! [`schedule_idle_restart`] parks a restart until that snapshot is empty.
//! A restart is process-wide, so the one pending request lives in a
//! process-global slot: a new request replaces it, `cancel_pending` drops
//! it, and every change is broadcast as a global `restart-pending` WS
//! event so every open client can show (and cancel) it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::watch;

use crate::state::AppState;
use crate::ws::broadcaster::{Broadcaster, WsEvent};

/// Global WS event type announcing the pending-restart state.
pub const RESTART_PENDING_EVENT: &str = "restart-pending";

/// How often a pending idle restart re-checks the activity list.
const IDLE_POLL: Duration = Duration::from_secs(3);
/// Give up on an idle restart that has waited this long; the user can
/// always ask again (or restart anyway).
const IDLE_CAP: Duration = Duration::from_secs(12 * 60 * 60);
/// Consecutive empty polls required before restarting. A single empty
/// poll can land in the gap between one turn ending and the next
/// starting (a subagent reporting back wakes its parent), so one idle
/// reading is not proof the work is done.
const IDLE_CONFIRMATIONS: u32 = 2;

#[derive(Debug, Clone, Serialize)]
pub struct SessionActivity {
    pub session_id: String,
    pub name: String,
    pub folder_name: Option<String>,
    pub project_name: Option<String>,
    /// RFC 3339 start of the current turn (its `agent-start` event).
    pub running_since: Option<String>,
    pub running_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubagentActivity {
    pub session_id: String,
    pub name: String,
    pub parent_session_id: String,
    pub parent_name: Option<String>,
    pub running_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerActivity {
    pub session_id: String,
    pub session_name: String,
    pub card_id: String,
    pub card_title: Option<String>,
    pub project_id: Option<String>,
    pub project_name: Option<String>,
    pub step: Option<String>,
    pub running_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackgroundActivity {
    pub task_id: String,
    pub label: String,
    pub command: String,
    pub session_id: String,
    pub session_name: Option<String>,
    pub running_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ActivityCounts {
    pub sessions: usize,
    pub subagents: usize,
    pub workers: usize,
    pub background_tasks: usize,
}

/// Everything a restart would interrupt right now.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Activity {
    /// Ordinary sessions mid-turn (chats, experts, orchestrator brains).
    pub sessions: Vec<SessionActivity>,
    pub subagents: Vec<SubagentActivity>,
    /// Card workers mid-turn.
    pub workers: Vec<WorkerActivity>,
    pub background_tasks: Vec<BackgroundActivity>,
    pub counts: ActivityCounts,
    pub total: usize,
}

fn secs_since_millis(ts_ms: i64) -> u64 {
    let now = chrono::Utc::now().timestamp_millis();
    (now.saturating_sub(ts_ms).max(0) / 1000) as u64
}

fn secs_since_rfc3339(ts: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|t| secs_since_millis(t.timestamp_millis()))
}

/// Snapshot what a restart would interrupt. Every session is checked
/// against the live provider run state, not the event log, so a turn the
/// log still shows as open after a crash is not reported.
pub async fn collect_activity(state: &AppState) -> anyhow::Result<Activity> {
    let sessions = state.db.list_sessions().await?;
    let names: std::collections::HashMap<&str, &str> = sessions
        .iter()
        .map(|s| (s.id.as_str(), s.name.as_str()))
        .collect();

    let mut activity = Activity::default();
    let mut folders = std::collections::HashMap::new();
    let mut projects = std::collections::HashMap::new();

    for s in &sessions {
        if !state.session_manager.is_running(&s.id).await {
            continue;
        }
        let started = state
            .db
            .latest_event_of_kind(&s.id, "agent-start")
            .await
            .ok()
            .flatten()
            .map(|e| e.ts);
        let running_secs = started.map(secs_since_millis);

        let project_name = match &s.project_id {
            Some(pid) => {
                if !projects.contains_key(pid) {
                    let p = state.db.get_project(pid).await.ok().flatten();
                    projects.insert(pid.clone(), p.map(|p| p.name));
                }
                projects.get(pid).cloned().flatten()
            }
            None => None,
        };

        if let Some(parent) = &s.parent_session_id {
            activity.subagents.push(SubagentActivity {
                session_id: s.id.clone(),
                name: s.name.clone(),
                parent_session_id: parent.clone(),
                parent_name: names.get(parent.as_str()).map(|n| n.to_string()),
                running_secs,
            });
        } else if let (true, Some(card_id)) = (s.is_worker, &s.card_id) {
            let card = state.db.get_card(card_id).await.ok().flatten();
            activity.workers.push(WorkerActivity {
                session_id: s.id.clone(),
                session_name: s.name.clone(),
                card_id: card_id.clone(),
                card_title: card.as_ref().map(|c| c.title.clone()),
                project_id: s.project_id.clone(),
                project_name,
                step: s
                    .worker_step
                    .clone()
                    .or_else(|| card.as_ref().map(|c| c.step.clone())),
                running_secs,
            });
        } else {
            if !folders.contains_key(&s.folder_id) {
                let f = state.db.get_folder(&s.folder_id).await.ok().flatten();
                folders.insert(s.folder_id.clone(), f.map(|f| f.name));
            }
            activity.sessions.push(SessionActivity {
                session_id: s.id.clone(),
                name: s.name.clone(),
                folder_name: folders.get(&s.folder_id).cloned().flatten(),
                project_name,
                running_since: started.and_then(|ms| {
                    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.to_rfc3339())
                }),
                running_secs,
            });
        }
    }

    for t in state.background.list_running() {
        activity.background_tasks.push(BackgroundActivity {
            command: crate::background::report::display_command(&t.program, &t.args),
            session_name: names.get(t.session_id.as_str()).map(|n| n.to_string()),
            running_secs: secs_since_rfc3339(&t.started_at),
            task_id: t.id,
            label: t.label,
            session_id: t.session_id,
        });
    }

    activity.counts = ActivityCounts {
        sessions: activity.sessions.len(),
        subagents: activity.subagents.len(),
        workers: activity.workers.len(),
        background_tasks: activity.background_tasks.len(),
    };
    activity.total = activity.counts.sessions
        + activity.counts.subagents
        + activity.counts.workers
        + activity.counts.background_tasks;
    Ok(activity)
}

/// How a wait for idle ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleOutcome {
    Idle,
    Cancelled,
    TimedOut,
}

/// Poll `probe` (the number of in-flight items) every `poll` until it has
/// read zero `IDLE_CONFIRMATIONS` times in a row, `cancel` flips to true,
/// or `cap` elapses. `on_tick` sees every reading. Pure over its inputs so
/// the waiting rules are testable without restarting anything.
pub async fn wait_until_idle<P, Fut, T>(
    mut probe: P,
    mut cancel: watch::Receiver<bool>,
    poll: Duration,
    cap: Duration,
    mut on_tick: T,
) -> IdleOutcome
where
    P: FnMut() -> Fut,
    Fut: std::future::Future<Output = usize>,
    T: FnMut(usize),
{
    let deadline = tokio::time::Instant::now() + cap;
    let mut idle_reads = 0;
    loop {
        if *cancel.borrow() {
            return IdleOutcome::Cancelled;
        }
        let remaining = probe().await;
        on_tick(remaining);
        if remaining == 0 {
            idle_reads += 1;
            if idle_reads >= IDLE_CONFIRMATIONS {
                return IdleOutcome::Idle;
            }
        } else {
            idle_reads = 0;
        }
        if tokio::time::Instant::now() >= deadline {
            return IdleOutcome::TimedOut;
        }
        tokio::select! {
            _ = tokio::time::sleep(poll) => {}
            changed = cancel.changed() => {
                // A dropped sender means nobody can cancel any more; keep
                // waiting on the poll alone.
                if changed.is_err() {
                    tokio::time::sleep(poll).await;
                }
            }
        }
    }
}

/// Which restart is pending: a plain restart or an already-downloaded
/// update waiting to be exec'd.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RestartKind {
    Restart,
    Update,
}

/// The pending idle restart, as reported to clients.
#[derive(Debug, Clone, Serialize)]
pub struct PendingRestart {
    pub kind: RestartKind,
    /// Release tag an update restart will come up on.
    pub version: Option<String>,
    /// RFC 3339.
    pub requested_at: String,
    /// In-flight items at the last poll.
    pub remaining: usize,
}

struct Slot {
    id: u64,
    info: PendingRestart,
    cancel: watch::Sender<bool>,
}

static PENDING: Mutex<Option<Slot>> = Mutex::new(None);
static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn slot() -> std::sync::MutexGuard<'static, Option<Slot>> {
    PENDING.lock().unwrap_or_else(|p| p.into_inner())
}

/// The pending idle restart, if any.
pub fn pending() -> Option<PendingRestart> {
    slot().as_ref().map(|s| s.info.clone())
}

fn broadcast(broadcaster: &Broadcaster, data: serde_json::Value) {
    broadcaster.broadcast(WsEvent {
        event_type: RESTART_PENDING_EVENT.into(),
        session_id: String::new(),
        data,
    });
}

fn broadcast_pending(broadcaster: &Broadcaster, info: &PendingRestart) {
    broadcast(
        broadcaster,
        serde_json::json!({ "pending": info, "restarting": false }),
    );
}

/// Drop the pending idle restart. Returns whether one was pending.
pub fn cancel_pending(broadcaster: &Broadcaster) -> bool {
    let Some(s) = slot().take() else {
        return false;
    };
    let _ = s.cancel.send(true);
    broadcast(
        broadcaster,
        serde_json::json!({ "pending": null, "restarting": false }),
    );
    true
}

/// The executable to re-exec for a plain restart. After an update swapped
/// the binary without restarting, Linux reports the running image as
/// `"<path> (deleted)"`; the file to exec is the one now at `<path>`.
pub fn current_exe_for_restart() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let s = exe.to_string_lossy();
    Ok(match s.strip_suffix(" (deleted)") {
        Some(stripped) => PathBuf::from(stripped),
        None => exe,
    })
}

/// Re-exec into `exe` after a short delay, so the HTTP response (or WS
/// frame) announcing it is flushed first.
pub fn restart_soon(exe: PathBuf, delay: Duration) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        tracing::warn!(exe = %exe.display(), "restarting Peckboard");
        if let Err(e) = crate::service::update::restart(&exe) {
            tracing::error!("restart re-exec failed: {e}");
        }
    });
}

/// Park a restart into `exe` until nothing is running, replacing any
/// restart already pending. `do_restart` performs the actual re-exec —
/// injected so tests can observe the decision without the process dying.
pub fn schedule_idle_restart(
    state: Arc<AppState>,
    exe: PathBuf,
    kind: RestartKind,
    version: Option<String>,
    do_restart: impl FnOnce(&Path) + Send + 'static,
) -> PendingRestart {
    let (tx, rx) = watch::channel(false);
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let info = PendingRestart {
        kind,
        version,
        requested_at: chrono::Utc::now().to_rfc3339(),
        remaining: 0,
    };
    if let Some(old) = slot().replace(Slot {
        id,
        info: info.clone(),
        cancel: tx,
    }) {
        let _ = old.cancel.send(true);
    }
    broadcast_pending(&state.broadcaster, &info);

    tokio::spawn(async move {
        let probe_state = state.clone();
        let tick_state = state.clone();
        let outcome = wait_until_idle(
            move || {
                let st = probe_state.clone();
                async move {
                    match collect_activity(&st).await {
                        Ok(a) => a.total,
                        // Can't tell what's running: don't restart blind.
                        Err(e) => {
                            tracing::warn!("idle restart: activity check failed: {e:#}");
                            usize::MAX
                        }
                    }
                }
            },
            rx,
            IDLE_POLL,
            IDLE_CAP,
            move |remaining| {
                let mut guard = slot();
                if let Some(s) = guard.as_mut().filter(|s| s.id == id)
                    && s.info.remaining != remaining
                {
                    s.info.remaining = remaining;
                    let info = s.info.clone();
                    drop(guard);
                    broadcast_pending(&tick_state.broadcaster, &info);
                }
            },
        )
        .await;

        // Only the request that still owns the slot may act on it: a
        // replaced or cancelled one just exits.
        let owned = {
            let mut guard = slot();
            if guard.as_ref().is_some_and(|s| s.id == id) {
                guard.take()
            } else {
                None
            }
        };
        let Some(owned) = owned else { return };
        match outcome {
            IdleOutcome::Idle => {
                tracing::warn!("idle restart: nothing running — restarting");
                broadcast(
                    &state.broadcaster,
                    serde_json::json!({ "pending": owned.info, "restarting": true }),
                );
                tokio::time::sleep(Duration::from_millis(500)).await;
                do_restart(&exe);
            }
            IdleOutcome::TimedOut | IdleOutcome::Cancelled => {
                tracing::warn!(?outcome, "idle restart abandoned");
                broadcast(
                    &state.broadcaster,
                    serde_json::json!({ "pending": null, "restarting": false }),
                );
            }
        }
    });
    info
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn counter(readings: Vec<usize>) -> impl FnMut() -> std::future::Ready<usize> {
        let mut it = readings.into_iter();
        let mut last = usize::MAX;
        move || {
            if let Some(n) = it.next() {
                last = n;
            }
            std::future::ready(last)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn waits_until_two_consecutive_idle_reads() {
        let (_tx, rx) = watch::channel(false);
        let polls = Arc::new(AtomicUsize::new(0));
        let seen = polls.clone();
        // Busy, a one-poll idle gap, busy again, then settled.
        let outcome = wait_until_idle(
            counter(vec![2, 0, 1, 0, 0]),
            rx,
            Duration::from_secs(3),
            Duration::from_secs(3600),
            move |_| {
                seen.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await;
        assert_eq!(outcome, IdleOutcome::Idle);
        assert_eq!(
            polls.load(Ordering::SeqCst),
            5,
            "a single idle read between turns must not trigger the restart"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_stops_the_wait() {
        let (tx, rx) = watch::channel(false);
        let wait = tokio::spawn(wait_until_idle(
            counter(vec![3]),
            rx,
            Duration::from_secs(3),
            Duration::from_secs(3600),
            |_| {},
        ));
        tokio::time::sleep(Duration::from_secs(10)).await;
        tx.send(true).unwrap();
        assert_eq!(wait.await.unwrap(), IdleOutcome::Cancelled);
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_after_the_cap() {
        let (_tx, rx) = watch::channel(false);
        let outcome = wait_until_idle(
            counter(vec![1]),
            rx,
            Duration::from_secs(3),
            Duration::from_secs(30),
            |_| {},
        )
        .await;
        assert_eq!(outcome, IdleOutcome::TimedOut);
    }
}

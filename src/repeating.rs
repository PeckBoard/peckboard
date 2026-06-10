//! Repeating tasks: a scheduled prompt that spawns a fresh session on
//! each tick. The hard invariant — *only one running session per task at
//! a time* — is enforced through the same proof-token pattern as
//! [`crate::provider::manager::SessionLock`]:
//!
//! * Per-task tokio Mutex held across the "is any session for T currently
//!   running?" check AND the new-session spawn.
//! * The only function that creates the new session + dispatches it
//!   ([`RepeatingTaskManager::start_run_locked`]) takes a `&TaskLock`,
//!   so callers cannot bypass the lock.
//! * There is no public free function for "start a run for task T" —
//!   external callers must go through `try_run_now` / the scheduler
//!   loop, both of which acquire a lock first.
//!
//! Schedule format (`schedule_kind` / `schedule_value`):
//! - `interval`  → `{ "minutes": N }`         — fire every N minutes
//! - `daily`     → `{ "hour": H, "minute": M }` — fire daily at HH:MM UTC
//! - `weekly`    → `{ "weekday": 0..=6, "hour": H, "minute": M }` — fire weekly,
//!                  weekday is 0=Mon … 6=Sun (matches `chrono::Weekday::num_days_from_monday`)

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Datelike, Duration, Timelike, Utc};
use serde::Deserialize;
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::db::Db;
use crate::db::models::{NewSession, RepeatingTask, UpdateRepeatingTask};
use crate::provider::manager::SessionManager;
use crate::provider::stream::SpawnConfig;
use crate::service::mcp_server::{McpTokenRegistry, write_mcp_config};
use crate::ws::broadcaster::{Broadcaster, WsEvent};

/// Dependencies the manager needs to spawn a runnable session for a
/// task. Bundled into one struct so the method signatures don't grow
/// unboundedly each time we add a new wire-up requirement. Cheap to
/// construct from `&AppState` at every dispatch site.
pub struct RunContext<'a> {
    pub db: &'a Db,
    pub broadcaster: &'a Arc<Broadcaster>,
    pub session_manager: &'a SessionManager,
    pub mcp_tokens: &'a McpTokenRegistry,
    pub data_dir: &'a std::path::Path,
    pub http_port: u16,
}

/// Smallest practical interval. Stops the scheduler from chewing CPU and
/// blocks a stuck task from spawning a thousand sessions per second.
pub const MIN_INTERVAL_MINUTES: i64 = 1;

/// Outcome of an attempted run dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartOutcome {
    /// Spawned a fresh session and dispatched the prompt.
    Spawned,
    /// A previous run is still in flight; no new session was created.
    AlreadyRunning,
    /// Task is disabled. (Force-run with `respect_enabled = false`
    /// bypasses this.)
    Disabled,
}

/// Proof token: the bearer holds the per-task lock for `task_id`.
///
/// The only way to construct one is via `RepeatingTaskManager::lock_task`
/// or `try_lock_task`, so a `&TaskLock` parameter on `start_run_locked`
/// is a compile-time guarantee that the caller has serialised against
/// every other "is any session running? → spawn" decision for this task.
///
/// Without this token a careless future caller could just call
/// `db.create_session()` + `session_manager.send_or_queue()` and create
/// a parallel run. Making `start_run_locked` private and routing every
/// dispatch through `try_run_now` (which acquires the lock for them)
/// keeps that path closed.
pub struct TaskLock {
    _guard: OwnedMutexGuard<()>,
    task_id: String,
}

impl TaskLock {
    pub fn task_id(&self) -> &str {
        &self.task_id
    }
}

/// Per-task dispatch coordinator. Owns the per-task locks and the
/// scheduler tick loop. Cheap to clone (everything inside is `Arc`).
#[derive(Clone)]
pub struct RepeatingTaskManager {
    task_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl Default for RepeatingTaskManager {
    fn default() -> Self {
        Self::new()
    }
}

impl RepeatingTaskManager {
    pub fn new() -> Self {
        Self {
            task_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn lock_task(&self, task_id: &str) -> TaskLock {
        let lock = {
            let mut map = self.task_locks.lock().await;
            map.entry(task_id.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        TaskLock {
            _guard: lock.lock_owned().await,
            task_id: task_id.to_string(),
        }
    }

    async fn try_lock_task(&self, task_id: &str) -> Option<TaskLock> {
        let lock = {
            let mut map = self.task_locks.lock().await;
            map.entry(task_id.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        lock.try_lock_owned().ok().map(|g| TaskLock {
            _guard: g,
            task_id: task_id.to_string(),
        })
    }

    /// Drop task-lock entries that nobody else references. See the
    /// matching note on [`crate::provider::manager::SessionManager::evict_idle_locks`]
    /// for the safety argument: `Arc::strong_count == 1` means only
    /// the map holds the entry, so dropping it is invisible to live
    /// callers (the next `lock_task` re-creates it transparently).
    /// Returns the number of entries removed.
    pub async fn evict_idle_locks(&self) -> usize {
        let mut map = self.task_locks.lock().await;
        let before = map.len();
        map.retain(|_, lock| Arc::strong_count(lock) > 1);
        before - map.len()
    }

    /// Spawn a background task that periodically evicts idle task-lock
    /// entries. Returns the join handle for the caller to hold. See the
    /// matching note on [`crate::provider::manager::SessionManager::spawn_lock_sweeper`]
    /// for why this clones the inner map rather than taking `Arc<Self>`.
    pub fn spawn_lock_sweeper(&self) -> tokio::task::JoinHandle<()> {
        let locks = self.task_locks.clone();
        tokio::spawn(async move {
            const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);
            loop {
                tokio::time::sleep(SWEEP_INTERVAL).await;
                let mut map = locks.lock().await;
                let before = map.len();
                map.retain(|_, lock| Arc::strong_count(lock) > 1);
                let evicted = before - map.len();
                drop(map);
                if evicted > 0 {
                    tracing::debug!("Repeating-task lock sweep: evicted {evicted} idle entries");
                }
            }
        })
    }

    /// Force a run *right now* (e.g. operator clicked "Run now"). Acquires
    /// the per-task lock, checks no run is in flight, and dispatches.
    ///
    /// `respect_enabled = true` skips disabled tasks; the force-run route
    /// passes `false` so the operator can always trigger a one-off.
    pub async fn try_run_now(
        &self,
        task_id: &str,
        ctx: RunContext<'_>,
        respect_enabled: bool,
    ) -> anyhow::Result<StartOutcome> {
        let lock = self.lock_task(task_id).await;
        self.start_run_locked(&lock, &ctx, respect_enabled).await
    }

    /// One scheduler tick. Loads every due task and tries to start a run
    /// for each. Tasks whose lock is already held (currently dispatching
    /// from another path, e.g. force-run) are skipped this tick —
    /// `try_lock_task` is non-blocking so a long-running run can't starve
    /// the scheduler loop.
    pub async fn run_due_tasks(&self, ctx: RunContext<'_>) {
        let now = Utc::now().to_rfc3339();
        let due = match ctx.db.list_due_repeating_tasks(&now).await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("Failed to list due repeating tasks: {e}");
                return;
            }
        };

        for task in due {
            let lock = match self.try_lock_task(&task.id).await {
                Some(l) => l,
                None => {
                    tracing::debug!(task_id = %task.id, "Scheduler skipping task: lock held");
                    continue;
                }
            };
            match self.start_run_locked(&lock, &ctx, true).await {
                Ok(StartOutcome::Spawned) => {}
                Ok(other) => {
                    tracing::debug!(task_id = %task.id, ?other, "Scheduler tick: nothing to do");
                }
                Err(e) => tracing::error!(task_id = %task.id, "Scheduler dispatch failed: {e}"),
            }
        }
    }

    /// **Private.** Atomic dispatch: check is_running across all this
    /// task's existing sessions, and if none are live, create a fresh
    /// session, mark it as belonging to the task, dispatch the prompt,
    /// and recompute `next_run_at`.
    ///
    /// Callers MUST obtain a `&TaskLock` first — both public entry
    /// points (`try_run_now`, `run_due_tasks`) do this. The private
    /// scope means there is no way to bypass the lock without editing
    /// this module.
    async fn start_run_locked(
        &self,
        lock: &TaskLock,
        ctx: &RunContext<'_>,
        respect_enabled: bool,
    ) -> anyhow::Result<StartOutcome> {
        let task_id = lock.task_id();
        let db = ctx.db;
        let broadcaster = ctx.broadcaster;
        let session_manager = ctx.session_manager;

        // Reload the task inside the lock — the row may have been edited
        // (or deleted) between `list_due_repeating_tasks` and now.
        let task = match db.get_repeating_task(task_id).await? {
            Some(t) => t,
            None => {
                tracing::warn!(task_id = %task_id, "start_run_locked: task vanished");
                return Ok(StartOutcome::AlreadyRunning);
            }
        };

        if respect_enabled && !task.enabled {
            return Ok(StartOutcome::Disabled);
        }

        // Existence check: any session previously spawned by this task
        // that's still being processed by the provider. We scan the rows
        // for this task and ask the SessionManager whether any of them
        // are live. Cheap because the row count grows linearly per
        // schedule and `is_running` is an in-memory check.
        let existing = db.list_sessions_by_repeating_task(task_id).await?;
        for s in &existing {
            if session_manager.is_running(&s.id).await {
                tracing::debug!(
                    task_id = %task_id,
                    session_id = %s.id,
                    "Repeating task already has a running session; skipping",
                );
                // Still bump next_run_at so we don't spin every tick.
                let next = next_run_at_after(&task, Utc::now());
                let _ = db
                    .update_repeating_task(
                        task_id,
                        UpdateRepeatingTask {
                            next_run_at: Some(next),
                            updated_at: Some(Utc::now().to_rfc3339()),
                            ..Default::default()
                        },
                    )
                    .await;
                return Ok(StartOutcome::AlreadyRunning);
            }
        }

        let folder = db
            .get_folder(&task.folder_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("folder not found: {}", task.folder_id))?;

        let now_dt = Utc::now();
        let now = now_dt.to_rfc3339();
        let session_id = uuid::Uuid::new_v4().to_string();
        let session_name = format!("{} — {}", task.name, format_run_label(&now_dt));

        let session = db
            .create_session(NewSession {
                id: session_id.clone(),
                name: session_name,
                folder_id: task.folder_id.clone(),
                model: task.model.clone(),
                effort: task.effort.clone(),
                is_worker: false,
                project_id: None,
                card_id: None,
                conversation_id: None,
                created_at: now.clone(),
                last_activity: now.clone(),
                repeating_task_id: Some(task.id.clone()),
                ..Default::default()
            })
            .await?;

        // Persist the prompt as a user event so it shows up in the chat
        // transcript exactly like a human send. Without this the new
        // session would open with a blank scroll-back and the prompt
        // would only exist as the dispatched message.
        if let Ok(ev) = db
            .append_event(
                &session_id,
                "user",
                serde_json::json!({ "text": task.prompt, "source": "repeating-task" }),
            )
            .await
        {
            broadcaster.broadcast(WsEvent {
                event_type: "event".into(),
                session_id: session_id.clone(),
                data: serde_json::json!({
                    "id": ev.id,
                    "seq": ev.seq,
                    "ts": ev.ts,
                    "kind": ev.kind,
                    "data": { "text": task.prompt, "source": "repeating-task" },
                }),
            });
        }

        // Mint an MCP token + config for the spawned session so the
        // agent has access to ask_user, write_report, and the
        // repeating-task management tools just like an interactive
        // session would. The token is scoped to the session id (no
        // project_id since this is a plain session).
        let mcp_token = ctx.mcp_tokens.issue_token(session_id.clone(), None).await;
        let mcp_config_path: Option<PathBuf> =
            write_mcp_config(ctx.data_dir, &session_id, ctx.http_port, &mcp_token).ok();

        let config = SpawnConfig {
            working_dir: folder.path.clone(),
            model: task.model.clone().unwrap_or_else(|| "default".into()),
            effort: task.effort.clone(),
            mcp_config_path: mcp_config_path.map(|p| p.to_string_lossy().to_string()),
            env: Default::default(),
            permission_mode: Some("bypass".into()),
            timeout_ms: None,
            metadata: serde_json::Value::Null,
            system_prompt_suffix: Some(build_recurring_system_prompt(
                &task.name,
                &task.id,
                task.last_run_at.as_deref(),
                &now,
            )),
            restrict_to_qa: false,
        };

        // Dispatch via the regular send-or-queue path. The TaskLock is
        // held until this function returns, which guarantees no other
        // call can pass the is_running check above and double-spawn.
        let dispatch_result = session_manager
            .send_or_queue(&session_id, &task.prompt, db, broadcaster, config)
            .await;
        if let Err(e) = dispatch_result {
            // Dispatch failed: the session row already exists but no
            // provider run was kicked off. Best-effort surface that as
            // an agent-end crash event so the UI doesn't show an empty
            // session that "should" be running.
            let crash_data = serde_json::json!({
                "status": "crashed",
                "reason": format!("dispatch error: {e}"),
            });
            if let Ok(ev) = db
                .append_event(&session_id, "agent-end", crash_data.clone())
                .await
            {
                broadcaster.broadcast(WsEvent {
                    event_type: "event".into(),
                    session_id: session_id.clone(),
                    data: serde_json::json!({
                        "id": ev.id,
                        "seq": ev.seq,
                        "ts": ev.ts,
                        "kind": ev.kind,
                        "data": crash_data,
                    }),
                });
            }
            return Err(e);
        }

        // Update last_run_at + next_run_at. Compute next_run_at from
        // *now*, not from the previous next_run_at: if the scheduler
        // was paused or the machine slept past several due times we
        // catch up to "now + interval", not 12 retries in a row.
        let next = next_run_at_after(&task, now_dt);
        let _ = db
            .update_repeating_task(
                task_id,
                UpdateRepeatingTask {
                    last_run_at: Some(Some(now.clone())),
                    next_run_at: Some(next),
                    updated_at: Some(now.clone()),
                    ..Default::default()
                },
            )
            .await;

        broadcaster.broadcast(WsEvent {
            event_type: "repeating-task-run".into(),
            session_id: task.id.clone(),
            data: serde_json::json!({
                "taskId": task.id,
                "sessionId": session.id,
                "startedAt": now,
            }),
        });

        Ok(StartOutcome::Spawned)
    }
}

/// Parse + sanity-check a schedule descriptor. Used both at create/edit
/// time (so invalid input is rejected at the route boundary) and inside
/// `next_run_at_after` (so a corrupted row can't infinite-loop the
/// scheduler).
#[derive(Debug, Clone)]
pub enum Schedule {
    Interval {
        minutes: i64,
    },
    Daily {
        hour: u32,
        minute: u32,
    },
    Weekly {
        weekday: u32,
        hour: u32,
        minute: u32,
    },
}

#[derive(Deserialize)]
struct IntervalValue {
    minutes: i64,
}
#[derive(Deserialize)]
struct DailyValue {
    hour: u32,
    minute: u32,
}
#[derive(Deserialize)]
struct WeeklyValue {
    weekday: u32,
    hour: u32,
    minute: u32,
}

impl Schedule {
    pub fn parse(kind: &str, value_json: &str) -> anyhow::Result<Self> {
        match kind {
            "interval" => {
                let v: IntervalValue = serde_json::from_str(value_json)
                    .map_err(|e| anyhow::anyhow!("invalid interval schedule value: {e}"))?;
                if v.minutes < MIN_INTERVAL_MINUTES {
                    anyhow::bail!(
                        "interval minutes must be >= {MIN_INTERVAL_MINUTES} (got {})",
                        v.minutes
                    );
                }
                Ok(Schedule::Interval { minutes: v.minutes })
            }
            "daily" => {
                let v: DailyValue = serde_json::from_str(value_json)
                    .map_err(|e| anyhow::anyhow!("invalid daily schedule value: {e}"))?;
                if v.hour > 23 || v.minute > 59 {
                    anyhow::bail!("daily hour/minute out of range");
                }
                Ok(Schedule::Daily {
                    hour: v.hour,
                    minute: v.minute,
                })
            }
            "weekly" => {
                let v: WeeklyValue = serde_json::from_str(value_json)
                    .map_err(|e| anyhow::anyhow!("invalid weekly schedule value: {e}"))?;
                if v.weekday > 6 || v.hour > 23 || v.minute > 59 {
                    anyhow::bail!("weekly weekday/hour/minute out of range");
                }
                Ok(Schedule::Weekly {
                    weekday: v.weekday,
                    hour: v.hour,
                    minute: v.minute,
                })
            }
            other => anyhow::bail!("unknown schedule_kind: {other}"),
        }
    }
}

/// Compute the next `next_run_at` *after* `now` for a task. Returns
/// `None` only if the schedule string is corrupted, in which case the
/// scheduler will keep the row idle instead of spinning.
pub fn next_run_at_after(task: &RepeatingTask, now: DateTime<Utc>) -> Option<String> {
    let sched = match Schedule::parse(&task.schedule_kind, &task.schedule_value) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(task_id = %task.id, "Corrupt schedule, leaving next_run_at unset: {e}");
            return None;
        }
    };
    let next = match sched {
        Schedule::Interval { minutes } => {
            // Floor to whole minutes so the timestamps stay readable.
            let next = now + Duration::minutes(minutes);
            next.with_second(0).and_then(|t| t.with_nanosecond(0))?
        }
        Schedule::Daily { hour, minute } => {
            let mut candidate = now
                .with_hour(hour)?
                .with_minute(minute)?
                .with_second(0)?
                .with_nanosecond(0)?;
            if candidate <= now {
                candidate += Duration::days(1);
            }
            candidate
        }
        Schedule::Weekly {
            weekday,
            hour,
            minute,
        } => {
            let mut candidate = now
                .with_hour(hour)?
                .with_minute(minute)?
                .with_second(0)?
                .with_nanosecond(0)?;
            // Step to the matching weekday at or after the current day.
            let current = now.weekday().num_days_from_monday();
            let mut delta = (weekday + 7 - current) % 7;
            if delta == 0 && candidate <= now {
                delta = 7;
            }
            candidate += Duration::days(delta as i64);
            candidate
        }
    };
    Some(next.to_rfc3339())
}

/// First `next_run_at` for a newly-created task: same as `next_run_at_after`
/// but rounded down to the next whole minute when "interval" so the
/// first tick lines up with a human-readable clock.
pub fn initial_next_run_at(task: &RepeatingTask) -> Option<String> {
    next_run_at_after(task, Utc::now())
}

fn format_run_label(t: &DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// Per-spawn system prompt baked into every repeating-task run. Tells
/// the agent it's running as part of a recurring task (so it can frame
/// its work accordingly) and points at a notes-file convention for
/// carrying context across runs without history persistence.
///
/// Compact-not-append is the load-bearing instruction: each run starts
/// fresh, so a naively-appending notes file would grow without bound and
/// become useless to read at the top of every run. Phrasing it as
/// "rewrite, don't append" — and giving the agent the explicit
/// permission to drop stale details — is what keeps the file small
/// enough to be cheap context on every tick.
pub fn build_recurring_system_prompt(
    task_name: &str,
    task_id: &str,
    last_run_at: Option<&str>,
    now_rfc3339: &str,
) -> String {
    let last_run = match last_run_at {
        Some(t) => format!("Previous run finished around {t}."),
        None => "This is the first run.".to_string(),
    };
    let notes_path = format!(".peckboard/repeating-tasks/{task_id}.md");
    format!(
        "# Repeating Task Context\n\
\n\
You are running as part of a recurring task: \"{task_name}\".\n\
- This run started at {now_rfc3339}.\n\
- {last_run}\n\
- Each run spawns a fresh session — no chat history is carried over from previous runs.\n\
- This could be the first run or the 100th; assume nothing about prior context being in your conversation.\n\
\n\
## Carrying Context Across Runs\n\
\n\
To remember things between runs, maintain a compact notes file at this path inside the working directory:\n\
\n\
    {notes_path}\n\
\n\
Treat it as long-term memory, NOT an event log:\n\
- Read it at the start of your run if it exists.\n\
- At the end of your run, REWRITE it (don't just append) so it stays short and skimmable.\n\
- Keep only what's load-bearing for future runs: standing decisions, the current state of the recurring work, deadlines, things you should not redo next time, open questions still waiting on the user.\n\
- Drop stale details, one-off observations, and the play-by-play of this run.\n\
- Only switch to append-style logging if the user has explicitly asked you to keep a running log.\n\
- Aim for a file that's quick to read on every run, not one that grows unbounded.\n\
"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn make_task(kind: &str, value: &str) -> RepeatingTask {
        RepeatingTask {
            id: "t1".into(),
            name: "Task".into(),
            description: "".into(),
            folder_id: "f1".into(),
            prompt: "do thing".into(),
            schedule_kind: kind.into(),
            schedule_value: value.into(),
            model: None,
            effort: None,
            enabled: true,
            next_run_at: None,
            last_run_at: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn schedule_parse_interval_rejects_zero() {
        let err = Schedule::parse("interval", r#"{"minutes":0}"#).unwrap_err();
        assert!(err.to_string().contains("minutes must be"));
    }

    #[test]
    fn schedule_parse_rejects_unknown_kind() {
        assert!(Schedule::parse("cron", "{}").is_err());
    }

    #[test]
    fn schedule_parse_daily_validates_range() {
        assert!(Schedule::parse("daily", r#"{"hour":24,"minute":0}"#).is_err());
        assert!(Schedule::parse("daily", r#"{"hour":12,"minute":60}"#).is_err());
        assert!(Schedule::parse("daily", r#"{"hour":12,"minute":30}"#).is_ok());
    }

    #[test]
    fn next_run_interval_steps_forward_by_minutes() {
        let task = make_task("interval", r#"{"minutes":30}"#);
        let now = Utc.with_ymd_and_hms(2026, 6, 9, 10, 15, 22).unwrap();
        let next = next_run_at_after(&task, now).unwrap();
        let parsed: DateTime<Utc> = next.parse().unwrap();
        assert_eq!(parsed, Utc.with_ymd_and_hms(2026, 6, 9, 10, 45, 0).unwrap());
    }

    #[test]
    fn next_run_daily_uses_tomorrow_when_time_already_passed() {
        let task = make_task("daily", r#"{"hour":9,"minute":0}"#);
        let now = Utc.with_ymd_and_hms(2026, 6, 9, 10, 0, 0).unwrap();
        let next: DateTime<Utc> = next_run_at_after(&task, now).unwrap().parse().unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 6, 10, 9, 0, 0).unwrap());
    }

    #[test]
    fn next_run_daily_uses_today_when_time_still_future() {
        let task = make_task("daily", r#"{"hour":15,"minute":30}"#);
        let now = Utc.with_ymd_and_hms(2026, 6, 9, 10, 0, 0).unwrap();
        let next: DateTime<Utc> = next_run_at_after(&task, now).unwrap().parse().unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 6, 9, 15, 30, 0).unwrap());
    }

    #[test]
    fn next_run_weekly_steps_to_next_match() {
        // 2026-06-09 is a Tuesday (Mon=0). Schedule fires Wed (weekday=2)
        // at 09:00; from Tue 10:00 the next fire is Wed 09:00.
        let task = make_task("weekly", r#"{"weekday":2,"hour":9,"minute":0}"#);
        let now = Utc.with_ymd_and_hms(2026, 6, 9, 10, 0, 0).unwrap();
        let next: DateTime<Utc> = next_run_at_after(&task, now).unwrap().parse().unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 6, 10, 9, 0, 0).unwrap());
    }

    #[test]
    fn recurring_system_prompt_includes_task_name_and_notes_path() {
        let body = build_recurring_system_prompt(
            "Inventory sweep",
            "task-uuid-123",
            None,
            "2026-06-09T10:00:00Z",
        );
        assert!(body.contains("Inventory sweep"));
        assert!(body.contains(".peckboard/repeating-tasks/task-uuid-123.md"));
        assert!(body.contains("This is the first run."));
        assert!(body.contains("2026-06-09T10:00:00Z"));
        // The compact-not-append rule is load-bearing — make sure
        // future edits can't quietly drop it without flipping this test.
        assert!(body.contains("REWRITE it"));
        assert!(body.contains("don't just append"));
    }

    #[test]
    fn recurring_system_prompt_mentions_previous_run() {
        let body = build_recurring_system_prompt(
            "Daily digest",
            "task-uuid-456",
            Some("2026-06-08T09:00:00Z"),
            "2026-06-09T09:00:00Z",
        );
        assert!(body.contains("Previous run finished around 2026-06-08T09:00:00Z"));
        assert!(!body.contains("This is the first run."));
    }

    #[tokio::test]
    async fn evict_idle_locks_removes_unheld_task_entries() {
        let m = RepeatingTaskManager::new();
        drop(m.lock_task("t1").await);
        drop(m.lock_task("t2").await);
        assert_eq!(m.task_locks.lock().await.len(), 2);

        let evicted = m.evict_idle_locks().await;
        assert_eq!(evicted, 2);
        assert_eq!(m.task_locks.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn evict_idle_locks_keeps_active_task_entries() {
        let m = RepeatingTaskManager::new();
        let live = m.lock_task("active").await;
        drop(m.lock_task("idle").await);

        let evicted = m.evict_idle_locks().await;
        assert_eq!(evicted, 1);
        let remaining = m.task_locks.lock().await;
        assert!(remaining.contains_key("active"));
        assert!(!remaining.contains_key("idle"));
        drop(live);
    }

    #[test]
    fn next_run_weekly_same_day_after_window_uses_next_week() {
        // Same weekday but the schedule time already passed today.
        let task = make_task("weekly", r#"{"weekday":1,"hour":8,"minute":0}"#);
        let now = Utc.with_ymd_and_hms(2026, 6, 9, 10, 0, 0).unwrap();
        let next: DateTime<Utc> = next_run_at_after(&task, now).unwrap().parse().unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 6, 16, 8, 0, 0).unwrap());
    }
}

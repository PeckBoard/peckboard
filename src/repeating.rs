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

use std::collections::{HashMap, HashSet, VecDeque};
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
    /// Watchdog that observes runs and refuses dispatch (or, on its own
    /// audit loop, retroactively disables the task) if a scheduler-run
    /// would violate the "never run quicker than the schedule, never
    /// more than once per minute" invariant. See [`RunAuditor`].
    pub auditor: &'a RunAuditor,
}

/// Smallest practical interval. Stops the scheduler from chewing CPU and
/// blocks a stuck task from spawning a thousand sessions per second.
pub const MIN_INTERVAL_MINUTES: i64 = 1;

/// Absolute floor on the gap between two consecutive *scheduler*-initiated
/// runs of the same task, regardless of what the schedule says. Even an
/// `interval=1` task should never produce two scheduler runs in the same
/// minute — that pattern is the canonical "scheduler is wedged in a
/// bug loop" symptom and worth refusing on principle.
///
/// Manual ("Run now") runs are exempt: an operator clicking the button
/// twice is intentional and not a bug.
pub const MIN_SCHEDULER_GAP_SECONDS: i64 = 60;

/// Slack subtracted from the schedule-prescribed gap when deciding
/// whether a scheduler tick is "too early". The scheduler ticks every
/// 30 seconds, so a run scheduled for `T` may fire as late as `T + 30s`;
/// the next run scheduled for `T + interval` could be evaluated at
/// `T + interval - ε` if the *next* tick lands a hair early. Giving a
/// 30s tolerance avoids flagging this normal jitter as a violation.
pub const SCHEDULER_GAP_SLOP_SECONDS: i64 = 30;

/// Did this dispatch come from the scheduler tick (subject to throttle)
/// or from a human clicking "Run now" (always allowed)?
///
/// Carried explicitly through the dispatch path rather than overloading
/// `respect_enabled` so a future reader can't mistakenly conflate the
/// two — they happened to coincide today, but "Run now ignores
/// `enabled`" and "Run now ignores throttle" are independent product
/// decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunTrigger {
    /// Operator clicked "Run now" (POST /api/repeating-tasks/:id/run).
    /// Bypasses the run-policy throttle entirely.
    Manual,
    /// Periodic scheduler tick (`run_due_tasks`). Subject to the throttle.
    Scheduler,
}

/// Outcome of an attempted run dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    /// Spawned a fresh session and dispatched the prompt.
    Spawned,
    /// A previous run is still in flight; no new session was created.
    AlreadyRunning,
    /// Task is disabled. (Force-run with `respect_enabled = false`
    /// bypasses this.)
    Disabled,
    /// The run-policy guard refused the dispatch because the gap since
    /// `last_run_at` is below the schedule's minimum. Carries a human-
    /// readable reason for log/UI surfacing. Only Scheduler-triggered
    /// runs can be throttled.
    Throttled(String),
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
    ///
    /// Manual runs always bypass the run-policy throttle — see [`RunTrigger`].
    pub async fn try_run_now(
        &self,
        task_id: &str,
        ctx: RunContext<'_>,
        respect_enabled: bool,
    ) -> anyhow::Result<StartOutcome> {
        let lock = self.lock_task(task_id).await;
        self.start_run_locked(&lock, &ctx, respect_enabled, RunTrigger::Manual)
            .await
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
            match self
                .start_run_locked(&lock, &ctx, true, RunTrigger::Scheduler)
                .await
            {
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
        trigger: RunTrigger,
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

        // Inline run-policy guard. Manual triggers (operator clicked
        // "Run now") are explicitly exempt — the user asked for that
        // run, and refusing it would be confusing. Scheduler triggers
        // are gated so that a corrupted next_run_at, a wedged tick
        // loop, or any future bug that flips a task into "fire on
        // every tick" mode can't actually fire faster than the
        // schedule allows.
        let now_dt = Utc::now();
        if let PolicyDecision::Throttle(reason) =
            check_run_policy(&task, task.last_run_at.as_deref(), now_dt, trigger)
        {
            tracing::warn!(
                task_id = %task_id,
                ?trigger,
                "Repeating-task run policy refused dispatch: {reason}",
            );
            // Bump next_run_at past the schedule floor so the scheduler
            // tick doesn't keep picking the same row up every 30s and
            // re-throttling it. Without this, a corrupted next_run_at
            // pointing into the past would loop the scheduler through
            // the guard every tick.
            let next = next_run_at_after(&task, now_dt);
            let _ = db
                .update_repeating_task(
                    task_id,
                    UpdateRepeatingTask {
                        next_run_at: Some(next),
                        updated_at: Some(now_dt.to_rfc3339()),
                        ..Default::default()
                    },
                )
                .await;
            return Ok(StartOutcome::Throttled(reason));
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

        // Recompute now() in case the policy check above took non-trivial
        // time (it shouldn't, but `now` is also the `last_activity` /
        // `created_at` we'll persist, and re-reading it keeps those
        // fields close to wall-clock).
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
            .send_or_queue(
                &session_id,
                crate::provider::message::UserMessage::from_text(task.prompt.clone()),
                db,
                broadcaster,
                config,
            )
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

        // Feed the auditor. For scheduler-initiated dispatches we record
        // the timestamp into the per-task history; the audit loop later
        // walks that history (and, independently, the session rows in
        // the DB) to detect any pair of scheduler runs that are closer
        // together than the schedule allows. Manual dispatches mark the
        // session id so the audit pass can skip it — a human clicking
        // "Run now" twice in 10 seconds is intentional and not a bug.
        match trigger {
            RunTrigger::Scheduler => {
                ctx.auditor
                    .record_scheduler_dispatch(&task.id, now_dt)
                    .await
            }
            RunTrigger::Manual => ctx.auditor.mark_manual_session(&session.id).await,
        }

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

// ── Run-policy guard ──────────────────────────────────────────────────
//
// Two parallel-but-independent checks enforce the invariant "a scheduler-
// triggered repeating-task run must not fire faster than its schedule
// allows, and never more than once per minute":
//
// 1. [`check_run_policy`] — synchronous pre-dispatch guard called from
//    [`RepeatingTaskManager::start_run_locked`]. Decides on the spot,
//    using `task.last_run_at` from the row reloaded inside the lock.
//
// 2. [`RunAuditor`] — independent post-dispatch watchdog. Periodically
//    audits the actual session rows persisted in the DB for each task,
//    looking at consecutive scheduler-spawned `created_at` timestamps,
//    and surfaces / kill-switches any violations the inline guard
//    failed to prevent.
//
// The two layers intentionally observe DIFFERENT ground truths:
//
// - Inline guard reads `repeating_tasks.last_run_at` (set by the
//   dispatch path).
// - Watchdog reads `sessions.created_at` filtered by
//   `repeating_task_id` (set when the session row is inserted).
//
// If a future refactor breaks one of those side effects, the other
// layer keeps catching the violation.

/// Result of the inline policy check. Throttle messages are surfaced
/// in logs and the API response, so they should be human-actionable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    Allow,
    Throttle(String),
}

/// Minimum gap (in seconds) the policy will allow between two
/// *scheduler*-initiated runs of `task`. Combines the absolute floor
/// ([`MIN_SCHEDULER_GAP_SECONDS`]) with the schedule's own minimum
/// cadence; the larger of the two wins.
///
/// For `interval` tasks this is `max(minutes * 60, 60)`. For
/// `daily`/`weekly` tasks the natural cadence is huge, but we still
/// apply the 60-second floor so a corrupted schedule that yields
/// "fire every tick" can't slip through.
pub fn min_run_gap_seconds(task: &RepeatingTask) -> i64 {
    let schedule_floor = Schedule::parse(&task.schedule_kind, &task.schedule_value)
        .ok()
        .map(|s| match s {
            Schedule::Interval { minutes } => minutes.saturating_mul(60),
            Schedule::Daily { .. } => 86_400,
            Schedule::Weekly { .. } => 7 * 86_400,
        })
        .unwrap_or(MIN_SCHEDULER_GAP_SECONDS);
    schedule_floor.max(MIN_SCHEDULER_GAP_SECONDS)
}

/// Decide whether `task` may dispatch a fresh run *right now*.
///
/// Manual triggers always return [`PolicyDecision::Allow`] — see the
/// note on [`RunTrigger::Manual`]. Scheduler triggers require either:
///   - no previous run on record, or
///   - `now - last_run_at >= min_run_gap_seconds(task) - SCHEDULER_GAP_SLOP_SECONDS`,
///     AND `now - last_run_at >= MIN_SCHEDULER_GAP_SECONDS` (the slop
///     never lets the hard floor be breached).
///
/// `last_run_at_rfc3339` is taken from `task.last_run_at` by the
/// caller; passed in explicitly so tests can pin a known value.
pub fn check_run_policy(
    task: &RepeatingTask,
    last_run_at_rfc3339: Option<&str>,
    now: DateTime<Utc>,
    trigger: RunTrigger,
) -> PolicyDecision {
    if matches!(trigger, RunTrigger::Manual) {
        return PolicyDecision::Allow;
    }
    let last_run_at: Option<DateTime<Utc>> = last_run_at_rfc3339
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));
    let Some(last) = last_run_at else {
        return PolicyDecision::Allow;
    };
    let gap = (now - last).num_seconds();
    if gap < MIN_SCHEDULER_GAP_SECONDS {
        return PolicyDecision::Throttle(format!(
            "scheduler run blocked: only {gap}s since last run (hard floor is \
             {MIN_SCHEDULER_GAP_SECONDS}s)",
        ));
    }
    let schedule_floor = min_run_gap_seconds(task);
    let allowed = (schedule_floor - SCHEDULER_GAP_SLOP_SECONDS).max(MIN_SCHEDULER_GAP_SECONDS);
    if gap < allowed {
        return PolicyDecision::Throttle(format!(
            "scheduler run blocked: only {gap}s since last run for task with \
             schedule_kind={} (allowed minimum is {allowed}s)",
            task.schedule_kind,
        ));
    }
    PolicyDecision::Allow
}

// ── Watchdog ─────────────────────────────────────────────────────────

/// Cap on the per-task in-memory history. Twenty entries is enough to
/// notice a runaway loop within seconds (each pair of timestamps is
/// checked) without growing without bound.
const AUDITOR_HISTORY_CAP: usize = 20;

/// Per-task list of recent scheduler-spawn timestamps. A `VecDeque` so
/// we can drop the oldest entry in `O(1)` when the cap is reached.
type DispatchHistory = VecDeque<DateTime<Utc>>;

/// Inner state of the auditor. Hidden behind a `Mutex` so the manager
/// can record dispatches from any task path without callers caring
/// about locking.
#[derive(Default)]
struct AuditorState {
    /// `task_id -> [ts1, ts2, ...]` for scheduler-triggered runs only.
    scheduler_history: HashMap<String, DispatchHistory>,
    /// Session ids the auditor knows came from a manual force-run —
    /// excluded from the DB-side audit.
    manual_session_ids: HashSet<String>,
}

/// Description of a single watchdog violation for log + WS surfacing.
#[derive(Debug, Clone)]
pub struct WatchdogViolation {
    pub task_id: String,
    pub gap_seconds: i64,
    pub min_gap_seconds: i64,
    pub source: ViolationSource,
}

/// Which observation path caught the violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViolationSource {
    /// Detected in the auditor's own in-memory dispatch log.
    InMemoryHistory,
    /// Detected by scanning persisted `sessions` rows for the task.
    PersistedSessions,
}

/// Independent observer of repeating-task runs. The auditor doesn't
/// participate in dispatch decisions itself — its job is to scream when
/// the dispatch path produces an outcome that violates the invariant.
///
/// Cheap to clone; everything inside is `Arc`-wrapped.
#[derive(Clone)]
pub struct RunAuditor {
    state: Arc<Mutex<AuditorState>>,
    /// Sessions older than `audit_start` are ignored. Set when the
    /// auditor is constructed (typically at process start) so a
    /// restart doesn't false-positive on historical sessions whose
    /// manual-vs-scheduler classification is no longer in memory.
    audit_start: DateTime<Utc>,
}

impl Default for RunAuditor {
    fn default() -> Self {
        Self::new()
    }
}

impl RunAuditor {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(AuditorState::default())),
            audit_start: Utc::now(),
        }
    }

    /// Construct an auditor pinned to a specific start time. Used by
    /// tests that want to control which seeded sessions fall inside
    /// the audit window.
    pub fn with_start(audit_start: DateTime<Utc>) -> Self {
        Self {
            state: Arc::new(Mutex::new(AuditorState::default())),
            audit_start,
        }
    }

    /// Record a successful scheduler dispatch. Bounded ring buffer.
    pub async fn record_scheduler_dispatch(&self, task_id: &str, at: DateTime<Utc>) {
        let mut state = self.state.lock().await;
        let hist = state
            .scheduler_history
            .entry(task_id.to_string())
            .or_default();
        hist.push_back(at);
        while hist.len() > AUDITOR_HISTORY_CAP {
            hist.pop_front();
        }
    }

    /// Mark a session as a manual force-run so the DB audit pass skips
    /// it. Capped at a generous-but-bounded size so a malicious caller
    /// can't grow the set forever.
    pub async fn mark_manual_session(&self, session_id: &str) {
        const MAX_MANUAL_IDS: usize = 10_000;
        let mut state = self.state.lock().await;
        // Best-effort eviction: when the cap is hit we drop an arbitrary
        // entry. Any "real" manual session that gets evicted will, at
        // worst, be classified as scheduler in the DB audit — which is
        // the conservative (i.e. complaint-prone) direction.
        if state.manual_session_ids.len() >= MAX_MANUAL_IDS {
            if let Some(first) = state.manual_session_ids.iter().next().cloned() {
                state.manual_session_ids.remove(&first);
            }
        }
        state.manual_session_ids.insert(session_id.to_string());
    }

    /// Forget a session id. Used when a session is deleted so the set
    /// doesn't accumulate dead pointers.
    pub async fn forget_session(&self, session_id: &str) {
        let mut state = self.state.lock().await;
        state.manual_session_ids.remove(session_id);
    }

    /// In-memory check: walks the per-task scheduler history and flags
    /// any pair of consecutive timestamps closer than the schedule's
    /// minimum gap. Returns one violation per offending task (the
    /// tightest gap observed).
    pub async fn audit_in_memory(&self, tasks: &[RepeatingTask]) -> Vec<WatchdogViolation> {
        let state = self.state.lock().await;
        let mut out = Vec::new();
        for task in tasks {
            let Some(hist) = state.scheduler_history.get(&task.id) else {
                continue;
            };
            if hist.len() < 2 {
                continue;
            }
            let min_gap = min_run_gap_seconds(task);
            // Walk the history once, record the tightest gap.
            let mut tightest: Option<i64> = None;
            let mut prev = hist[0];
            for &ts in hist.iter().skip(1) {
                let gap = (ts - prev).num_seconds();
                if gap < min_gap && tightest.is_none_or(|t| gap < t) {
                    tightest = Some(gap);
                }
                prev = ts;
            }
            if let Some(gap) = tightest {
                out.push(WatchdogViolation {
                    task_id: task.id.clone(),
                    gap_seconds: gap,
                    min_gap_seconds: min_gap,
                    source: ViolationSource::InMemoryHistory,
                });
            }
        }
        out
    }

    /// DB-side check: queries persisted `sessions` rows tied to each
    /// task, drops anything the auditor knows is manual, drops anything
    /// older than `audit_start`, and flags consecutive scheduler-spawn
    /// `created_at` timestamps that are too close together.
    ///
    /// Independent from `audit_in_memory`: a refactor that breaks
    /// `record_scheduler_dispatch` (so `audit_in_memory` sees nothing)
    /// is still caught here, because session rows are produced by an
    /// entirely different code path.
    pub async fn audit_persisted(
        &self,
        db: &Db,
        tasks: &[RepeatingTask],
    ) -> Vec<WatchdogViolation> {
        let manual = {
            let state = self.state.lock().await;
            state.manual_session_ids.clone()
        };
        let audit_start = self.audit_start;
        let mut out = Vec::new();
        for task in tasks {
            let sessions = match db.list_sessions_by_repeating_task(&task.id).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.id,
                        "Auditor: list_sessions_by_repeating_task failed: {e}",
                    );
                    continue;
                }
            };
            // list_sessions_by_repeating_task returns newest-first; the
            // gap check needs chronological order.
            let mut scheduled: Vec<DateTime<Utc>> = sessions
                .into_iter()
                .filter(|s| !manual.contains(&s.id))
                .filter_map(|s| DateTime::parse_from_rfc3339(&s.created_at).ok())
                .map(|d| d.with_timezone(&Utc))
                .filter(|ts| *ts >= audit_start)
                .collect();
            if scheduled.len() < 2 {
                continue;
            }
            scheduled.sort();
            let min_gap = min_run_gap_seconds(task);
            let mut tightest: Option<i64> = None;
            let mut prev = scheduled[0];
            for ts in scheduled.iter().skip(1) {
                let gap = (*ts - prev).num_seconds();
                if gap < min_gap && tightest.is_none_or(|t| gap < t) {
                    tightest = Some(gap);
                }
                prev = *ts;
            }
            if let Some(gap) = tightest {
                out.push(WatchdogViolation {
                    task_id: task.id.clone(),
                    gap_seconds: gap,
                    min_gap_seconds: min_gap,
                    source: ViolationSource::PersistedSessions,
                });
            }
        }
        out
    }

    /// One audit pass: run both checks, broadcast each violation, and
    /// kill-switch the offending task (disable + clear `next_run_at`)
    /// so the bug can't keep firing while a human investigates. The
    /// kill switch is idempotent — disabling an already-disabled task
    /// is a no-op for the scheduler.
    pub async fn audit_pass(&self, db: &Db, broadcaster: &Arc<Broadcaster>) -> usize {
        let tasks = match db.list_repeating_tasks().await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("Auditor: list_repeating_tasks failed: {e}");
                return 0;
            }
        };
        let mut violations = self.audit_in_memory(&tasks).await;
        violations.extend(self.audit_persisted(db, &tasks).await);
        let count = violations.len();
        for v in violations {
            tracing::error!(
                task_id = %v.task_id,
                gap_seconds = v.gap_seconds,
                min_gap_seconds = v.min_gap_seconds,
                ?v.source,
                "Repeating-task watchdog: invariant violated; disabling task",
            );
            let now = Utc::now().to_rfc3339();
            let _ = db
                .update_repeating_task(
                    &v.task_id,
                    UpdateRepeatingTask {
                        enabled: Some(false),
                        next_run_at: Some(None),
                        updated_at: Some(now.clone()),
                        ..Default::default()
                    },
                )
                .await;
            broadcaster.broadcast(WsEvent {
                event_type: "repeating-task-watchdog".into(),
                session_id: v.task_id.clone(),
                data: serde_json::json!({
                    "taskId": v.task_id,
                    "gapSeconds": v.gap_seconds,
                    "minGapSeconds": v.min_gap_seconds,
                    "source": match v.source {
                        ViolationSource::InMemoryHistory => "in_memory_history",
                        ViolationSource::PersistedSessions => "persisted_sessions",
                    },
                    "action": "disabled",
                    "at": now,
                }),
            });
        }
        count
    }

    /// Background loop: tick `audit_pass` every `interval`. Returns the
    /// `JoinHandle` so `main` can hold it for the process lifetime.
    pub fn spawn_audit_loop(
        &self,
        db: Db,
        broadcaster: Arc<Broadcaster>,
        interval: std::time::Duration,
    ) -> tokio::task::JoinHandle<()> {
        let auditor = self.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(interval);
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            t.tick().await; // skip the immediate first tick
            loop {
                t.tick().await;
                let _ = auditor.audit_pass(&db, &broadcaster).await;
            }
        })
    }
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

    // ── Run-policy guard tests ────────────────────────────────────

    fn task_with_last_run(
        kind: &str,
        value: &str,
        last_run: Option<DateTime<Utc>>,
    ) -> RepeatingTask {
        let mut t = make_task(kind, value);
        t.last_run_at = last_run.map(|d| d.to_rfc3339());
        t
    }

    #[test]
    fn policy_allows_first_scheduler_run_when_no_history() {
        let task = make_task("interval", r#"{"minutes":5}"#);
        let now = Utc.with_ymd_and_hms(2026, 6, 9, 10, 0, 0).unwrap();
        assert_eq!(
            check_run_policy(&task, None, now, RunTrigger::Scheduler),
            PolicyDecision::Allow,
        );
    }

    #[test]
    fn policy_blocks_scheduler_run_below_hard_floor() {
        let now = Utc.with_ymd_and_hms(2026, 6, 9, 10, 0, 0).unwrap();
        // interval=1min schedule, but only 30s since last run — must
        // refuse on the 60s hard floor.
        let task = task_with_last_run(
            "interval",
            r#"{"minutes":1}"#,
            Some(now - Duration::seconds(30)),
        );
        let decision = check_run_policy(
            &task,
            task.last_run_at.as_deref(),
            now,
            RunTrigger::Scheduler,
        );
        match decision {
            PolicyDecision::Throttle(reason) => {
                assert!(reason.contains("hard floor"), "got: {reason}");
            }
            PolicyDecision::Allow => panic!("expected throttle, got Allow"),
        }
    }

    #[test]
    fn policy_blocks_scheduler_run_below_schedule_floor() {
        let now = Utc.with_ymd_and_hms(2026, 6, 9, 10, 0, 0).unwrap();
        // interval=5min schedule. Last run 100s ago — past the 60s
        // hard floor, but well below the 5min schedule (allowing 30s
        // tick slop, the floor is 270s).
        let task = task_with_last_run(
            "interval",
            r#"{"minutes":5}"#,
            Some(now - Duration::seconds(100)),
        );
        let decision = check_run_policy(
            &task,
            task.last_run_at.as_deref(),
            now,
            RunTrigger::Scheduler,
        );
        assert!(
            matches!(decision, PolicyDecision::Throttle(_)),
            "expected throttle, got {decision:?}",
        );
    }

    #[test]
    fn policy_allows_scheduler_run_after_schedule_floor() {
        let now = Utc.with_ymd_and_hms(2026, 6, 9, 10, 0, 0).unwrap();
        let task = task_with_last_run(
            "interval",
            r#"{"minutes":5}"#,
            Some(now - Duration::seconds(300)),
        );
        assert_eq!(
            check_run_policy(
                &task,
                task.last_run_at.as_deref(),
                now,
                RunTrigger::Scheduler
            ),
            PolicyDecision::Allow,
        );
    }

    #[test]
    fn policy_allows_manual_runs_regardless_of_gap() {
        let now = Utc.with_ymd_and_hms(2026, 6, 9, 10, 0, 0).unwrap();
        // Last run 1 second ago — manual must still be allowed.
        let task = task_with_last_run(
            "interval",
            r#"{"minutes":60}"#,
            Some(now - Duration::seconds(1)),
        );
        assert_eq!(
            check_run_policy(&task, task.last_run_at.as_deref(), now, RunTrigger::Manual),
            PolicyDecision::Allow,
        );
    }

    #[test]
    fn policy_applies_60s_floor_to_daily_and_weekly_tasks() {
        let now = Utc.with_ymd_and_hms(2026, 6, 9, 10, 0, 0).unwrap();
        for (kind, value) in [
            ("daily", r#"{"hour":10,"minute":0}"#),
            ("weekly", r#"{"weekday":1,"hour":10,"minute":0}"#),
        ] {
            let task = task_with_last_run(kind, value, Some(now - Duration::seconds(30)));
            let decision = check_run_policy(
                &task,
                task.last_run_at.as_deref(),
                now,
                RunTrigger::Scheduler,
            );
            assert!(
                matches!(decision, PolicyDecision::Throttle(_)),
                "expected throttle for {kind}, got {decision:?}",
            );
        }
    }

    #[test]
    fn min_run_gap_seconds_picks_max_of_hard_floor_and_schedule() {
        let one_min = make_task("interval", r#"{"minutes":1}"#);
        assert_eq!(min_run_gap_seconds(&one_min), 60);
        let five_min = make_task("interval", r#"{"minutes":5}"#);
        assert_eq!(min_run_gap_seconds(&five_min), 300);
        let daily = make_task("daily", r#"{"hour":10,"minute":0}"#);
        assert_eq!(min_run_gap_seconds(&daily), 86_400);
    }

    // ── Watchdog tests ────────────────────────────────────────────

    #[tokio::test]
    async fn auditor_in_memory_flags_pair_below_min_gap() {
        let now = Utc.with_ymd_and_hms(2026, 6, 9, 10, 0, 0).unwrap();
        let auditor = RunAuditor::with_start(now - Duration::hours(1));
        let task = make_task("interval", r#"{"minutes":5}"#);
        // Two scheduler dispatches 60 seconds apart for a 5-minute task.
        auditor.record_scheduler_dispatch(&task.id, now).await;
        auditor
            .record_scheduler_dispatch(&task.id, now + Duration::seconds(60))
            .await;
        let violations = auditor.audit_in_memory(std::slice::from_ref(&task)).await;
        assert_eq!(violations.len(), 1);
        let v = &violations[0];
        assert_eq!(v.task_id, task.id);
        assert_eq!(v.gap_seconds, 60);
        assert_eq!(v.min_gap_seconds, 300);
        assert_eq!(v.source, ViolationSource::InMemoryHistory);
    }

    #[tokio::test]
    async fn auditor_in_memory_clean_when_gap_respected() {
        let now = Utc.with_ymd_and_hms(2026, 6, 9, 10, 0, 0).unwrap();
        let auditor = RunAuditor::with_start(now - Duration::hours(1));
        let task = make_task("interval", r#"{"minutes":5}"#);
        auditor.record_scheduler_dispatch(&task.id, now).await;
        auditor
            .record_scheduler_dispatch(&task.id, now + Duration::seconds(310))
            .await;
        assert!(
            auditor
                .audit_in_memory(std::slice::from_ref(&task))
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn auditor_history_cap_drops_oldest_entries() {
        let auditor = RunAuditor::new();
        let base = Utc::now();
        for i in 0..(AUDITOR_HISTORY_CAP as i64 + 5) {
            auditor
                .record_scheduler_dispatch("t1", base + Duration::seconds(i * 1000))
                .await;
        }
        let state = auditor.state.lock().await;
        let hist = state.scheduler_history.get("t1").unwrap();
        assert_eq!(hist.len(), AUDITOR_HISTORY_CAP);
    }

    #[tokio::test]
    async fn auditor_persisted_skips_manual_sessions() {
        let db = Db::in_memory().unwrap();
        let now = Utc::now();
        let auditor = RunAuditor::with_start(now - Duration::hours(1));

        // Seed a task + two sessions 30 seconds apart. With both
        // classified scheduler, this would be a violation. Marking the
        // second one manual must suppress the alarm.
        let ts = now.to_rfc3339();
        db.create_folder(crate::db::models::NewFolder {
            id: "f1".into(),
            name: "f1".into(),
            path: "/tmp".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_repeating_task(crate::db::models::NewRepeatingTask {
            id: "t1".into(),
            name: "t1".into(),
            description: "".into(),
            folder_id: "f1".into(),
            prompt: "x".into(),
            schedule_kind: "interval".into(),
            schedule_value: r#"{"minutes":5}"#.into(),
            model: None,
            effort: None,
            enabled: true,
            next_run_at: None,
            last_run_at: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();
        for (id, offset) in [("s_a", 0), ("s_b", 30)] {
            let created = (now + Duration::seconds(offset)).to_rfc3339();
            db.create_session(NewSession {
                id: id.into(),
                name: id.into(),
                folder_id: "f1".into(),
                model: None,
                effort: None,
                is_worker: false,
                project_id: None,
                card_id: None,
                conversation_id: None,
                created_at: created.clone(),
                last_activity: created,
                repeating_task_id: Some("t1".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        }

        let tasks = db.list_repeating_tasks().await.unwrap();
        let violations = auditor.audit_persisted(&db, &tasks).await;
        assert_eq!(violations.len(), 1, "should flag without manual marker");

        auditor.mark_manual_session("s_b").await;
        let violations = auditor.audit_persisted(&db, &tasks).await;
        assert!(
            violations.is_empty(),
            "marking s_b as manual must suppress the alarm",
        );
    }

    #[tokio::test]
    async fn auditor_persisted_ignores_sessions_before_audit_start() {
        let db = Db::in_memory().unwrap();
        let now = Utc::now();
        // Audit window opens NOW; the seeded sessions are well in the
        // past and must be skipped to avoid false positives.
        let auditor = RunAuditor::with_start(now);
        let old = now - Duration::hours(2);

        let ts = now.to_rfc3339();
        db.create_folder(crate::db::models::NewFolder {
            id: "f1".into(),
            name: "f1".into(),
            path: "/tmp".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_repeating_task(crate::db::models::NewRepeatingTask {
            id: "t1".into(),
            name: "t1".into(),
            description: "".into(),
            folder_id: "f1".into(),
            prompt: "x".into(),
            schedule_kind: "interval".into(),
            schedule_value: r#"{"minutes":5}"#.into(),
            model: None,
            effort: None,
            enabled: true,
            next_run_at: None,
            last_run_at: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();
        for (id, offset) in [("s_a", 0i64), ("s_b", 30)] {
            let created = (old + Duration::seconds(offset)).to_rfc3339();
            db.create_session(NewSession {
                id: id.into(),
                name: id.into(),
                folder_id: "f1".into(),
                model: None,
                effort: None,
                is_worker: false,
                project_id: None,
                card_id: None,
                conversation_id: None,
                created_at: created.clone(),
                last_activity: created,
                repeating_task_id: Some("t1".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        let tasks = db.list_repeating_tasks().await.unwrap();
        let violations = auditor.audit_persisted(&db, &tasks).await;
        assert!(violations.is_empty());
    }

    #[tokio::test]
    async fn audit_pass_disables_task_and_broadcasts_when_violation_found() {
        let db = Db::in_memory().unwrap();
        let broadcaster = Broadcaster::new();
        let mut rx = broadcaster.subscribe_all();
        let now = Utc::now();
        let auditor = RunAuditor::with_start(now - Duration::hours(1));

        let ts = now.to_rfc3339();
        db.create_folder(crate::db::models::NewFolder {
            id: "f1".into(),
            name: "f1".into(),
            path: "/tmp".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_repeating_task(crate::db::models::NewRepeatingTask {
            id: "t1".into(),
            name: "t1".into(),
            description: "".into(),
            folder_id: "f1".into(),
            prompt: "x".into(),
            schedule_kind: "interval".into(),
            schedule_value: r#"{"minutes":5}"#.into(),
            model: None,
            effort: None,
            enabled: true,
            next_run_at: Some(ts.clone()),
            last_run_at: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();

        // Two in-memory dispatches 10 seconds apart for an interval=5min task.
        auditor.record_scheduler_dispatch("t1", now).await;
        auditor
            .record_scheduler_dispatch("t1", now + Duration::seconds(10))
            .await;

        let count = auditor.audit_pass(&db, &broadcaster).await;
        assert!(count >= 1, "expected at least one violation; got {count}");

        let after = db.get_repeating_task("t1").await.unwrap().unwrap();
        assert!(!after.enabled, "watchdog should disable the task");
        assert!(
            after.next_run_at.is_none(),
            "watchdog should clear next_run_at",
        );

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("broadcast must fire")
            .expect("rx not closed");
        assert_eq!(event.event_type, "repeating-task-watchdog");
        assert_eq!(event.data["taskId"], "t1");
        assert_eq!(event.data["action"], "disabled");
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

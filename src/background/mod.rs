//! Peckboard-managed background processes.
//!
//! An agent calls the `run_background` MCP tool to start a long-running
//! command (a build, a test suite, a dev server) that peckboard — not the
//! agent's own CLI — owns. The tool returns immediately. When the process
//! ends (success, nonzero exit, timeout, or stop) peckboard injects a report
//! into the originating session as a durable `user` event tagged
//! `{"source": "background-task", "background_task": {..}}` and wakes the
//! session: a fresh turn when idle, the durable queue when mid-turn. See
//! [`crate::service::session_notify`].
//!
//! Registry state is in-memory only: a server restart kills every task
//! ([`BackgroundRegistry::shutdown_all`]) and forgets it. Each task runs in
//! its own process group so stop/timeout/shutdown take the whole tree down.
//! stdout+stderr are merged line-wise, secret-masked, and written to
//! `<data_dir>/bg/<id>.log` (capped at [`LOG_CAP_BYTES`]) plus an in-memory
//! ring of the last [`TAIL_LINES`] lines.

mod dispatcher;
pub mod report;
#[cfg(test)]
mod tests;

pub use dispatcher::WeakAppDispatcher;

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::Notify;

use crate::db::Db;
use crate::plugin::host::PreparedExec;
use crate::service::mcp_server::ExpertDispatcher;
use crate::service::secret_mask::SecretMasker;
use crate::ws::broadcaster::{Broadcaster, WsEvent};

/// Log directory name under the data dir.
pub const LOG_DIR: &str = "bg";
/// In-memory output ring size, in lines.
pub const TAIL_LINES: usize = 200;
/// Output lines quoted in the completion report.
pub const REPORT_LINES: usize = 40;
/// Per-task log file cap; past it the file stops growing (the ring keeps
/// going) and the task is flagged `log_truncated`.
pub const LOG_CAP_BYTES: u64 = 50 * 1024 * 1024;
/// Upper (and default) bound on `timeout_secs`.
pub const MAX_TIMEOUT_SECS: u64 = 24 * 60 * 60;
/// SIGTERM → SIGKILL grace on stop / timeout.
pub const STOP_GRACE: Duration = Duration::from_secs(5);
/// Concurrently running tasks one session may own.
pub const MAX_RUNNING_PER_SESSION: usize = 16;
/// Largest `lines` a tail request may ask for.
pub const MAX_TAIL_REQUEST: usize = 2000;
/// WebSocket event type broadcast on task start / finish.
pub const WS_EVENT: &str = "background_task";
/// `source` marker on the injected report event.
pub const EVENT_SOURCE: &str = "background-task";

/// A single output line longer than this is split.
const MAX_LINE_BYTES: usize = 16 * 1024;
/// How far back from EOF a large tail request reads.
const MAX_LOG_TAIL_READ: u64 = 8 * 1024 * 1024;
/// Finished tasks are forgotten (and their logs deleted) after this long.
const FINISHED_RETENTION_SECS: i64 = 24 * 60 * 60;
/// Output lines an attached caller gets back inline.
const ATTACHED_RESULT_LINES: usize = 400;
/// After the process exits, how long to wait for its pipes to drain (a
/// daemonized grandchild may hold them open forever).
const DRAIN_WAIT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    Succeeded,
    Failed,
    TimedOut,
    Stopped,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Stopped => "stopped",
        }
    }
}

/// The task JSON returned by the MCP tools, the REST routes, and carried by
/// the `background_task` WS event.
#[derive(Debug, Clone, Serialize)]
pub struct TaskInfo {
    pub id: String,
    pub session_id: String,
    pub label: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    /// RFC 3339.
    pub started_at: String,
    /// RFC 3339; `None` while running.
    pub finished_at: Option<String>,
    pub status: TaskStatus,
    pub exit_code: Option<i32>,
    /// Terminating signal (unix) when the process died from one.
    pub signal: Option<i32>,
    pub pid: Option<u32>,
    pub log_path: String,
    pub log_truncated: bool,
    pub timeout_secs: u64,
    /// A stop was requested and the process hasn't exited yet.
    pub stopping: bool,
}

/// Outcome of [`BackgroundRegistry::spawn_attached`].
pub enum Attached {
    /// Finished inside the window: final snapshot + last output lines. The
    /// task is already forgotten (no listing, no report).
    Finished(TaskInfo, Vec<String>),
    /// Still running when the window closed: now an ordinary background task
    /// (listed, reported on exit). Carries the output so far.
    Detached(TaskInfo, Vec<String>),
}
/// The inline result handed to an attached (`run_command`) caller: the final
/// task snapshot plus its last output lines.
type AttachedResult = (TaskInfo, Vec<String>);

struct Task {
    info: Mutex<TaskInfo>,
    tail: Mutex<VecDeque<String>>,
    started: Instant,
    stop: Notify,
    stop_requested: AtomicBool,
    /// Suppress the completion report (session deleted / server shutdown).
    silent: AtomicBool,
    /// Set while a `run_command` caller waits inline. Whichever side takes
    /// it first wins: `supervise` (finished inside the window → result goes
    /// inline, no report) or the caller (window over → handed off, reported
    /// like any background task).
    attached: Mutex<Option<tokio::sync::oneshot::Sender<AttachedResult>>>,
    /// Not yet shown as a background task (attached, still inside the
    /// window): kept out of listings.
    hidden: AtomicBool,
    /// Don't kill the process group when the leader exits on its own —
    /// `run_command`'s `x &` detaches on purpose.
    keep_group_on_exit: bool,
}

impl Task {
    fn info(&self) -> TaskInfo {
        lock(&self.info).clone()
    }

    fn push_line(&self, line: String) {
        let mut t = lock(&self.tail);
        if t.len() == TAIL_LINES {
            t.pop_front();
        }
        t.push_back(line);
    }

    fn last_lines(&self, n: usize) -> Vec<String> {
        let t = lock(&self.tail);
        let skip = t.len().saturating_sub(n);
        t.iter().skip(skip).cloned().collect()
    }

    fn is_running(&self) -> bool {
        lock(&self.info).status == TaskStatus::Running
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Where completion reports go: bound once at boot, after `AppState`
/// exists (the dispatcher needs it).
struct Reporter {
    db: Db,
    broadcaster: Arc<Broadcaster>,
    dispatcher: Option<Arc<dyn ExpertDispatcher>>,
}

/// Registry of every background task this server process started.
///
/// Constructed unbound ([`Self::new`] / `Default`) so it can sit in
/// `AppState` before the state exists; [`Self::bind`] wires the DB,
/// broadcaster, and dispatcher. An unbound registry refuses to spawn, so a
/// task can never run without a place to report to.
pub struct BackgroundRegistry {
    log_dir: PathBuf,
    tasks: Mutex<HashMap<String, Arc<Task>>>,
    /// Sessions whose tasks were killed by [`Self::kill_session`] (deleted).
    /// A `run_background` call already in flight when the session was
    /// deleted is refused at insert time instead of leaking an unowned
    /// process. Lock order: `tasks` before `deleted_sessions`.
    deleted_sessions: Mutex<HashSet<String>>,
    reporter: OnceLock<Reporter>,
}

impl Default for BackgroundRegistry {
    fn default() -> Self {
        Self::new(std::env::temp_dir().join("peckboard-bg"))
    }
}

static GLOBAL: OnceLock<Arc<BackgroundRegistry>> = OnceLock::new();

/// Register the boot registry for call paths that have no `AppState` (the
/// in-process plugin-provider tool bridge).
pub fn set_global(registry: Arc<BackgroundRegistry>) {
    let _ = GLOBAL.set(registry);
}

pub fn global() -> Option<Arc<BackgroundRegistry>> {
    GLOBAL.get().cloned()
}

/// Remove logs left by a previous server process (unreachable: the
/// registry that knew them is gone).
pub fn clear_stale_logs(log_dir: &Path) {
    if log_dir.exists()
        && let Err(e) = std::fs::remove_dir_all(log_dir)
    {
        tracing::warn!(dir = %log_dir.display(), "failed to clear stale background logs: {e}");
    }
}

impl BackgroundRegistry {
    /// Unbound registry writing logs under `log_dir`.
    pub fn new(log_dir: PathBuf) -> Self {
        Self {
            log_dir,
            tasks: Mutex::new(HashMap::new()),
            deleted_sessions: Mutex::new(HashSet::new()),
            reporter: OnceLock::new(),
        }
    }

    /// Whether [`Self::bind`] ran (an unbound registry refuses to spawn).
    pub fn is_bound(&self) -> bool {
        self.reporter.get().is_some()
    }

    /// Bind where reports go. `dispatcher` wakes the session; without one
    /// the report is still persisted + broadcast, just not driven. First
    /// bind wins.
    pub fn bind(
        &self,
        db: Db,
        broadcaster: Arc<Broadcaster>,
        dispatcher: Option<Arc<dyn ExpertDispatcher>>,
    ) {
        let _ = self.reporter.set(Reporter {
            db,
            broadcaster,
            dispatcher,
        });
    }

    fn task(&self, id: &str) -> Option<Arc<Task>> {
        lock(&self.tasks).get(id).cloned()
    }

    /// Snapshot of one task.
    pub fn get(&self, id: &str) -> Option<TaskInfo> {
        self.task(id).map(|t| t.info())
    }

    /// Snapshot of one task, only if it belongs to `session_id`.
    pub fn get_for_session(&self, id: &str, session_id: &str) -> Option<TaskInfo> {
        self.get(id).filter(|t| t.session_id == session_id)
    }

    /// Every task (running or finished) owned by `session_id`, oldest first.
    pub fn list_for_session(&self, session_id: &str) -> Vec<TaskInfo> {
        self.prune_finished();
        let mut out: Vec<(Instant, TaskInfo)> = lock(&self.tasks)
            .values()
            .filter(|t| !t.hidden.load(Ordering::SeqCst))
            .map(|t| (t.started, t.info()))
            .filter(|(_, i)| i.session_id == session_id)
            .collect();
        out.sort_by_key(|(s, _)| *s);
        out.into_iter().map(|(_, i)| i).collect()
    }

    /// Whether `session_id` owns any still-running task. A subagent with one
    /// isn't done yet: the task's exit report resumes it (see
    /// `crate::subagent::handle_subagent_done`).
    pub fn has_running_for_session(&self, session_id: &str) -> bool {
        lock(&self.tasks).values().any(|t| {
            let i = lock(&t.info);
            i.session_id == session_id && i.status == TaskStatus::Running
        })
    }

    /// Stop every running task of `session_id` with no completion report,
    /// so the exit can't resume the session. Unlike [`Self::kill_session`]
    /// the tasks stay listed and later spawns are still allowed. Used when
    /// a subagent crashed and its result was already reported.
    pub fn stop_session_silently(&self, session_id: &str) {
        let running: Vec<Arc<Task>> = lock(&self.tasks)
            .values()
            .filter(|t| {
                let i = lock(&t.info);
                i.session_id == session_id && i.status == TaskStatus::Running
            })
            .cloned()
            .collect();
        for task in running {
            task.silent.store(true, Ordering::SeqCst);
            task.stop_requested.store(true, Ordering::SeqCst);
            task.stop.notify_one();
            let info = {
                let mut i = lock(&task.info);
                i.stopping = true;
                i.clone()
            };
            self.broadcast("updated", &info);
        }
    }

    /// The last `lines` (secret-masked) output lines. Served from the ring
    /// when it can; larger requests read the log file's tail.
    pub fn tail(&self, id: &str, lines: usize) -> Option<Vec<String>> {
        let task = self.task(id)?;
        let lines = lines.clamp(1, MAX_TAIL_REQUEST);
        let info = task.info();
        if lines <= TAIL_LINES || info.log_truncated {
            return Some(task.last_lines(lines));
        }
        Some(
            read_log_tail(Path::new(&info.log_path), lines)
                .unwrap_or_else(|_| task.last_lines(lines)),
        )
    }

    /// Start `prepared` with `args` as a background task owned by
    /// `session_id`. Returns once the process is spawned.
    pub(crate) async fn spawn(
        self: &Arc<Self>,
        session_id: &str,
        prepared: PreparedExec,
        args: Vec<String>,
        label: Option<String>,
        timeout_secs: Option<u64>,
    ) -> Result<TaskInfo, String> {
        self.spawn_inner(session_id, prepared, args, label, timeout_secs, None)
            .await
            .map(|t| t.info())
    }

    /// `run_command`'s long path: start the command as a hidden task and
    /// wait up to `window` for it. Finished in time → the result comes back
    /// inline and the task leaves no trace. Otherwise it becomes an ordinary
    /// background task (listed, reported on exit) and the caller gets its
    /// handle — so a command outliving the agent's tool-call transport limit
    /// keeps running instead of being orphaned behind a client timeout.
    pub(crate) async fn spawn_attached(
        self: &Arc<Self>,
        session_id: &str,
        prepared: PreparedExec,
        args: Vec<String>,
        timeout_secs: u64,
        window: Duration,
    ) -> Result<Attached, String> {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let task = self
            .spawn_inner(
                session_id,
                prepared,
                args,
                None,
                Some(timeout_secs),
                Some(tx),
            )
            .await?;
        if let Ok(Ok((info, lines))) = tokio::time::timeout(window, &mut rx).await {
            return Ok(Attached::Finished(info, lines));
        }
        {
            let mut slot = lock(&task.attached);
            if slot.take().is_some() {
                // Handed off. Announce under the slot lock so this "started"
                // can't land after `supervise`'s "finished".
                task.hidden.store(false, Ordering::SeqCst);
                let info = task.info();
                self.broadcast("started", &info);
                drop(slot);
                return Ok(Attached::Detached(info, task.last_lines(REPORT_LINES)));
            }
        }
        // `supervise` took the sender first: its result is on the way.
        rx.await
            .map(|(info, lines)| Attached::Finished(info, lines))
            .map_err(|_| "the command ended without reporting a result".to_string())
    }

    async fn spawn_inner(
        self: &Arc<Self>,
        session_id: &str,
        prepared: PreparedExec,
        args: Vec<String>,
        label: Option<String>,
        timeout_secs: Option<u64>,
        attached: Option<tokio::sync::oneshot::Sender<AttachedResult>>,
    ) -> Result<Arc<Task>, String> {
        if self.reporter.get().is_none() {
            return Err("background tasks are not configured on this server".into());
        }
        self.prune_finished();
        // Cheap early refusal before forking; repeated authoritatively under
        // the same lock acquisition as the insert below.
        self.admit(&lock(&self.tasks), session_id)?;

        let (program, cwd, env, masker) = prepared.into_parts();
        std::fs::create_dir_all(&self.log_dir)
            .map_err(|e| format!("failed to create the background log dir: {e}"))?;
        let id = uuid::Uuid::new_v4().to_string();
        let log_path = self.log_dir.join(format!("{id}.log"));
        let file = std::fs::File::create(&log_path)
            .map_err(|e| format!("failed to create the task log: {e}"))?;

        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(&args)
            .envs(env)
            .current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // Own process group: stop / timeout / shutdown signal the whole tree.
        #[cfg(unix)]
        cmd.process_group(0);
        crate::provider::turn::reset_child_signals(&mut cmd);
        set_parent_death_signal(&mut cmd);
        let mut child = match spawn_child(cmd).await {
            Ok(c) => c,
            Err(e) => {
                let _ = std::fs::remove_file(&log_path);
                return Err(format!("failed to start '{program}': {e}"));
            }
        };

        let timeout_secs = timeout_secs
            .unwrap_or(MAX_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS);
        let label = label
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| {
                let d = report::display_command(&program, &args);
                if d.chars().count() > 60 {
                    format!("{}\u{2026}", d.chars().take(60).collect::<String>())
                } else {
                    d
                }
            });
        let info = TaskInfo {
            id: id.clone(),
            session_id: session_id.to_string(),
            label,
            program,
            args,
            cwd: cwd.to_string_lossy().to_string(),
            started_at: chrono::Utc::now().to_rfc3339(),
            finished_at: None,
            status: TaskStatus::Running,
            exit_code: None,
            signal: None,
            pid: child.id(),
            log_path: log_path.to_string_lossy().to_string(),
            log_truncated: false,
            timeout_secs,
            stopping: false,
        };
        let hidden = attached.is_some();
        let task = Arc::new(Task {
            info: Mutex::new(info.clone()),
            tail: Mutex::new(VecDeque::with_capacity(TAIL_LINES)),
            started: Instant::now(),
            stop: Notify::new(),
            stop_requested: AtomicBool::new(false),
            silent: AtomicBool::new(false),
            keep_group_on_exit: hidden,
            attached: Mutex::new(attached),
            hidden: AtomicBool::new(hidden),
        });
        {
            let mut tasks = lock(&self.tasks);
            // Check + insert under one lock: concurrent spawns can't both
            // pass the cap, and a spawn racing `kill_session` can't register
            // a task for a session that was just deleted.
            if let Err(e) = self.admit(&tasks, session_id) {
                drop(tasks);
                signal_group(info.pid, Signal::Kill);
                let _ = child.start_kill();
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
                let _ = std::fs::remove_file(&log_path);
                return Err(e);
            }
            tasks.insert(id, task.clone());
        }
        if !hidden {
            self.broadcast("started", &info);
        }
        tracing::info!(
            session_id = %info.session_id,
            task_id = %info.id,
            pid = ?info.pid,
            "background task started: {}",
            report::display_command(&info.program, &info.args)
        );

        let sink = Arc::new(Mutex::new(LogSink {
            file: Some(BufWriter::new(file)),
            written: 0,
        }));
        let masker = Arc::new(masker);
        let pumps: Vec<tokio::task::JoinHandle<()>> = [
            child
                .stdout
                .take()
                .map(|s| spawn_pump(s, task.clone(), sink.clone(), masker.clone())),
            child
                .stderr
                .take()
                .map(|s| spawn_pump(s, task.clone(), sink.clone(), masker.clone())),
        ]
        .into_iter()
        .flatten()
        .collect();

        let registry = self.clone();
        let handle = task.clone();
        tokio::spawn(async move {
            registry
                .supervise(task, child, pumps, sink, Duration::from_secs(timeout_secs))
                .await;
        });
        Ok(handle)
    }

    /// Ask a running task to stop: SIGTERM to its process group, SIGKILL
    /// after [`STOP_GRACE`]. The completion report still fires (STOPPED).
    pub fn stop(&self, id: &str) -> Result<TaskInfo, String> {
        let task = self.task(id).ok_or("background task not found")?;
        if !task.is_running() {
            return Err(format!(
                "background task {id} already finished ({})",
                task.info().status.as_str()
            ));
        }
        task.stop_requested.store(true, Ordering::SeqCst);
        task.stop.notify_one();
        let info = {
            let mut i = lock(&task.info);
            i.stopping = true;
            i.clone()
        };
        self.broadcast("updated", &info);
        Ok(info)
    }

    /// Kill (SIGKILL, no report) and forget every task of a deleted session,
    /// and refuse any later spawn for it (an in-flight `run_background`).
    /// Only for session deletion: the tombstone is permanent.
    pub fn kill_session(&self, session_id: &str) {
        let removed: Vec<Arc<Task>> = {
            let mut tasks = lock(&self.tasks);
            lock(&self.deleted_sessions).insert(session_id.to_string());
            let ids: Vec<String> = tasks
                .iter()
                .filter(|(_, t)| lock(&t.info).session_id == session_id)
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter().filter_map(|id| tasks.remove(id)).collect()
        };
        for task in removed {
            task.silent.store(true, Ordering::SeqCst);
            task.stop_requested.store(true, Ordering::SeqCst);
            let info = task.info();
            if info.status == TaskStatus::Running {
                signal_group(info.pid, Signal::Kill);
                task.stop.notify_one();
            } else {
                let _ = std::fs::remove_file(&info.log_path);
            }
        }
    }

    /// Server shutdown: SIGTERM every running task's group, give them a
    /// moment, SIGKILL whatever is left. No reports (the server is going
    /// down; the sessions' agents are being stopped too).
    pub async fn shutdown_all(&self) {
        let running: Vec<Arc<Task>> = lock(&self.tasks)
            .values()
            .filter(|t| t.is_running())
            .cloned()
            .collect();
        if running.is_empty() {
            return;
        }
        tracing::info!(
            count = running.len(),
            "stopping background tasks for shutdown"
        );
        for t in &running {
            t.silent.store(true, Ordering::SeqCst);
            t.stop_requested.store(true, Ordering::SeqCst);
            signal_group(t.info().pid, Signal::Term);
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && running.iter().any(|t| t.is_running()) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // Tasks whose leader already exited are skipped on purpose: their
        // supervisor SIGKILLed the whole group the moment it reaped the
        // leader (see `supervise`), and once reaped the pgid number is free
        // for reuse, so signalling it again could hit an unrelated group.
        // An unreaped leader (zombie) still reads as running and is killed.
        for t in running.iter().filter(|t| t.is_running()) {
            signal_group(t.info().pid, Signal::Kill);
        }
    }

    /// May `session_id` start another task? Takes the caller's `tasks` guard
    /// so the check and the following insert share one lock acquisition.
    fn admit(&self, tasks: &HashMap<String, Arc<Task>>, session_id: &str) -> Result<(), String> {
        if lock(&self.deleted_sessions).contains(session_id) {
            return Err("session was deleted".into());
        }
        let running = tasks
            .values()
            .filter(|t| {
                let i = lock(&t.info);
                i.status == TaskStatus::Running && i.session_id == session_id
            })
            .count();
        if running >= MAX_RUNNING_PER_SESSION {
            return Err(format!(
                "this session already has {running} background tasks running (max \
                 {MAX_RUNNING_PER_SESSION}); stop one with stop_background first"
            ));
        }
        Ok(())
    }

    /// Forget finished tasks older than the retention window.
    fn prune_finished(&self) {
        let cutoff = chrono::Utc::now() - chrono::Duration::seconds(FINISHED_RETENTION_SECS);
        let mut tasks = lock(&self.tasks);
        tasks.retain(|_, t| {
            let i = lock(&t.info);
            let expired = i
                .finished_at
                .as_deref()
                .and_then(|f| chrono::DateTime::parse_from_rfc3339(f).ok())
                .is_some_and(|f| f < cutoff);
            if expired {
                let _ = std::fs::remove_file(&i.log_path);
            }
            !expired
        });
    }

    fn broadcast(&self, action: &str, info: &TaskInfo) {
        let Some(reporter) = self.reporter.get() else {
            return;
        };
        reporter.broadcaster.broadcast(WsEvent {
            event_type: WS_EVENT.into(),
            session_id: info.session_id.clone(),
            data: serde_json::json!({ "action": action, "task": info }),
        });
    }

    /// Own the child until it ends, then record the outcome and report it.
    async fn supervise(
        self: Arc<Self>,
        task: Arc<Task>,
        mut child: tokio::process::Child,
        pumps: Vec<tokio::task::JoinHandle<()>>,
        sink: Arc<Mutex<LogSink>>,
        timeout: Duration,
    ) {
        enum End {
            Exited(Option<std::process::ExitStatus>),
            TimedOut,
            Stopped,
        }
        let pid = task.info().pid;
        let end = tokio::select! {
            s = child.wait() => End::Exited(s.ok()),
            _ = tokio::time::sleep(timeout) => End::TimedOut,
            _ = task.stop.notified() => End::Stopped,
        };
        let (exit, timed_out, stopped) = match end {
            End::Exited(s) => {
                // Leader gone on its own: take down anything it left in the
                // group (`sh -c "x &"`, `npm run dev`'s children) so nothing
                // outlives the task untracked. Safe right after the reap: the
                // pgid can't be reused while members remain. `run_command`
                // tasks keep them: detaching `x &` is the caller's intent.
                if !task.keep_group_on_exit {
                    signal_group(pid, Signal::Kill);
                }
                (s, false, false)
            }
            End::TimedOut => (terminate(&mut child, pid).await, true, false),
            End::Stopped => (terminate(&mut child, pid).await, false, true),
        };

        // Let the pumps flush what's left in the pipes.
        for h in pumps {
            let abort = h.abort_handle();
            if tokio::time::timeout(DRAIN_WAIT, h).await.is_err() {
                abort.abort();
            }
        }
        let log_truncated = {
            let mut s = lock(&sink);
            s.close();
            s.file.is_none() && s.written >= LOG_CAP_BYTES
        };

        let stop_requested = task.stop_requested.load(Ordering::SeqCst);
        let code = exit.and_then(|s| s.code());
        #[cfg(unix)]
        let signal = exit.and_then(|s| std::os::unix::process::ExitStatusExt::signal(&s));
        #[cfg(not(unix))]
        let signal: Option<i32> = None;
        // A requested stop wins over the exit code: a process that exits 0
        // on SIGTERM was still stopped, not successful.
        let status = if timed_out {
            TaskStatus::TimedOut
        } else if stopped {
            TaskStatus::Stopped
        } else if exit.is_some_and(|s| s.success()) {
            TaskStatus::Succeeded
        } else if stop_requested {
            TaskStatus::Stopped
        } else {
            TaskStatus::Failed
        };

        let info = {
            let mut i = lock(&task.info);
            i.status = status;
            i.exit_code = code;
            i.signal = signal;
            i.finished_at = Some(chrono::Utc::now().to_rfc3339());
            i.log_truncated = i.log_truncated || log_truncated;
            i.stopping = false;
            i.clone()
        };
        let elapsed = task.started.elapsed().as_secs();
        tracing::info!(
            session_id = %info.session_id,
            task_id = %info.id,
            status = info.status.as_str(),
            exit_code = ?info.exit_code,
            "background task finished after {elapsed}s"
        );
        // A `run_command` caller still inside its sync window takes the
        // result inline: no UI event, no report, nothing left behind.
        if let Some(tx) = lock(&task.attached).take() {
            let lines = if info.log_truncated {
                task.last_lines(TAIL_LINES)
            } else {
                read_log_tail(Path::new(&info.log_path), ATTACHED_RESULT_LINES)
                    .unwrap_or_else(|_| task.last_lines(TAIL_LINES))
            };
            if tx.send((info.clone(), lines)).is_ok() {
                lock(&self.tasks).remove(&info.id);
                let _ = std::fs::remove_file(&info.log_path);
                return;
            }
        }

        if task.silent.load(Ordering::SeqCst) {
            if self.task(&info.id).is_none() {
                // Session deleted: the task was already dropped from the map.
                let _ = std::fs::remove_file(&info.log_path);
            } else {
                // Still listed (silent stop / shutdown): update the UI, no report.
                self.broadcast("finished", &info);
            }
            return;
        }
        self.broadcast("finished", &info);

        let Some(reporter) = self.reporter.get() else {
            return;
        };
        // The session may have been deleted while the task ran.
        if !matches!(reporter.db.get_session(&info.session_id).await, Ok(Some(_))) {
            return;
        }
        let text = report::compose(&info, elapsed, &task.last_lines(REPORT_LINES));
        if let Err(e) = crate::service::session_notify::notify_session(
            &reporter.db,
            &reporter.broadcaster,
            reporter.dispatcher.as_deref(),
            &info.session_id,
            &text,
            serde_json::json!({
                "source": EVENT_SOURCE,
                "background_task": {
                    "id": info.id,
                    "label": info.label,
                    "status": info.status,
                    "exit_code": info.exit_code,
                },
            }),
        )
        .await
        {
            tracing::warn!(
                session_id = %info.session_id,
                task_id = %info.id,
                "background task report append failed: {e}"
            );
        }
    }
}

/// Log file writer with a byte cap.
struct LogSink {
    file: Option<BufWriter<std::fs::File>>,
    written: u64,
}

impl LogSink {
    /// Append one line; returns true when this line hit the cap.
    fn write_line(&mut self, line: &str) -> bool {
        let Some(f) = self.file.as_mut() else {
            return false;
        };
        let bytes = line.len() as u64 + 1;
        if self.written + bytes > LOG_CAP_BYTES {
            let _ = writeln!(
                f,
                "[peckboard: log truncated at {} MB; later output is not saved]",
                LOG_CAP_BYTES / (1024 * 1024)
            );
            let _ = f.flush();
            self.file = None;
            self.written = LOG_CAP_BYTES;
            return true;
        }
        if writeln!(f, "{line}").is_err() {
            self.file = None;
        }
        self.written += bytes;
        false
    }

    fn flush(&mut self) {
        if let Some(f) = self.file.as_mut() {
            let _ = f.flush();
        }
    }

    fn close(&mut self) {
        self.flush();
    }
}

fn spawn_pump<R: AsyncRead + Unpin + Send + 'static>(
    r: R,
    task: Arc<Task>,
    sink: Arc<Mutex<LogSink>>,
    masker: Arc<SecretMasker>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(pump(r, task, sink, masker))
}

/// Read one pipe to EOF, splitting into lines (masked, logged, ringed).
async fn pump<R: AsyncRead + Unpin>(
    mut r: R,
    task: Arc<Task>,
    sink: Arc<Mutex<LogSink>>,
    masker: Arc<SecretMasker>,
) {
    let mut buf = vec![0u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let n = match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        pending.extend_from_slice(&buf[..n]);
        while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = pending.drain(..=pos).collect();
            emit_line(&task, &sink, &masker, &line[..line.len() - 1]);
        }
        if pending.len() > MAX_LINE_BYTES {
            let line = std::mem::take(&mut pending);
            emit_line(&task, &sink, &masker, &line);
        }
        lock(&sink).flush();
    }
    if !pending.is_empty() {
        emit_line(&task, &sink, &masker, &pending);
        lock(&sink).flush();
    }
}

fn emit_line(task: &Task, sink: &Mutex<LogSink>, masker: &SecretMasker, raw: &[u8]) {
    let text = String::from_utf8_lossy(raw);
    let text = text.strip_suffix('\r').unwrap_or(&text);
    let masked = masker.mask(text).into_owned();
    if lock(sink).write_line(&masked) {
        lock(&task.info).log_truncated = true;
    }
    task.push_line(masked);
}

/// Last `lines` lines of a log file, reading at most [`MAX_LOG_TAIL_READ`]
/// bytes back from EOF.
fn read_log_tail(path: &Path, lines: usize) -> std::io::Result<Vec<String>> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(MAX_LOG_TAIL_READ);
    f.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut all: Vec<&str> = text.lines().collect();
    if start > 0 && !all.is_empty() {
        all.remove(0); // partial first line
    }
    let skip = all.len().saturating_sub(lines);
    Ok(all[skip..].iter().map(|s| s.to_string()).collect())
}

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

/// Signal the task's whole process group (unix). Elsewhere a no-op; the
/// callers fall back to killing the direct child.
fn signal_group(pid: Option<u32>, sig: Signal) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        let sig = match sig {
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        };
        // SAFETY: plain syscall; negative pid addresses the process group
        // created by `process_group(0)` at spawn.
        unsafe {
            libc::kill(-(pid as i32), sig);
        }
    }
    #[cfg(not(unix))]
    let _ = (pid, sig);
}

/// Stop the task: SIGTERM the group, SIGKILL after [`STOP_GRACE`].
async fn terminate(
    child: &mut tokio::process::Child,
    pid: Option<u32>,
) -> Option<std::process::ExitStatus> {
    signal_group(pid, Signal::Term);
    #[cfg(not(unix))]
    let _ = child.start_kill();
    match tokio::time::timeout(STOP_GRACE, child.wait()).await {
        Ok(Ok(s)) => {
            // Leader gone; take down anything it left in the group.
            signal_group(pid, Signal::Kill);
            Some(s)
        }
        _ => {
            signal_group(pid, Signal::Kill);
            let _ = child.start_kill();
            child.wait().await.ok()
        }
    }
}

/// Linux: SIGKILL the task's leader if the server dies (crash, SIGKILL,
/// OOM) so it isn't left running with nobody to report to. The signal fires
/// when the *thread* that forked the child exits, hence [`spawn_child`]'s
/// dedicated thread. It is not inherited across fork, so it only covers the
/// leader; grandchildren are handled by the group kills on normal paths and
/// are the one leak left on a hard server crash.
#[cfg(target_os = "linux")]
fn set_parent_death_signal(cmd: &mut tokio::process::Command) {
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: runs in the forked child before exec; prctl/getppid are
    // async-signal-safe and nothing else executes in that window.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // The server died between fork and prctl: we were reparented
            // and the death signal will never come.
            if libc::getppid() != parent {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn set_parent_death_signal(_cmd: &mut tokio::process::Command) {}

/// Spawn `cmd`. On Linux the fork happens on one dedicated thread that
/// lives as long as the process: `PR_SET_PDEATHSIG` fires when the forking
/// *thread* exits, and `spawn` is reachable from `spawn_blocking` / plugin
/// threads (the in-process provider tool bridge `block_on`s from one) that
/// the pool retires after idling, which would SIGKILL a healthy task.
#[cfg(target_os = "linux")]
async fn spawn_child(cmd: tokio::process::Command) -> std::io::Result<tokio::process::Child> {
    type Reply = tokio::sync::oneshot::Sender<std::io::Result<tokio::process::Child>>;
    type Job = (tokio::process::Command, tokio::runtime::Handle, Reply);
    static SPAWNER: OnceLock<std::sync::mpsc::Sender<Job>> = OnceLock::new();
    let tx = SPAWNER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("peckboard-bg-spawner".into())
            .spawn(move || {
                for (mut cmd, handle, reply) in rx {
                    // Register the child with the caller's runtime.
                    let _guard = handle.enter();
                    let _ = reply.send(cmd.spawn());
                }
            })
            .expect("failed to start the background spawner thread");
        tx
    });
    let (reply, rx) = tokio::sync::oneshot::channel();
    tx.send((cmd, tokio::runtime::Handle::current(), reply))
        .map_err(|_| std::io::Error::other("background spawner thread is gone"))?;
    rx.await
        .map_err(|_| std::io::Error::other("background spawner thread dropped the request"))?
}

#[cfg(not(target_os = "linux"))]
async fn spawn_child(mut cmd: tokio::process::Command) -> std::io::Result<tokio::process::Child> {
    cmd.spawn()
}

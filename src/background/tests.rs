use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;
use crate::db::models::{NewFolder, NewSession};
use crate::plugin::host::{InvocationContext, prepare_exec};

/// Records every resume so a test can assert the session was woken.
#[derive(Default)]
struct RecordingDispatcher {
    resumed: Mutex<Vec<(String, String)>>,
}

impl ExpertDispatcher for RecordingDispatcher {
    fn dispatch_capture<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn resume_session<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async { panic!("background reports must use resume_session_appended") })
    }

    fn resume_session_appended<'a>(
        &'a self,
        session_id: &'a str,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        self.resumed
            .lock()
            .unwrap()
            .push((session_id.to_string(), text.to_string()));
        Box::pin(async { Ok(()) })
    }
}

struct Fixture {
    db: Db,
    registry: Arc<BackgroundRegistry>,
    dispatcher: Arc<RecordingDispatcher>,
    _dir: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let db = Db::in_memory().unwrap();
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: "f-1".into(),
        name: "f-1".into(),
        path: project.to_string_lossy().to_string(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_session(NewSession {
        id: "s-1".into(),
        name: "s-1".into(),
        folder_id: "f-1".into(),
        created_at: ts.clone(),
        last_activity: ts,
        ..Default::default()
    })
    .await
    .unwrap();
    let registry = Arc::new(BackgroundRegistry::new(dir.path().join(LOG_DIR)));
    let dispatcher = Arc::new(RecordingDispatcher::default());
    registry.bind(
        db.clone(),
        crate::ws::broadcaster::Broadcaster::new(),
        Some(dispatcher.clone()),
    );
    Fixture {
        db,
        registry,
        dispatcher,
        _dir: dir,
    }
}

async fn start(f: &Fixture, program: &str, args: &[&str]) -> TaskInfo {
    try_start(f, program, args).await.unwrap()
}

async fn try_start(f: &Fixture, program: &str, args: &[&str]) -> Result<TaskInfo, String> {
    let db = f.db.clone();
    let program = program.to_string();
    let prepared = tokio::task::spawn_blocking(move || {
        let inv = InvocationContext {
            session_id: Some("s-1".into()),
            folder_id: Some("f-1".into()),
            ..Default::default()
        };
        prepare_exec(&db, &program, &inv, false, None)
    })
    .await
    .unwrap()
    .unwrap();
    f.registry
        .spawn(
            "s-1",
            prepared,
            args.iter().map(|s| s.to_string()).collect(),
            Some("job".into()),
            None,
        )
        .await
}

/// Wait for the report to land: the persisted user event AND the wake-up.
async fn wait_for_report(f: &Fixture) -> serde_json::Value {
    for _ in 0..200 {
        let events = f.db.events_tail("s-1", 20).await.unwrap();
        if let Some(ev) = events.iter().find(|e| e.kind == "user")
            && !f.dispatcher.resumed.lock().unwrap().is_empty()
        {
            return serde_json::from_str(&ev.data).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no background report was delivered");
}

#[tokio::test]
async fn success_reports_exit_zero_with_output() {
    let f = fixture().await;
    let info = start(&f, "sh", &["-c", "echo hello-from-bg"]).await;
    assert_eq!(info.status, TaskStatus::Running);
    assert!(info.log_path.ends_with(&format!("{}.log", info.id)));

    let data = wait_for_report(&f).await;
    let text = data["text"].as_str().unwrap();
    assert!(
        text.starts_with(&format!(
            "[background task \"job\" ({}) finished: exit 0",
            info.id
        )),
        "{text}"
    );
    assert!(text.contains("hello-from-bg"), "{text}");
    assert_eq!(data["source"], EVENT_SOURCE);
    assert_eq!(data["background_task"]["id"], info.id.as_str());
    assert_eq!(data["background_task"]["status"], "succeeded");

    let done = f.registry.get(&info.id).unwrap();
    assert_eq!(done.status, TaskStatus::Succeeded);
    assert_eq!(done.exit_code, Some(0));
    assert!(done.finished_at.is_some());
    let log = std::fs::read_to_string(&done.log_path).unwrap();
    assert!(log.contains("hello-from-bg"));
    let (sid, woke) = f.dispatcher.resumed.lock().unwrap()[0].clone();
    assert_eq!(sid, "s-1");
    assert_eq!(woke, text);
}

#[tokio::test]
async fn nonzero_exit_reports_failed() {
    let f = fixture().await;
    let info = start(&f, "sh", &["-c", "echo boom >&2; exit 3"]).await;

    let data = wait_for_report(&f).await;
    let text = data["text"].as_str().unwrap();
    assert!(text.contains("FAILED: exit 3"), "{text}");
    assert!(text.contains("boom"), "stderr is captured too: {text}");
    assert_eq!(data["background_task"]["status"], "failed");
    assert_eq!(data["background_task"]["exit_code"], 3);
    assert_eq!(f.registry.get(&info.id).unwrap().status, TaskStatus::Failed);
}
async fn start_attached(f: &Fixture, script: &str, window: Duration) -> Attached {
    let db = f.db.clone();
    let prepared = tokio::task::spawn_blocking(move || {
        let inv = InvocationContext {
            session_id: Some("s-1".into()),
            folder_id: Some("f-1".into()),
            ..Default::default()
        };
        prepare_exec(&db, "sh", &inv, false, None)
    })
    .await
    .unwrap()
    .unwrap();
    f.registry
        .spawn_attached(
            "s-1",
            prepared,
            vec!["-c".into(), script.into()],
            60,
            window,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn attached_finish_inside_window_returns_inline_without_report() {
    let f = fixture().await;
    let Attached::Finished(info, lines) =
        start_attached(&f, "echo inline-out; exit 2", Duration::from_secs(10)).await
    else {
        panic!("a quick command must finish inline");
    };
    assert_eq!(info.exit_code, Some(2));
    assert!(lines.iter().any(|l| l.contains("inline-out")), "{lines:?}");
    // Consumed inline: forgotten, never listed, no report.
    assert!(f.registry.get(&info.id).is_none());
    assert!(f.registry.list_for_session("s-1").is_empty());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(f.dispatcher.resumed.lock().unwrap().is_empty());
    assert!(f.db.events_tail("s-1", 20).await.unwrap().is_empty());
}

#[tokio::test]
async fn attached_overrun_hands_off_to_a_reported_background_task() {
    let f = fixture().await;
    let Attached::Detached(info, _) = start_attached(
        &f,
        "echo early; sleep 1; echo late-out",
        Duration::from_millis(200),
    )
    .await
    else {
        panic!("a command outliving the window must be handed off");
    };
    assert_eq!(info.status, TaskStatus::Running);
    assert_eq!(f.registry.list_for_session("s-1").len(), 1);
    let data = wait_for_report(&f).await;
    let text = data["text"].as_str().unwrap();
    assert!(text.contains("late-out"), "{text}");
    assert_eq!(data["background_task"]["id"], info.id.as_str());
}

#[tokio::test]
async fn attached_leader_exit_keeps_detached_children() {
    let f = fixture().await;
    let marker = f._dir.path().join("project").join("survived");
    let script = format!("(sleep 0.5; touch {}) >/dev/null 2>&1 &", marker.display());
    let Attached::Finished(..) = start_attached(&f, &script, Duration::from_secs(10)).await else {
        panic!("the leader exits at once");
    };
    for _ in 0..60 {
        if marker.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("run_command's backgrounded child was killed with the leader");
}

#[tokio::test]
async fn stop_kills_the_process_and_reports_stopped() {
    let f = fixture().await;
    let info = start(&f, "sleep", &["30"]).await;
    assert_eq!(f.registry.list_for_session("s-1").len(), 1);
    assert!(f.registry.list_for_session("other").is_empty());

    let stopping = f.registry.stop(&info.id).unwrap();
    assert!(stopping.stopping);

    let data = wait_for_report(&f).await;
    let text = data["text"].as_str().unwrap();
    assert!(text.contains("STOPPED"), "{text}");
    assert_eq!(data["background_task"]["status"], "stopped");
    assert_eq!(
        f.registry.get(&info.id).unwrap().status,
        TaskStatus::Stopped
    );
    // A second stop is refused: the task already finished.
    assert!(f.registry.stop(&info.id).is_err());
}

#[tokio::test]
async fn stop_wins_over_a_clean_exit_on_sigterm() {
    let f = fixture().await;
    let info = start(
        &f,
        "sh",
        &[
            "-c",
            "trap 'exit 0' TERM; echo ready; while true; do sleep 0.1; done",
        ],
    )
    .await;
    for _ in 0..100 {
        if f.registry
            .tail(&info.id, 10)
            .unwrap()
            .iter()
            .any(|l| l == "ready")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    f.registry.stop(&info.id).unwrap();
    let data = wait_for_report(&f).await;
    assert_eq!(data["background_task"]["status"], "stopped");
    assert_eq!(data["background_task"]["exit_code"], 0);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn leader_exit_kills_orphaned_grandchildren() {
    let f = fixture().await;
    let info = start(&f, "sh", &["-c", "sleep 60 & echo $!"]).await;
    let data = wait_for_report(&f).await;
    assert_eq!(data["background_task"]["status"], "succeeded");
    let pid: u32 = f
        .registry
        .tail(&info.id, 10)
        .unwrap()
        .iter()
        .find_map(|l| l.trim().parse().ok())
        .expect("the grandchild pid was printed");
    // Gone, or a zombie waiting for its new parent to reap it.
    let alive = || {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| s.rsplit_once(") ").map(|(_, r)| !r.starts_with('Z')))
            .unwrap_or(false)
    };
    for _ in 0..100 {
        if !alive() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("grandchild {pid} outlived its task");
}

#[tokio::test]
async fn silent_session_stop_ends_tasks_without_waking_the_session() {
    let f = fixture().await;
    let info = start(&f, "sleep", &["30"]).await;
    assert!(f.registry.has_running_for_session("s-1"));
    assert!(!f.registry.has_running_for_session("other"));

    f.registry.stop_session_silently("s-1");
    for _ in 0..200 {
        if !f.registry.has_running_for_session("s-1") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!f.registry.has_running_for_session("s-1"));
    assert_eq!(
        f.registry.get(&info.id).unwrap().status,
        TaskStatus::Stopped
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(f.dispatcher.resumed.lock().unwrap().is_empty());
    assert!(f.db.events_tail("s-1", 20).await.unwrap().is_empty());
    // Not a tombstone: the session can still start tasks.
    try_start(&f, "sh", &["-c", "true"]).await.unwrap();
}

#[tokio::test]
async fn deleted_session_refuses_new_tasks() {
    let f = fixture().await;
    f.registry.kill_session("s-1");
    let err = try_start(&f, "sh", &["-c", "true"]).await.unwrap_err();
    assert!(err.contains("deleted"), "{err}");
}

#[test]
fn headline_formats() {
    assert_eq!(report::fmt_duration(192), "3m12s");
    assert_eq!(report::fmt_duration(3601), "1h0m1s");
    assert_eq!(report::fmt_duration(7), "7s");
}

/// A task still running when the server goes down leaves a sidecar; the next
/// boot reads it back and tells the owning session the task was lost, so
/// the agent isn't left to hit a silent "not found".
#[tokio::test]
async fn task_lost_to_restart_is_reported_to_its_session() {
    let f = fixture().await;
    let info = start(&f, "sh", &["-c", "sleep 30"]).await;
    let log_dir = f._dir.path().join(LOG_DIR);
    assert!(log_dir.join(format!("{}{META_SUFFIX}", info.id)).exists());

    f.registry.shutdown_all().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        f.dispatcher.resumed.lock().unwrap().is_empty(),
        "shutdown itself reports nothing"
    );

    // Next boot: collect + clear, then a fresh registry reports.
    let lost = clear_stale_logs(&log_dir);
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0].id, info.id);
    assert_eq!(lost[0].session_id, "s-1");
    assert!(!log_dir.exists());
    let next = BackgroundRegistry::new(log_dir);
    next.bind(
        f.db.clone(),
        crate::ws::broadcaster::Broadcaster::new(),
        Some(f.dispatcher.clone()),
    );
    next.report_lost(lost).await;

    let data = wait_for_report(&f).await;
    assert!(
        data["text"].as_str().unwrap().contains("was lost"),
        "{data}"
    );
    assert_eq!(data["background_task"]["status"], "lost");
}

/// Adds folder `f-2` (another directory) and session `s-2` (in `f-1`).
async fn add_second_folder_and_session(f: &Fixture) {
    let ts = chrono::Utc::now().to_rfc3339();
    let other = f._dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    f.db.create_folder(NewFolder {
        id: "f-2".into(),
        name: "f-2".into(),
        path: other.to_string_lossy().to_string(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    f.db.create_session(NewSession {
        id: "s-2".into(),
        name: "worker two".into(),
        folder_id: "f-1".into(),
        created_at: ts.clone(),
        last_activity: ts,
        ..Default::default()
    })
    .await
    .unwrap();
}

async fn try_start_as(
    registry: &Arc<BackgroundRegistry>,
    db: &Db,
    session: &str,
    folder: &str,
    program: &str,
    args: &[&str],
) -> Result<TaskInfo, String> {
    let db = db.clone();
    let (s, fo, p) = (session.to_string(), folder.to_string(), program.to_string());
    let prepared = tokio::task::spawn_blocking(move || {
        let inv = InvocationContext {
            session_id: Some(s),
            folder_id: Some(fo),
            ..Default::default()
        };
        prepare_exec(&db, &p, &inv, false, None)
    })
    .await
    .unwrap()
    .unwrap();
    registry
        .spawn(
            session,
            prepared,
            args.iter().map(|s| s.to_string()).collect(),
            None,
            None,
        )
        .await
}

async fn wait_not_running(registry: &BackgroundRegistry, id: &str) {
    for _ in 0..200 {
        if registry
            .get(id)
            .is_some_and(|t| t.status != TaskStatus::Running)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task {id} never finished");
}

#[tokio::test]
async fn duplicate_start_is_refused_naming_the_running_copy() {
    let f = fixture().await;
    add_second_folder_and_session(&f).await;
    let first = start(&f, "sh", &["-c", "sleep 30  # war"]).await;

    // Another session, same folder, whitespace-only differences, other label.
    let err = try_start_as(
        &f.registry,
        &f.db,
        "s-2",
        "f-1",
        "sh",
        &[" -c", "sleep   30 # war "],
    )
    .await
    .unwrap_err();
    assert!(err.starts_with("Already running: task "), "{err}");
    assert!(err.contains(&first.id), "{err}");
    assert!(err.contains("session s-1 (s-1)"), "{err}");
    assert!(err.contains(&first.log_path), "{err}");
    assert!(err.contains(&first.cwd), "{err}");
    // The owner itself is pointed at background_status.
    let err = try_start(&f, "sh", &["-c", "sleep 30  # war"])
        .await
        .unwrap_err();
    assert!(
        err.contains(&format!("background_status {}", first.id)),
        "{err}"
    );
    assert_eq!(f.registry.list_running().len(), 1);

    // Same command in another directory, or another command here: allowed.
    try_start_as(
        &f.registry,
        &f.db,
        "s-2",
        "f-2",
        "sh",
        &["-c", "sleep 30  # war"],
    )
    .await
    .unwrap();
    try_start_as(&f.registry, &f.db, "s-2", "f-1", "sh", &["-c", "sleep 31"])
        .await
        .unwrap();

    // Once the first is gone, the command may start again.
    f.registry.stop(&first.id).unwrap();
    wait_not_running(&f.registry, &first.id).await;
    try_start(&f, "sh", &["-c", "sleep 30  # war"])
        .await
        .unwrap();
    f.registry.shutdown_all().await;
}

#[tokio::test]
async fn concurrent_identical_starts_admit_exactly_one() {
    let f = fixture().await;
    let (a, b) = tokio::join!(
        try_start(&f, "sleep", &["30"]),
        try_start(&f, "sleep", &["30"])
    );
    assert_eq!(
        [&a, &b].iter().filter(|r| r.is_ok()).count(),
        1,
        "{a:?} {b:?}"
    );
    assert_eq!(f.registry.list_running().len(), 1);
    f.registry.shutdown_all().await;
}

#[tokio::test]
async fn task_lost_to_restart_does_not_block_a_rerun() {
    let f = fixture().await;
    start(&f, "sleep", &["30"]).await;
    f.registry.shutdown_all().await;
    let log_dir = f._dir.path().join(LOG_DIR);
    let lost = clear_stale_logs(&log_dir);
    assert_eq!(lost.len(), 1);
    let next = Arc::new(BackgroundRegistry::new(log_dir));
    next.bind(
        f.db.clone(),
        crate::ws::broadcaster::Broadcaster::new(),
        Some(f.dispatcher.clone()),
    );
    next.report_lost(lost).await;
    try_start_as(&next, &f.db, "s-1", "f-1", "sleep", &["30"])
        .await
        .unwrap();
    next.shutdown_all().await;
}

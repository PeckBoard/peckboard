//! Integration tests that lock in the contract of the worker / session
//! lifecycle plumbing fixed in the "session management is fucked" pass:
//!
//! 1. `interrupt` actually stops the run and delivers a completion
//!    notification.
//! 2. `send_or_queue` is atomic: a message sent while busy is queued, not
//!    fire-a-second-agent.
//! 3. The queue drains on every termination path, not just clean exits.
//! 4. `send_or_queue` does not double-spawn under concurrent callers.
//! 5. The watchdog respects the grace period and the per-session lock.
//!
//! All tests use the mock provider so they're deterministic and don't
//! depend on the real `claude` CLI.

use std::sync::Arc;
use std::time::Duration;

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewQueuedMessage, NewSession};
use peckboard::provider::agent::ProcessCompletion;
use peckboard::provider::claude::register_claude_provider;
use peckboard::provider::manager::{SendOutcome, SessionManager};
use peckboard::provider::mock::register_mock_provider;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::provider::stream::SpawnConfig;
use peckboard::ws::broadcaster::Broadcaster;

async fn build_env() -> (SessionManager, Db, Arc<Broadcaster>) {
    let registry = Arc::new(ProviderRegistry::new());
    register_claude_provider(&registry).await;
    register_mock_provider(&registry).await;
    let manager = SessionManager::new(registry);
    let db = Db::in_memory().unwrap();
    let broadcaster = Broadcaster::new();
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "F".into(),
        path: "/tmp".into(),
        created_at: ts,
    })
    .await
    .unwrap();
    (manager, db, broadcaster)
}

async fn make_session(db: &Db, id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
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
        created_at: ts.clone(),
        last_activity: ts,
    })
    .await
    .unwrap();
}

fn cfg(model: &str) -> SpawnConfig {
    SpawnConfig {
        model: model.into(),
        effort: None,
        working_dir: String::new(),
        mcp_config_path: None,
        env: Default::default(),
        permission_mode: None,
        timeout_ms: None,
        metadata: serde_json::Value::Null,
    }
}

async fn wait_for_completion(
    rx: &mut tokio::sync::mpsc::Receiver<ProcessCompletion>,
    sid: &str,
) -> ProcessCompletion {
    let c = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("completion timeout")
        .expect("channel still open");
    assert_eq!(c.session_id, sid);
    c
}

// ── 1. Interrupt actually stops the run ─────────────────────────────────

#[tokio::test]
async fn interrupt_aborts_blocking_run_and_delivers_completion() {
    let (manager, db, broadcaster) = build_env().await;
    let mut rx = manager.take_completion_rx().await.unwrap();
    make_session(&db, "s1").await;

    // mock:ask blocks indefinitely waiting on stdin.
    manager
        .send_message("s1", "go", &db, &broadcaster, cfg("mock:ask"))
        .await
        .unwrap();

    // Let the run register itself so is_running flips true.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(manager.is_running("s1").await);

    // Interrupt — must terminate the run AND deliver a completion.
    manager.interrupt("s1").await;
    let completion = wait_for_completion(&mut rx, "s1").await;
    assert!(
        !completion.completed,
        "interrupted run should report completed=false"
    );
    assert!(
        !manager.is_running("s1").await,
        "should no longer be running"
    );

    // An agent-end event of kind crash must be in the log (no orphaned
    // spinner state on the UI).
    let kinds: Vec<String> = db
        .events_tail("s1", 50)
        .await
        .unwrap()
        .iter()
        .map(|e| e.kind.clone())
        .collect();
    assert!(
        kinds.contains(&"agent-end".to_string()),
        "expected agent-end in event log, got {kinds:?}"
    );
}

// ── 2. Atomic send-or-queue ────────────────────────────────────────────

#[tokio::test]
async fn send_or_queue_queues_when_agent_already_running() {
    let (manager, db, broadcaster) = build_env().await;
    let mut rx = manager.take_completion_rx().await.unwrap();
    make_session(&db, "s2").await;

    // First message starts a blocking run.
    let first = manager
        .send_or_queue("s2", "first", &db, &broadcaster, cfg("mock:ask"))
        .await
        .unwrap();
    assert_eq!(first, SendOutcome::Started);
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Second message must be queued, not spawn a parallel agent.
    let second = manager
        .send_or_queue("s2", "second", &db, &broadcaster, cfg("mock:ask"))
        .await
        .unwrap();
    assert_eq!(second, SendOutcome::Queued);

    // The queued message is persisted.
    let queued = db.get_queued_message("s2").await.unwrap();
    assert_eq!(queued.unwrap().text, "second");

    // Drain so the run we started doesn't leak into the next test.
    manager.interrupt("s2").await;
    let _ = wait_for_completion(&mut rx, "s2").await;
}

// ── 3. Queue drains on every termination path ──────────────────────────

#[tokio::test]
async fn drain_queued_delivers_after_clean_completion() {
    let (manager, db, broadcaster) = build_env().await;
    let mut rx = manager.take_completion_rx().await.unwrap();
    make_session(&db, "s3").await;

    // Pre-seed a queued message so a finished run will trigger a drain.
    db.upsert_queued_message(NewQueuedMessage {
        session_id: "s3".into(),
        text: "follow-up".into(),
        queued_at: chrono::Utc::now().to_rfc3339(),
        ..Default::default()
    })
    .await
    .unwrap();

    // Start (and let complete) a short echo run.
    manager
        .send_message("s3", "first", &db, &broadcaster, cfg("mock:echo"))
        .await
        .unwrap();
    let first = wait_for_completion(&mut rx, "s3").await;
    assert!(first.completed);

    // Drain — must dispatch the queued message as a fresh run.
    let drained = manager
        .drain_queued("s3", &db, &broadcaster, cfg("mock:echo"))
        .await
        .unwrap();
    assert!(drained, "drain_queued should report a delivery");
    assert!(db.get_queued_message("s3").await.unwrap().is_none());

    let second = wait_for_completion(&mut rx, "s3").await;
    assert!(second.completed);

    // The text echoed in the second run is the queued message.
    let text_events: Vec<String> = db
        .events_tail("s3", 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "agent-text")
        .map(|e| {
            serde_json::from_str::<serde_json::Value>(&e.data)
                .unwrap()
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        })
        .collect();
    assert!(
        text_events.iter().any(|t| t == "follow-up"),
        "expected the queued text to appear in agent output, got {text_events:?}"
    );
}

#[tokio::test]
async fn drain_queued_delivers_after_interrupted_run() {
    let (manager, db, broadcaster) = build_env().await;
    let mut rx = manager.take_completion_rx().await.unwrap();
    make_session(&db, "s4").await;

    // Start a blocking run and queue a message while it's busy.
    manager
        .send_message("s4", "first", &db, &broadcaster, cfg("mock:ask"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    db.upsert_queued_message(NewQueuedMessage {
        session_id: "s4".into(),
        text: "drain-me".into(),
        queued_at: chrono::Utc::now().to_rfc3339(),
        ..Default::default()
    })
    .await
    .unwrap();

    // Interrupt the blocking run — completion arrives with completed=false.
    manager.interrupt("s4").await;
    let first = wait_for_completion(&mut rx, "s4").await;
    assert!(!first.completed);

    // Drain MUST still deliver — failing runs cannot leave queue items
    // stranded.
    let drained = manager
        .drain_queued("s4", &db, &broadcaster, cfg("mock:echo"))
        .await
        .unwrap();
    assert!(drained);
    let second = wait_for_completion(&mut rx, "s4").await;
    assert!(second.completed);
}

#[tokio::test]
async fn drain_queued_is_noop_when_nothing_queued() {
    let (manager, db, broadcaster) = build_env().await;
    let _rx = manager.take_completion_rx().await.unwrap();
    make_session(&db, "s5").await;

    let drained = manager
        .drain_queued("s5", &db, &broadcaster, cfg("mock:echo"))
        .await
        .unwrap();
    assert!(!drained, "drain on empty queue should be a no-op");
}

#[tokio::test]
async fn drain_queued_is_noop_while_already_running() {
    let (manager, db, broadcaster) = build_env().await;
    let mut rx = manager.take_completion_rx().await.unwrap();
    make_session(&db, "s6").await;

    db.upsert_queued_message(NewQueuedMessage {
        session_id: "s6".into(),
        text: "later".into(),
        queued_at: chrono::Utc::now().to_rfc3339(),
        ..Default::default()
    })
    .await
    .unwrap();

    // Start a long-running ask scenario.
    manager
        .send_message("s6", "go", &db, &broadcaster, cfg("mock:ask"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Drain attempted while busy must skip — queue stays put.
    let drained = manager
        .drain_queued("s6", &db, &broadcaster, cfg("mock:echo"))
        .await
        .unwrap();
    assert!(!drained, "drain must skip while a run is in flight");
    assert!(
        db.get_queued_message("s6").await.unwrap().is_some(),
        "queue entry must survive a no-op drain"
    );

    // Tear down.
    manager.interrupt("s6").await;
    let _ = wait_for_completion(&mut rx, "s6").await;
}

// ── 4. Concurrent send_or_queue does not double-spawn ──────────────────

#[tokio::test]
async fn concurrent_send_or_queue_never_double_spawns() {
    let (manager, db, broadcaster) = build_env().await;
    let mut rx = manager.take_completion_rx().await.unwrap();
    make_session(&db, "s7").await;
    let manager = Arc::new(manager);
    let broadcaster_a = broadcaster.clone();
    let broadcaster_b = broadcaster.clone();
    let db_a = db.clone();
    let db_b = db.clone();
    let m_a = manager.clone();
    let m_b = manager.clone();

    // Two concurrent send_or_queue calls on the same session.
    let h_a = tokio::spawn(async move {
        m_a.send_or_queue("s7", "A", &db_a, &broadcaster_a, cfg("mock:ask"))
            .await
            .unwrap()
    });
    let h_b = tokio::spawn(async move {
        m_b.send_or_queue("s7", "B", &db_b, &broadcaster_b, cfg("mock:ask"))
            .await
            .unwrap()
    });
    let outcomes = [h_a.await.unwrap(), h_b.await.unwrap()];
    let started = outcomes
        .iter()
        .filter(|o| matches!(o, SendOutcome::Started))
        .count();
    let queued = outcomes
        .iter()
        .filter(|o| matches!(o, SendOutcome::Queued))
        .count();
    assert_eq!(
        started, 1,
        "exactly one of the two concurrent sends should start a run"
    );
    assert_eq!(queued, 1, "the other must be queued");

    // Exactly one agent-start event must exist in the log.
    let agent_starts = db
        .events_tail("s7", 100)
        .await
        .unwrap()
        .iter()
        .filter(|e| e.kind == "agent-start")
        .count();
    assert_eq!(
        agent_starts, 1,
        "concurrent send_or_queue must not double-spawn (got {agent_starts} agent-start events)"
    );

    manager.interrupt("s7").await;
    let _ = wait_for_completion(&mut rx, "s7").await;
}

// ── 5. Watchdog grace + per-session lock ───────────────────────────────

#[tokio::test]
async fn lock_session_serializes_handlers() {
    let (manager, _db, _broadcaster) = build_env().await;
    let manager = Arc::new(manager);

    let g1 = manager.lock_session("sx").await;
    // try_lock should fail while another guard holds it.
    let try1 = manager.try_lock_session("sx").await;
    assert!(try1.is_none(), "try_lock should fail while held");
    drop(g1);
    let try2 = manager.try_lock_session("sx").await;
    assert!(try2.is_some(), "try_lock should succeed after release");
}

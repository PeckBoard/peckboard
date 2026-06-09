//! End-to-end tests for the repeating-task scheduler.
//!
//! These exercise the type-safe dispatch guarantee: a forced run-now that
//! races a concurrent run-now must produce *at most one* new session, and
//! the second call must report `AlreadyRunning` rather than spawning a
//! parallel run.

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewRepeatingTask, NewSession};
use peckboard::provider::manager::SessionManager;
use peckboard::provider::mock::register_mock_provider;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::repeating::{RepeatingTaskManager, RunContext, StartOutcome, initial_next_run_at};
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::ws::broadcaster::Broadcaster;
use std::path::PathBuf;
use std::sync::Arc;

struct TestEnv {
    db: Db,
    broadcaster: Arc<Broadcaster>,
    session_manager: SessionManager,
    rtm: RepeatingTaskManager,
    mcp_tokens: McpTokenRegistry,
    data_dir: tempfile::TempDir,
}

impl TestEnv {
    fn run_ctx(&self) -> RunContext<'_> {
        RunContext {
            db: &self.db,
            broadcaster: &self.broadcaster,
            session_manager: &self.session_manager,
            mcp_tokens: &self.mcp_tokens,
            data_dir: self.data_dir.path(),
            http_port: 0,
        }
    }
    fn data_dir_path(&self) -> PathBuf {
        self.data_dir.path().to_path_buf()
    }
}

async fn fresh_state() -> TestEnv {
    let db = Db::in_memory().expect("in-memory db");
    let registry = Arc::new(ProviderRegistry::new());
    register_mock_provider(&registry).await;
    let session_manager = SessionManager::new(registry);
    let broadcaster = Broadcaster::new();
    let rtm = RepeatingTaskManager::new();
    let mcp_tokens = McpTokenRegistry::new();
    let data_dir = tempfile::tempdir().unwrap();
    TestEnv {
        db,
        broadcaster,
        session_manager,
        rtm,
        mcp_tokens,
        data_dir,
    }
}

async fn seed_folder(db: &Db, id: &str, path: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: id.into(),
        name: id.into(),
        path: path.into(),
        created_at: ts,
    })
    .await
    .unwrap();
}

async fn seed_task(db: &Db, id: &str, folder_id: &str, prompt: &str, model: Option<&str>) {
    let ts = chrono::Utc::now().to_rfc3339();
    let draft = peckboard::db::models::RepeatingTask {
        id: id.into(),
        name: id.into(),
        description: "".into(),
        folder_id: folder_id.into(),
        prompt: prompt.into(),
        schedule_kind: "interval".into(),
        schedule_value: r#"{"minutes":60}"#.into(),
        model: model.map(str::to_string),
        effort: None,
        enabled: true,
        next_run_at: None,
        last_run_at: None,
        created_at: ts.clone(),
        updated_at: ts.clone(),
    };
    let next = initial_next_run_at(&draft);
    db.create_repeating_task(NewRepeatingTask {
        id: id.into(),
        name: id.into(),
        description: "".into(),
        folder_id: folder_id.into(),
        prompt: prompt.into(),
        schedule_kind: "interval".into(),
        schedule_value: r#"{"minutes":60}"#.into(),
        model: model.map(str::to_string),
        effort: None,
        enabled: true,
        next_run_at: next,
        last_run_at: None,
        created_at: ts.clone(),
        updated_at: ts,
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn force_run_spawns_a_session_for_the_task() {
    let env = fresh_state().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_folder(&env.db, "f1", tmp.path().to_str().unwrap()).await;
    seed_task(&env.db, "t1", "f1", "hello", Some("mock:happy-path")).await;

    let outcome = env
        .rtm
        .try_run_now("t1", env.run_ctx(), false)
        .await
        .unwrap();
    assert_eq!(outcome, StartOutcome::Spawned);

    // Wait for the mock provider to wind down so the session leaves
    // is_running. Mock plays its scripted sequence in <1s; 5s is plenty.
    let session_id = {
        let sessions = env.db.list_sessions_by_repeating_task("t1").await.unwrap();
        assert_eq!(sessions.len(), 1, "expected exactly one session");
        sessions[0].id.clone()
    };
    for _ in 0..50 {
        if !env.session_manager.is_running(&session_id).await {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    env.session_manager.cancel_and_wait(&session_id).await;

    let after = env.db.get_repeating_task("t1").await.unwrap().unwrap();
    assert!(after.last_run_at.is_some());
    assert!(after.next_run_at.is_some());
}

#[tokio::test]
async fn force_run_does_not_double_spawn_when_already_running() {
    let env = fresh_state().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_folder(&env.db, "f1", tmp.path().to_str().unwrap()).await;
    seed_task(&env.db, "t1", "f1", "hello", Some("mock:happy-path")).await;

    let first = env
        .rtm
        .try_run_now("t1", env.run_ctx(), false)
        .await
        .unwrap();
    assert_eq!(first, StartOutcome::Spawned);

    let first_session_id = env
        .db
        .list_sessions_by_repeating_task("t1")
        .await
        .unwrap()
        .first()
        .map(|s| s.id.clone())
        .unwrap();

    let mut hit = false;
    for _ in 0..100 {
        if env.session_manager.is_running(&first_session_id).await {
            let outcome = env
                .rtm
                .try_run_now("t1", env.run_ctx(), false)
                .await
                .unwrap();
            assert_eq!(
                outcome,
                StartOutcome::AlreadyRunning,
                "second run must report AlreadyRunning while the first is still in flight",
            );
            hit = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(hit, "first run never reached is_running=true within 1s");

    for _ in 0..100 {
        if !env.session_manager.is_running(&first_session_id).await {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    env.session_manager.cancel_and_wait(&first_session_id).await;

    let sessions = env.db.list_sessions_by_repeating_task("t1").await.unwrap();
    assert_eq!(
        sessions.len(),
        1,
        "exactly one session should exist; got {:?}",
        sessions.iter().map(|s| &s.id).collect::<Vec<_>>(),
    );
}

#[tokio::test]
async fn force_run_skips_disabled_task_when_respect_enabled() {
    let env = fresh_state().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_folder(&env.db, "f1", tmp.path().to_str().unwrap()).await;
    seed_task(&env.db, "t1", "f1", "hello", Some("mock:happy-path")).await;
    env.db
        .update_repeating_task(
            "t1",
            peckboard::db::models::UpdateRepeatingTask {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let outcome = env
        .rtm
        .try_run_now("t1", env.run_ctx(), true)
        .await
        .unwrap();
    assert_eq!(outcome, StartOutcome::Disabled);
    assert!(
        env.db
            .list_sessions_by_repeating_task("t1")
            .await
            .unwrap()
            .is_empty()
    );

    // The same call with respect_enabled=false (the force-run route)
    // bypasses the gate.
    let outcome = env
        .rtm
        .try_run_now("t1", env.run_ctx(), false)
        .await
        .unwrap();
    assert_eq!(outcome, StartOutcome::Spawned);
    let sessions = env.db.list_sessions_by_repeating_task("t1").await.unwrap();
    assert_eq!(sessions.len(), 1);
    env.session_manager.cancel_and_wait(&sessions[0].id).await;
}

#[tokio::test]
async fn delete_task_detaches_sessions_instead_of_deleting_them() {
    let env = fresh_state().await;
    let db = &env.db;
    seed_folder(db, "f1", "/tmp/peckboard-test-rt").await;
    seed_task(db, "t1", "f1", "hello", None).await;
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_session(NewSession {
        id: "s1".into(),
        name: "spawned".into(),
        folder_id: "f1".into(),
        model: None,
        effort: None,
        is_worker: false,
        project_id: None,
        card_id: None,
        conversation_id: None,
        created_at: ts.clone(),
        last_activity: ts,
        repeating_task_id: Some("t1".into()),
        ..Default::default()
    })
    .await
    .unwrap();

    assert!(db.delete_repeating_task("t1").await.unwrap());
    let session = db.get_session("s1").await.unwrap().unwrap();
    assert!(session.repeating_task_id.is_none());
}

#[tokio::test]
async fn list_due_repeating_tasks_filters_by_next_run_at() {
    let env = fresh_state().await;
    let db = &env.db;
    seed_folder(db, "f1", "/tmp/peckboard-test-due").await;
    seed_task(db, "future", "f1", "later", None).await;
    seed_task(db, "past", "f1", "now", None).await;

    // Pin "past" to a time in the past, "future" to the future.
    db.update_repeating_task(
        "past",
        peckboard::db::models::UpdateRepeatingTask {
            next_run_at: Some(Some("2020-01-01T00:00:00Z".into())),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    db.update_repeating_task(
        "future",
        peckboard::db::models::UpdateRepeatingTask {
            next_run_at: Some(Some("2099-01-01T00:00:00Z".into())),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let now = chrono::Utc::now().to_rfc3339();
    let due = db.list_due_repeating_tasks(&now).await.unwrap();
    let ids: Vec<&str> = due.iter().map(|t| t.id.as_str()).collect();
    assert!(ids.contains(&"past"), "got {ids:?}");
    assert!(!ids.contains(&"future"), "got {ids:?}");

    // Disabled tasks must not surface even if next_run_at is past.
    db.update_repeating_task(
        "past",
        peckboard::db::models::UpdateRepeatingTask {
            enabled: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let due = db.list_due_repeating_tasks(&now).await.unwrap();
    let ids: Vec<&str> = due.iter().map(|t| t.id.as_str()).collect();
    assert!(!ids.contains(&"past"));
}

#[tokio::test]
async fn run_due_tasks_processes_overdue_tasks_then_advances_next_run_at() {
    let env = fresh_state().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_folder(&env.db, "f1", tmp.path().to_str().unwrap()).await;
    seed_task(&env.db, "t1", "f1", "go", Some("mock:happy-path")).await;
    // Force it to be due right now.
    env.db
        .update_repeating_task(
            "t1",
            peckboard::db::models::UpdateRepeatingTask {
                next_run_at: Some(Some("2020-01-01T00:00:00Z".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    env.rtm.run_due_tasks(env.run_ctx()).await;

    let after = env.db.get_repeating_task("t1").await.unwrap().unwrap();
    assert!(
        after.last_run_at.is_some(),
        "scheduler tick should mark last_run_at",
    );
    let parsed: chrono::DateTime<chrono::Utc> =
        after.next_run_at.as_deref().unwrap().parse().unwrap();
    let now = chrono::Utc::now();
    assert!(
        parsed > now,
        "next_run_at must advance to the future after a successful run; got {parsed} vs now {now}",
    );

    // Wait for the run to wind down so the test runtime can tear down cleanly.
    let sessions = env.db.list_sessions_by_repeating_task("t1").await.unwrap();
    for s in &sessions {
        env.session_manager.cancel_and_wait(&s.id).await;
    }
}

#[tokio::test]
async fn folder_cascade_delete_drops_repeating_tasks() {
    let env = fresh_state().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_folder(&env.db, "f1", tmp.path().to_str().unwrap()).await;
    seed_task(&env.db, "t1", "f1", "x", None).await;
    seed_task(&env.db, "t2", "f1", "y", None).await;

    let report = env.db.delete_folder_cascade("f1").await.unwrap();
    assert_eq!(report.sessions_deleted, 0);
    assert!(env.db.list_repeating_tasks().await.unwrap().is_empty());
    assert!(env.db.get_folder("f1").await.unwrap().is_none());
}

#[tokio::test]
async fn folder_empty_delete_drops_repeating_tasks_in_same_transaction() {
    let env = fresh_state().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_folder(&env.db, "f1", tmp.path().to_str().unwrap()).await;
    seed_task(&env.db, "t1", "f1", "x", None).await;

    let outcome = env.db.delete_folder_if_empty("f1").await.unwrap();
    assert_eq!(
        outcome,
        peckboard::db::crud::FolderEmptyDelete::Deleted,
        "empty-folder delete should succeed even when only tasks exist",
    );
    assert!(env.db.list_repeating_tasks().await.unwrap().is_empty());
}

#[tokio::test]
async fn run_ctx_writes_mcp_config_for_spawned_session() {
    let env = fresh_state().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_folder(&env.db, "f1", tmp.path().to_str().unwrap()).await;
    seed_task(&env.db, "t1", "f1", "go", Some("mock:happy-path")).await;

    let outcome = env
        .rtm
        .try_run_now("t1", env.run_ctx(), false)
        .await
        .unwrap();
    assert_eq!(outcome, StartOutcome::Spawned);

    let session = env
        .db
        .list_sessions_by_repeating_task("t1")
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    // The MCP config is written to `<data_dir>/mcp/<session_id>.json` — we
    // just check that some file got created in the mcp dir, since the
    // exact path is an implementation detail.
    let mcp_dir = env.data_dir_path().join("worker-mcp");
    let config_path = mcp_dir.join(format!("{}.json", session.id));
    assert!(
        config_path.exists(),
        "expected an MCP config file at {} for session {}",
        config_path.display(),
        session.id,
    );

    env.session_manager.cancel_and_wait(&session.id).await;
}

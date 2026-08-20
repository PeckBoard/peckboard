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
use peckboard::repeating::{
    RepeatingTaskManager, RunAuditor, RunContext, StartOutcome, initial_next_run_at,
};
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::ws::broadcaster::Broadcaster;
use std::path::PathBuf;
use std::sync::Arc;

struct TestEnv {
    db: Db,
    broadcaster: Arc<Broadcaster>,
    session_manager: SessionManager,
    rtm: RepeatingTaskManager,
    auditor: RunAuditor,
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
            auditor: &self.auditor,
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
    let auditor = RunAuditor::new();
    let mcp_tokens = McpTokenRegistry::new();
    let data_dir = tempfile::tempdir().unwrap();
    TestEnv {
        db,
        broadcaster,
        session_manager,
        rtm,
        auditor,
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
        timezone: None,
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
        timezone: None,
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn scheduler_throttles_runs_below_min_gap_via_inline_guard() {
    // Last run 30 seconds ago on a 60-minute interval: the next slot
    // after last_run_at is still ~59 minutes away, so the scheduler
    // trigger must not fire even if stored next_run_at is in the past.
    let env = fresh_state().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_folder(&env.db, "f1", tmp.path().to_str().unwrap()).await;
    seed_task(&env.db, "t1", "f1", "go", Some("mock:happy-path")).await;

    let now = chrono::Utc::now();
    let recent = (now - chrono::Duration::seconds(30)).to_rfc3339();
    env.db
        .update_repeating_task(
            "t1",
            peckboard::db::models::UpdateRepeatingTask {
                last_run_at: Some(Some(recent)),
                next_run_at: Some(Some("2020-01-01T00:00:00Z".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    env.rtm.run_due_tasks(env.run_ctx()).await;

    let sessions = env.db.list_sessions_by_repeating_task("t1").await.unwrap();
    assert!(
        sessions.is_empty(),
        "not-yet-due scheduler tick must not spawn a session; got {:?}",
        sessions.iter().map(|s| &s.id).collect::<Vec<_>>(),
    );
}

#[tokio::test]
async fn scheduler_fires_when_next_slot_after_last_run_is_past_even_if_stored_next_is_future() {
    // Regression: bumping next_run_at into the future (throttle /
    // already-running) used to drop an overdue slot on the floor.
    // Last execution 90 minutes ago on a 60-minute interval means the
    // slot at last+60m is 30 minutes past and has not run — fire it,
    // ignoring the stored 2099 next_run_at.
    let env = fresh_state().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_folder(&env.db, "f1", tmp.path().to_str().unwrap()).await;
    seed_task(&env.db, "t1", "f1", "go", Some("mock:happy-path")).await;

    let now = chrono::Utc::now();
    let last = (now - chrono::Duration::minutes(90)).to_rfc3339();
    env.db
        .update_repeating_task(
            "t1",
            peckboard::db::models::UpdateRepeatingTask {
                last_run_at: Some(Some(last)),
                next_run_at: Some(Some("2099-01-01T00:00:00Z".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    env.rtm.run_due_tasks(env.run_ctx()).await;

    let sessions = env.db.list_sessions_by_repeating_task("t1").await.unwrap();
    assert_eq!(
        sessions.len(),
        1,
        "overdue slot after last_run_at must dispatch even when stored next_run_at is in the future",
    );
    for s in &sessions {
        env.session_manager.cancel_and_wait(&s.id).await;
    }
}

#[tokio::test]
async fn scheduler_does_not_fire_when_next_slot_after_last_run_is_still_future() {
    // Inverse of the overdue-slot test: a stale next_run_at in the past
    // must not pull the task into the trigger while the next occurrence
    // after last_run_at is still ahead.
    let env = fresh_state().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_folder(&env.db, "f1", tmp.path().to_str().unwrap()).await;
    seed_task(&env.db, "t1", "f1", "go", Some("mock:happy-path")).await;

    let now = chrono::Utc::now();
    let last = (now - chrono::Duration::minutes(10)).to_rfc3339();
    env.db
        .update_repeating_task(
            "t1",
            peckboard::db::models::UpdateRepeatingTask {
                last_run_at: Some(Some(last)),
                next_run_at: Some(Some("2020-01-01T00:00:00Z".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    env.rtm.run_due_tasks(env.run_ctx()).await;

    let sessions = env.db.list_sessions_by_repeating_task("t1").await.unwrap();
    assert!(
        sessions.is_empty(),
        "next slot after last_run_at is still future; must not dispatch. got {:?}",
        sessions.iter().map(|s| &s.id).collect::<Vec<_>>(),
    );
}

#[tokio::test]
async fn manual_run_bypasses_throttle_even_immediately_after_prior_run() {
    let env = fresh_state().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_folder(&env.db, "f1", tmp.path().to_str().unwrap()).await;
    seed_task(&env.db, "t1", "f1", "go", Some("mock:happy-path")).await;

    // Pin last_run_at to 5 seconds ago — well below the 60s hard floor.
    let now = chrono::Utc::now();
    let recent = (now - chrono::Duration::seconds(5)).to_rfc3339();
    env.db
        .update_repeating_task(
            "t1",
            peckboard::db::models::UpdateRepeatingTask {
                last_run_at: Some(Some(recent)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let outcome = env
        .rtm
        .try_run_now("t1", env.run_ctx(), false)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        StartOutcome::Spawned,
        "manual force-run must bypass the throttle",
    );
    let sessions = env.db.list_sessions_by_repeating_task("t1").await.unwrap();
    assert_eq!(sessions.len(), 1);
    env.session_manager.cancel_and_wait(&sessions[0].id).await;
}

#[tokio::test]
async fn watchdog_disables_task_when_persisted_runs_violate_invariant() {
    // Seed two scheduler-spawned sessions 10 seconds apart for a
    // 5-minute task — the inline guard would have blocked them, but
    // we're simulating "future bug bypassed the inline guard" by
    // writing the rows directly. The watchdog's persisted-sessions
    // pass must catch the violation and disable the task.
    let env = fresh_state().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_folder(&env.db, "f1", tmp.path().to_str().unwrap()).await;
    seed_task(&env.db, "t1", "f1", "go", None).await;

    let now = chrono::Utc::now();
    for (id, offset) in [("s_a", 0i64), ("s_b", 10)] {
        let ts = (now + chrono::Duration::seconds(offset)).to_rfc3339();
        env.db
            .create_session(NewSession {
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
                repeating_task_id: Some("t1".into()),
                ..Default::default()
            })
            .await
            .unwrap();
    }

    let n = env.auditor.audit_pass(&env.db, &env.broadcaster).await;
    assert!(n >= 1);
    let after = env.db.get_repeating_task("t1").await.unwrap().unwrap();
    assert!(
        !after.enabled,
        "watchdog must disable the task after a violation",
    );
    assert!(
        after.next_run_at.is_none(),
        "watchdog must clear next_run_at"
    );
}

#[tokio::test]
async fn watchdog_ignores_pair_when_one_session_is_marked_manual() {
    // Same setup as the violation test, but the second session is
    // flagged as manual via try_run_now's path. The watchdog must
    // suppress the alarm because manual runs are explicitly exempt.
    let env = fresh_state().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_folder(&env.db, "f1", tmp.path().to_str().unwrap()).await;
    seed_task(&env.db, "t1", "f1", "go", None).await;

    let now = chrono::Utc::now();
    for (id, offset) in [("s_a", 0i64), ("s_b", 10)] {
        let ts = (now + chrono::Duration::seconds(offset)).to_rfc3339();
        env.db
            .create_session(NewSession {
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
                repeating_task_id: Some("t1".into()),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    env.auditor.mark_manual_session("s_b").await;

    let n = env.auditor.audit_pass(&env.db, &env.broadcaster).await;
    assert_eq!(n, 0);
    let after = env.db.get_repeating_task("t1").await.unwrap().unwrap();
    assert!(
        after.enabled,
        "task must remain enabled when only one of the pair is auto"
    );
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
async fn force_run_records_run_history() {
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

    let session_id = {
        let sessions = env.db.list_sessions_by_repeating_task("t1").await.unwrap();
        sessions[0].id.clone()
    };
    for _ in 0..50 {
        if !env.session_manager.is_running(&session_id).await {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    env.session_manager.cancel_and_wait(&session_id).await;

    let runs = env.db.list_repeating_task_runs("t1", 50).await.unwrap();
    assert_eq!(runs.len(), 1, "expected one dispatch recorded");
    assert_eq!(runs[0].status, "spawned");
    assert_eq!(runs[0].trigger, "manual");
    assert_eq!(runs[0].session_id.as_deref(), Some(session_id.as_str()));

    // A second force-run while the first session is detached should
    // record another "spawned" row -- run history isn't deduplicated.
    let outcome2 = env
        .rtm
        .try_run_now("t1", env.run_ctx(), false)
        .await
        .unwrap();
    assert_eq!(outcome2, StartOutcome::Spawned);
    let runs = env.db.list_repeating_task_runs("t1", 50).await.unwrap();
    assert_eq!(runs.len(), 2);
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

#[tokio::test]
async fn scheduler_disables_task_with_corrupt_schedule_instead_of_refiring() {
    // A row whose schedule_value can't be parsed has a NULL next_run_at,
    // which `is_scheduler_due` treats as "due now" — so it would
    // spawn a fresh session every tick. The scheduler must refuse the
    // dispatch and disable the row instead.
    let env = fresh_state().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_folder(&env.db, "f1", tmp.path().to_str().unwrap()).await;

    let ts = chrono::Utc::now().to_rfc3339();
    env.db
        .create_repeating_task(NewRepeatingTask {
            id: "t1".into(),
            name: "t1".into(),
            description: "".into(),
            folder_id: "f1".into(),
            prompt: "go".into(),
            schedule_kind: "interval".into(),
            schedule_value: r#"{"minutes":"banana"}"#.into(),
            model: Some("mock:happy-path".into()),
            effort: None,
            enabled: true,
            next_run_at: None,
            last_run_at: None,
            created_at: ts.clone(),
            updated_at: ts,
            timezone: None,
        })
        .await
        .unwrap();

    env.rtm.run_due_tasks(env.run_ctx()).await;

    let sessions = env.db.list_sessions_by_repeating_task("t1").await.unwrap();
    assert!(
        sessions.is_empty(),
        "corrupt schedule must not dispatch; got {:?}",
        sessions.iter().map(|s| &s.id).collect::<Vec<_>>(),
    );

    let task = env.db.get_repeating_task("t1").await.unwrap().unwrap();
    assert!(!task.enabled, "corrupt-schedule task must be disabled");
    assert!(task.next_run_at.is_none());
    // The refusal is the one thing an operator needs in the history:
    // "why did my task stop?".
    let runs = env.db.list_repeating_task_runs("t1", 10).await.unwrap();
    assert_eq!(
        runs.iter().map(|r| r.status.as_str()).collect::<Vec<_>>(),
        vec!["corrupt_schedule"],
    );
    assert!(
        runs[0]
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("corrupt schedule"),
        "expected the parse error in detail; got {:?}",
        runs[0].detail,
    );
    assert_eq!(runs[0].trigger.as_str(), "scheduler");

    // Second tick: the row is disabled, so it isn't even due any more.
    env.rtm.run_due_tasks(env.run_ctx()).await;
    assert!(
        env.db
            .list_sessions_by_repeating_task("t1")
            .await
            .unwrap()
            .is_empty(),
    );
}

/// Seed a `once` task whose `at` is `days_ahead` days in the future, so
/// it parses cleanly and has a pending `next_run_at` regardless of the
/// host timezone.
async fn seed_once_task(db: &Db, id: &str, folder_id: &str, days_ahead: i64) {
    let ts = chrono::Utc::now().to_rfc3339();
    let at = (chrono::Utc::now() + chrono::Duration::days(days_ahead))
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    let schedule_value = serde_json::json!({ "at": at }).to_string();
    let draft = peckboard::db::models::RepeatingTask {
        id: id.into(),
        name: id.into(),
        description: "".into(),
        folder_id: folder_id.into(),
        prompt: "go".into(),
        schedule_kind: "once".into(),
        schedule_value: schedule_value.clone(),
        model: Some("mock:happy-path".into()),
        effort: None,
        enabled: true,
        next_run_at: None,
        last_run_at: None,
        created_at: ts.clone(),
        updated_at: ts.clone(),
        timezone: None,
    };
    let next = initial_next_run_at(&draft);
    db.create_repeating_task(NewRepeatingTask {
        id: id.into(),
        name: id.into(),
        description: "".into(),
        folder_id: folder_id.into(),
        prompt: "go".into(),
        schedule_kind: "once".into(),
        schedule_value,
        model: Some("mock:happy-path".into()),
        effort: None,
        enabled: true,
        next_run_at: next,
        last_run_at: None,
        created_at: ts.clone(),
        updated_at: ts,
        timezone: None,
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn force_run_refuses_to_refire_a_consumed_once_task() {
    // A `once` schedule is consumed by firing: the dispatch disables the
    // task precisely so "Run now" can't fire it a second time. Force-run
    // passes respect_enabled=false, so that `enabled` flag alone doesn't
    // stop it — the explicit consumed-once guard must.
    let env = fresh_state().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_folder(&env.db, "f1", tmp.path().to_str().unwrap()).await;
    seed_once_task(&env.db, "t1", "f1", 1).await;

    let first = env
        .rtm
        .try_run_now("t1", env.run_ctx(), false)
        .await
        .unwrap();
    assert_eq!(first, StartOutcome::Spawned);
    let sessions = env.db.list_sessions_by_repeating_task("t1").await.unwrap();
    assert_eq!(sessions.len(), 1);
    env.session_manager.cancel_and_wait(&sessions[0].id).await;

    let task = env.db.get_repeating_task("t1").await.unwrap().unwrap();
    assert!(!task.enabled, "a fired once task disables itself");
    assert!(task.last_run_at.is_some());

    let second = env
        .rtm
        .try_run_now("t1", env.run_ctx(), false)
        .await
        .unwrap();
    assert_eq!(
        second,
        StartOutcome::ConsumedOnce,
        "force-run must not re-fire a consumed once task",
    );
    let sessions = env.db.list_sessions_by_repeating_task("t1").await.unwrap();
    assert_eq!(
        sessions.len(),
        1,
        "refused force-run must not spawn a second session; got {:?}",
        sessions.iter().map(|s| &s.id).collect::<Vec<_>>(),
    );
    // The refusal writes its own run-history row, so the view can show
    // why "Run now" did nothing. Rows come back newest-first.
    let runs = env.db.list_repeating_task_runs("t1", 10).await.unwrap();
    assert_eq!(
        runs.iter().map(|r| r.status.as_str()).collect::<Vec<_>>(),
        vec!["consumed_once", "spawned"],
    );
    assert_eq!(runs[0].trigger.as_str(), "manual");
}

#[tokio::test]
async fn force_run_works_again_after_a_once_task_is_re_armed() {
    // Guard against over-refusing: re-arming the task (the update route
    // recomputes next_run_at when the schedule or `enabled` changes) puts
    // it back in a state the scheduler would fire, so force-run must work.
    let env = fresh_state().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_folder(&env.db, "f1", tmp.path().to_str().unwrap()).await;
    seed_once_task(&env.db, "t1", "f1", 1).await;

    let first = env
        .rtm
        .try_run_now("t1", env.run_ctx(), false)
        .await
        .unwrap();
    assert_eq!(first, StartOutcome::Spawned);
    let sessions = env.db.list_sessions_by_repeating_task("t1").await.unwrap();
    env.session_manager.cancel_and_wait(&sessions[0].id).await;

    // Re-arm: enabled back on with a pending occurrence, exactly what the
    // PATCH route writes when the operator edits `at` / flips enabled.
    let re_armed_at = (chrono::Utc::now() + chrono::Duration::days(2)).to_rfc3339();
    env.db
        .update_repeating_task(
            "t1",
            peckboard::db::models::UpdateRepeatingTask {
                enabled: Some(true),
                next_run_at: Some(Some(re_armed_at)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let second = env
        .rtm
        .try_run_now("t1", env.run_ctx(), false)
        .await
        .unwrap();
    assert_eq!(
        second,
        StartOutcome::Spawned,
        "a re-armed once task must still be manually runnable",
    );
    let sessions = env.db.list_sessions_by_repeating_task("t1").await.unwrap();
    assert_eq!(sessions.len(), 2);
    for s in &sessions {
        env.session_manager.cancel_and_wait(&s.id).await;
    }
}

#[tokio::test]
async fn failed_dispatch_reschedules_next_run_at_instead_of_refiring_every_tick() {
    // Regression: a task pinned to a model no registered provider can serve
    // (deleted ollama alias, uninstalled provider, deleted account) used to
    // fail dispatch, record a "failed" run, then return early *before*
    // bumping next_run_at. The row stayed due, so every 30s scheduler tick
    // spawned another crashed session forever -- run history is capped but
    // sessions are not.
    let env = fresh_state().await;
    let tmp = tempfile::tempdir().unwrap();
    seed_folder(&env.db, "f1", tmp.path().to_str().unwrap()).await;
    // "ghost" has no registered provider in this test registry (only
    // "mock" is registered by `fresh_state`), so dispatch fails exactly
    // like a deleted account / uninstalled provider would.
    seed_task(&env.db, "t1", "f1", "go", Some("ghost:nope")).await;
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

    let after_first = env.db.get_repeating_task("t1").await.unwrap().unwrap();
    assert!(
        after_first.last_run_at.is_some(),
        "a failed dispatch must still record last_run_at",
    );
    let next_after_first: chrono::DateTime<chrono::Utc> = after_first
        .next_run_at
        .as_deref()
        .expect("failed dispatch must still advance next_run_at")
        .parse()
        .unwrap();
    assert!(
        next_after_first > chrono::Utc::now(),
        "next_run_at must move to the future after a failed dispatch, not stay due; got {next_after_first}",
    );

    let sessions_after_first = env.db.list_sessions_by_repeating_task("t1").await.unwrap();
    assert_eq!(
        sessions_after_first.len(),
        1,
        "exactly one crashed session should be spawned by the first tick",
    );

    // A second tick immediately after must NOT spawn another session: the
    // task is no longer due (next_run_at is in the future).
    env.rtm.run_due_tasks(env.run_ctx()).await;
    let sessions_after_second = env.db.list_sessions_by_repeating_task("t1").await.unwrap();
    assert_eq!(
        sessions_after_second.len(),
        1,
        "a dead-model task must not refire on the very next scheduler tick",
    );
}

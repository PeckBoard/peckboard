//! End-to-end style test that exercises the agent-provider abstraction
//! through the real dispatcher and the real event-log pipeline, using the
//! mock provider as the agent backend.
//!
//! Goal: prove that a route or worker calling
//! `SessionManager::send_message(..., model = "mock:echo")` results in the
//! expected sequence of `ProviderEvent`s landing in the events table, the
//! conversation id being persisted on the session, and a completion
//! notification being delivered to the orchestrator channel.

use std::sync::Arc;
use std::time::Duration;

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewSession};
use peckboard::provider::claude::register_claude_provider;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::mock::register_mock_provider;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::provider::stream::SpawnConfig;
use peckboard::ws::broadcaster::Broadcaster;

async fn build_dispatcher() -> (SessionManager, Db, Arc<Broadcaster>) {
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
        created_at: ts.clone(),
    })
    .await
    .unwrap();

    db.create_session(NewSession {
        id: "s1".into(),
        name: "S".into(),
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

    (manager, db, broadcaster)
}

#[tokio::test]
async fn mock_echo_flows_through_dispatcher() {
    let (manager, db, broadcaster) = build_dispatcher().await;
    let mut completion_rx = manager
        .take_completion_rx()
        .await
        .expect("completion rx available");

    let config = SpawnConfig {
        model: "mock:echo".into(),
        effort: None,
        working_dir: String::new(),
        mcp_config_path: None,
        env: Default::default(),
        permission_mode: None,
        timeout_ms: None,
        metadata: serde_json::Value::Null,
    };

    manager
        .send_or_queue("s1", "hello mock", &db, &broadcaster, config)
        .await
        .expect("dispatch succeeds");

    // Wait for the completion notification — sized generously to keep
    // CI flake risk near zero. The mock scenario should finish in a
    // few tens of milliseconds.
    let completion = tokio::time::timeout(Duration::from_secs(2), completion_rx.recv())
        .await
        .expect("completion arrived before timeout")
        .expect("channel still open");
    assert_eq!(completion.session_id, "s1");
    assert!(completion.completed, "mock:echo should report success");

    // The event log should now contain the scripted sequence.
    let events = db.events_tail("s1", 100).await.unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        vec!["agent-start", "agent-text", "agent-end"],
        "unexpected event sequence: {kinds:?}",
    );

    let text_event = &events[1];
    let text_data: serde_json::Value = serde_json::from_str(&text_event.data).unwrap();
    assert_eq!(text_data["text"], "hello mock");

    let end_event = &events[2];
    let end_data: serde_json::Value = serde_json::from_str(&end_event.data).unwrap();
    assert_eq!(end_data["status"], "complete");

    // The mock provider sets a synthetic conversation_id on Started /
    // Completed, and the shared `emit_event` helper persists it on the
    // session row — sanity check that.
    let session = db.get_session("s1").await.unwrap().unwrap();
    assert!(
        session
            .conversation_id
            .as_deref()
            .is_some_and(|c| c.starts_with("mock-")),
        "expected synthetic conversation_id, got {:?}",
        session.conversation_id,
    );
}

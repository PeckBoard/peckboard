//! Integration test for the plugin-driven todo/task lifecycle.
//!
//! A real `.wasm` fixture is impractical here: it would need the
//! `wasm32-unknown-unknown` target plus the Extism PDK wired into the test
//! toolchain. So this drives the *host-side* glue directly — the same two
//! steps `emit_plugin_todos` performs once a `todo`-hook plugin reports a
//! snapshot: parse the plugin's `allow` payload via
//! `snapshot_from_plugin_payload`, then persist+broadcast it through the shared
//! `emit_event` path. It proves a plugin can move a work item
//! `Pending -> In Progress -> Done` and that each step surfaces as a canonical
//! `todo` event, normalized identically to the Claude `TodoWrite` path. It also
//! checks the no-plugin fast path is a silent no-op.
//!
//! The only untested seam is the Extism `call` boundary inside
//! `PluginManager::dispatch` itself, which the wasm-fixture caveat above
//! covers; `dispatch_todo` is a thin wrapper over it whose payload-parsing
//! seam (`snapshot_from_plugin_payload`) is exercised here.

use std::sync::Arc;

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewSession};
use peckboard::plugin::manager::PluginManager;
use peckboard::plugin::todo_hook::{emit_plugin_todos, snapshot_from_plugin_payload};
use peckboard::provider::agent::emit_event;
use peckboard::provider::stream::ProviderEvent;
use peckboard::ws::broadcaster::Broadcaster;

async fn seed_session() -> (Db, Arc<Broadcaster>) {
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
        ..Default::default()
    })
    .await
    .unwrap();

    (db, broadcaster)
}

/// The `allow` payload a `todo`-hook plugin returns for one work item at a
/// given provider-native status. Snapshots are replace-all, so a single-item
/// transition is the whole list with that item's status changed.
fn plugin_snapshot(status: &str) -> serde_json::Value {
    serde_json::json!({
        "todos": [
            { "content": "Ship the feature", "status": status, "activeForm": "Shipping the feature" }
        ]
    })
}

/// Mirror what `emit_plugin_todos` does after a plugin reports a snapshot,
/// without needing a live wasm plugin: parse the payload, emit the `todo`
/// event through the shared path.
async fn emit_from_plugin_payload(db: &Db, broadcaster: &Broadcaster, payload: &serde_json::Value) {
    let snapshot =
        snapshot_from_plugin_payload(payload).expect("plugin allow-payload carries a todos array");
    emit_event(
        db,
        broadcaster,
        "s1",
        ProviderEvent::Todo {
            todos: snapshot.todos,
        },
    )
    .await;
}

#[tokio::test]
async fn plugin_drives_pending_in_progress_done_lifecycle() {
    let (db, broadcaster) = seed_session().await;

    // A plugin reports the same item three times as it advances. Note
    // `completed` is a provider-native token that must normalize to `done`.
    for status in ["pending", "in_progress", "completed"] {
        emit_from_plugin_payload(&db, &broadcaster, &plugin_snapshot(status)).await;
    }

    let events = db.events_tail("s1", 100).await.unwrap();
    let todo_kinds: Vec<&str> = events
        .iter()
        .map(|e| e.kind.as_str())
        .filter(|k| *k == "todo")
        .collect();
    assert_eq!(
        todo_kinds.len(),
        3,
        "each plugin snapshot should persist one `todo` event: {:?}",
        events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
    );

    // The single item's status walks the canonical lifecycle across the three
    // snapshots, with the provider-native `completed` normalized to `done`.
    let observed: Vec<String> = events
        .iter()
        .filter(|e| e.kind == "todo")
        .map(|e| {
            let data: serde_json::Value = serde_json::from_str(&e.data).unwrap();
            data["todos"][0]["status"].as_str().unwrap().to_string()
        })
        .collect();
    assert_eq!(observed, vec!["pending", "in_progress", "done"]);

    // The latest-wins read path (what the `/todos` route serves) sees Done.
    let latest = db
        .latest_event_of_kind("s1", "todo")
        .await
        .unwrap()
        .expect("a todo event exists");
    let snapshot: peckboard::todo::TodoSnapshot =
        serde_json::from_str(&latest.data).expect("latest todo event is a TodoSnapshot");
    assert_eq!(snapshot.todos.len(), 1);
    assert_eq!(snapshot.todos[0].status, peckboard::todo::TodoStatus::Done);
    assert_eq!(snapshot.todos[0].content, "Ship the feature");
    assert_eq!(
        snapshot.todos[0].active_form.as_deref(),
        Some("Shipping the feature"),
    );
}

#[tokio::test]
async fn no_plugin_installed_is_a_silent_no_op() {
    let (db, broadcaster) = seed_session().await;

    // No `todo`-hook plugin is loaded, so dispatch short-circuits: nothing is
    // emitted and the call reports it did no work. This is the path every
    // provider hits in the common no-plugin case.
    let empty = PluginManager::empty();
    let emitted = emit_plugin_todos(
        &empty,
        &db,
        &broadcaster,
        "s1",
        serde_json::json!({ "raw": "provider output" }),
    )
    .await;

    assert!(!emitted, "no plugin -> no todo event emitted");
    let todos = db.latest_event_of_kind("s1", "todo").await.unwrap();
    assert!(todos.is_none(), "no todo events should exist");
}

//! The durable half of auth recovery.
//!
//! A session's park state is derived from its event log rather than stored
//! on a column, so it survives a restart and the queue drain can read it
//! with one indexed lookup instead of a transcript scan. These tests pin
//! that derivation, and the queue-side lookup the release scan walks.

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewQueuedMessage, NewSession};
use peckboard::provider::auth_recovery::{AUTH_PARKED_KIND, AUTH_RESUMED_KIND, is_parked};
use serde_json::json;

async fn db_with_sessions(ids: &[&str]) -> Db {
    let db = Db::in_memory().unwrap();
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "F".into(),
        path: "/tmp".into(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    for id in ids {
        db.create_session(NewSession {
            id: (*id).into(),
            name: (*id).into(),
            folder_id: "f1".into(),
            model: Some("claude:claude-opus-4-8@acc_1".into()),
            effort: None,
            is_worker: false,
            project_id: None,
            card_id: None,
            conversation_id: None,
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    }
    db
}

#[tokio::test]
async fn park_state_is_whichever_marker_came_last() {
    let db = db_with_sessions(&["s1"]).await;

    // A session that has never failed to authenticate holds nothing.
    assert!(!is_parked(&db, "s1").await);

    db.append_event(
        "s1",
        AUTH_PARKED_KIND,
        json!({ "model": "claude:claude-opus-4-8@acc_1" }),
    )
    .await
    .unwrap();
    assert!(is_parked(&db, "s1").await);

    db.append_event(
        "s1",
        AUTH_RESUMED_KIND,
        json!({ "trigger": "account-updated" }),
    )
    .await
    .unwrap();
    assert!(!is_parked(&db, "s1").await);

    // The release doesn't stick: if the replay 401s again the turn parks
    // again, and the drain must hold it again rather than spinning.
    db.append_event("s1", AUTH_PARKED_KIND, json!({}))
        .await
        .unwrap();
    assert!(is_parked(&db, "s1").await);

    // Ordinary transcript traffic in between changes nothing.
    db.append_event("s1", "user", json!({ "text": "still there?" }))
        .await
        .unwrap();
    db.append_event("s1", "agent-end", json!({ "status": "complete" }))
        .await
        .unwrap();
    assert!(is_parked(&db, "s1").await);
}

#[tokio::test]
async fn park_state_does_not_leak_between_sessions() {
    let db = db_with_sessions(&["s1", "s2"]).await;
    db.append_event("s1", AUTH_PARKED_KIND, json!({}))
        .await
        .unwrap();
    assert!(is_parked(&db, "s1").await);
    assert!(!is_parked(&db, "s2").await);
}

#[tokio::test]
async fn queued_sessions_lookup_finds_each_holder_once() {
    // The release scan starts from this list, so a session with two parked
    // rows must not be released (and drained) twice.
    let db = db_with_sessions(&["s1", "s2"]).await;
    assert!(db.sessions_with_queued_messages().await.unwrap().is_empty());

    for (session_id, text) in [("s1", "first"), ("s1", "second"), ("s2", "other")] {
        db.enqueue_message(NewQueuedMessage {
            session_id: session_id.into(),
            text: text.into(),
            queued_at: chrono::Utc::now().to_rfc3339(),
            model: Some("claude:claude-opus-4-8@acc_1".into()),
            effort: None,
            attachment_ids: None,
            user_event_appended: true,
        })
        .await
        .unwrap();
    }

    let mut holders = db.sessions_with_queued_messages().await.unwrap();
    holders.sort();
    assert_eq!(holders, vec!["s1".to_string(), "s2".to_string()]);
}

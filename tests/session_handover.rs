//! Integration coverage for the model-switch handover persistence layer:
//! the two new session columns round-trip through `update_session`, and a
//! fresh session starts with them clear. The pure decision/extraction logic
//! is unit-tested in `src/handover.rs`; the full user-visible flow is
//! covered by the Playwright suite (web/e2e).

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewSession, UpdateSession};

async fn seed() -> (Db, String) {
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
    db.create_session(NewSession {
        id: "s1".into(),
        name: "S".into(),
        folder_id: "f1".into(),
        model: Some("claude:opus".into()),
        effort: None,
        is_worker: false,
        created_at: ts.clone(),
        last_activity: ts,
        ..Default::default()
    })
    .await
    .unwrap();
    (db, "s1".into())
}

#[tokio::test]
async fn handover_columns_default_clear() {
    let (db, sid) = seed().await;
    let s = db.get_session(&sid).await.unwrap().unwrap();
    assert_eq!(s.handover_to_model, None);
    assert_eq!(s.pending_handover_doc, None);
}

#[tokio::test]
async fn handover_columns_round_trip() {
    let (db, sid) = seed().await;

    // Park a target (begin_handover shape) — model deliberately unchanged.
    db.update_session(
        &sid,
        UpdateSession {
            handover_to_model: Some(Some("grok:grok-4".into())),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let s = db.get_session(&sid).await.unwrap().unwrap();
    assert_eq!(s.handover_to_model.as_deref(), Some("grok:grok-4"));
    assert_eq!(s.model.as_deref(), Some("claude:opus"));

    // Finalize shape — flip model, clear the flag, stash the doc.
    db.update_session(
        &sid,
        UpdateSession {
            model: Some(Some("grok:grok-4".into())),
            handover_to_model: Some(None),
            pending_handover_doc: Some(Some("## Goal\ncontinue".into())),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let s = db.get_session(&sid).await.unwrap().unwrap();
    assert_eq!(s.model.as_deref(), Some("grok:grok-4"));
    assert_eq!(s.handover_to_model, None);
    assert_eq!(s.pending_handover_doc.as_deref(), Some("## Goal\ncontinue"));

    // Consume shape — clear the doc after injection.
    db.update_session(
        &sid,
        UpdateSession {
            pending_handover_doc: Some(None),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let s = db.get_session(&sid).await.unwrap().unwrap();
    assert_eq!(s.pending_handover_doc, None);
}

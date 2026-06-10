//! Integration tests for the pm_decisions storage layer: the migration,
//! the ensure_schema repair path, and the CRUD lifecycle (pending →
//! answered → superseded). All state lives in an in-memory DB.

use diesel::prelude::*;
use diesel::sql_query;
use diesel::sqlite::SqliteConnection;

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewProject};
use peckboard::db::repair::ensure_schema;

async fn seed_project(db: &Db, project_id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "F".into(),
        path: "/tmp/pm-decisions".into(),
        created_at: ts.clone(),
    })
    .await
    .ok(); // shared across seeds within a test; ignore dupes

    db.create_project(NewProject {
        id: project_id.into(),
        name: "P".into(),
        context: "ctx".into(),
        folder_id: "f1".into(),
        worker_count: 1,
        status: "active".into(),
        workflow: "task".into(),
        model: None,
        effort: None,
        parallel_instructions: false,
        auto_notify_changes: true,
        worker_communication: false,
        created_at: ts.clone(),
        last_accessed_at: ts,
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn table_exists_after_migration() {
    // Db::in_memory() runs every embedded migration; a successful insert
    // proves 1781074848_pm_decisions applied.
    let db = Db::in_memory().unwrap();
    seed_project(&db, "p1").await;
    let d = db
        .create_pending_question("p1", "Ship dark mode?", None)
        .await
        .unwrap();
    assert_eq!(d.status, "pending");
}

#[test]
fn table_exists_after_fresh_ensure_schema_repair_run() {
    // A DB that somehow predates the pm_decisions migration must be
    // healed by ensure_schema. Stub the minimal projects table the other
    // repair steps prod, then run the repair twice (idempotence).
    let mut conn = SqliteConnection::establish(":memory:").unwrap();
    sql_query("CREATE TABLE projects (id TEXT PRIMARY KEY NOT NULL, name TEXT NOT NULL)")
        .execute(&mut conn)
        .unwrap();

    ensure_schema(&mut conn).unwrap();
    ensure_schema(&mut conn).unwrap(); // second run must be a no-op

    sql_query("INSERT INTO projects (id, name) VALUES ('p1', 'P')")
        .execute(&mut conn)
        .unwrap();
    sql_query(
        "INSERT INTO pm_decisions (id, project_id, question, created_at) \
         VALUES ('d1', 'p1', 'q?', '2026-01-01T00:00:00Z')",
    )
    .execute(&mut conn)
    .unwrap();
}

#[tokio::test]
async fn create_pending_then_answer() {
    let db = Db::in_memory().unwrap();
    seed_project(&db, "p1").await;

    let pending = db
        .create_pending_question("p1", "Which DB?", Some("s1"))
        .await
        .unwrap();
    assert_eq!(pending.status, "pending");
    assert_eq!(pending.answer, None);
    assert_eq!(pending.answered_at, None);
    assert_eq!(pending.asked_by_session_id.as_deref(), Some("s1"));

    let answered = db.answer_question(&pending.id, "SQLite").await.unwrap();
    assert_eq!(answered.id, pending.id);
    assert_eq!(answered.status, "answered");
    assert_eq!(answered.answer.as_deref(), Some("SQLite"));
    assert!(answered.answered_at.is_some(), "answered_at must be set");
}

#[tokio::test]
async fn record_decision_lands_answered() {
    let db = Db::in_memory().unwrap();
    seed_project(&db, "p1").await;

    let d = db
        .record_decision("p1", "Tabs or spaces?", "Spaces", None)
        .await
        .unwrap();
    assert_eq!(d.status, "answered");
    assert_eq!(d.answer.as_deref(), Some("Spaces"));
    assert!(d.answered_at.is_some());
    assert_eq!(d.asked_by_session_id, None);

    let answered = db.list_answered_pm_decisions("p1").await.unwrap();
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0].id, d.id);
}

#[tokio::test]
async fn supersede_chains_and_drops_old_row_from_answered_list() {
    let db = Db::in_memory().unwrap();
    seed_project(&db, "p1").await;

    let original = db
        .record_decision("p1", "Deploy target?", "Heroku", None)
        .await
        .unwrap();
    let replacement = db
        .supersede_decision(&original.id, "Deploy target?", "Fly.io")
        .await
        .unwrap();
    assert_eq!(replacement.status, "answered");
    assert_eq!(replacement.answer.as_deref(), Some("Fly.io"));
    assert_eq!(replacement.project_id, "p1");

    let all = db.list_pm_decisions_for_project("p1").await.unwrap();
    assert_eq!(all.len(), 2, "old + replacement rows: {all:?}");
    let old = all.iter().find(|d| d.id == original.id).unwrap();
    assert_eq!(old.status, "superseded");
    assert_eq!(old.superseded_by.as_deref(), Some(replacement.id.as_str()));
    // The original answer is preserved on the superseded row (audit trail).
    assert_eq!(old.answer.as_deref(), Some("Heroku"));

    let answered = db.list_answered_pm_decisions("p1").await.unwrap();
    assert_eq!(answered.len(), 1, "superseded row must drop out");
    assert_eq!(answered[0].id, replacement.id);

    // A pending row can't be superseded — answer it first.
    let pending = db
        .create_pending_question("p1", "CDN?", None)
        .await
        .unwrap();
    let err = db
        .supersede_decision(&pending.id, "CDN?", "Cloudflare")
        .await;
    assert!(err.is_err(), "superseding a pending row must error");
}

#[tokio::test]
async fn answer_question_on_non_pending_row_errors() {
    let db = Db::in_memory().unwrap();
    seed_project(&db, "p1").await;

    let d = db
        .record_decision("p1", "Already decided?", "Yes", None)
        .await
        .unwrap();
    let err = db.answer_question(&d.id, "No").await;
    assert!(err.is_err(), "answering an answered row must error");

    let err = db.answer_question("nope", "answer").await;
    assert!(err.is_err(), "answering a missing row must error");

    // The original answer must be untouched.
    let all = db.list_pm_decisions_for_project("p1").await.unwrap();
    assert_eq!(all[0].answer.as_deref(), Some("Yes"));
}

#[tokio::test]
async fn pending_count_and_list_pending() {
    let db = Db::in_memory().unwrap();
    seed_project(&db, "p1").await;

    assert_eq!(db.pending_pm_decision_count("p1").await.unwrap(), 0);

    let q1 = db.create_pending_question("p1", "Q1?", None).await.unwrap();
    db.create_pending_question("p1", "Q2?", Some("s9"))
        .await
        .unwrap();
    assert_eq!(db.pending_pm_decision_count("p1").await.unwrap(), 2);
    assert_eq!(db.list_pending_pm_decisions("p1").await.unwrap().len(), 2);

    db.answer_question(&q1.id, "A1").await.unwrap();
    assert_eq!(db.pending_pm_decision_count("p1").await.unwrap(), 1);
    let pending = db.list_pending_pm_decisions("p1").await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].question, "Q2?");
}

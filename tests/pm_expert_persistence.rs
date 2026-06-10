//! PM-expert persistence: decision export + rehydration. Mirrors the
//! question-expert persistence design (tests/question_expert_persistence.rs):
//! decisions recorded through the CRUD layer export to a stable report file,
//! and a fresh PM-expert session under the same stable id rehydrates from it
//! (idempotently).

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewProject, Project};
use peckboard::service::pm_expert::{
    PM_DECISIONS_FILE, ensure_project_pm_expert, export_pm_decisions, pm_decisions_folder,
    rehydrate_pm_expert,
};

async fn seed_project(db: &Db, project_id: &str) -> Project {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "F".into(),
        path: "/tmp/pm-expert-persistence".into(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();

    db.create_project(NewProject {
        id: project_id.into(),
        name: "Proj".into(),
        context: "".into(),
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
    .unwrap()
}

/// Index of `needle` within `haystack`, panicking with context when absent.
fn idx(haystack: &str, needle: &str) -> usize {
    haystack
        .find(needle)
        .unwrap_or_else(|| panic!("expected {needle:?} in export:\n{haystack}"))
}

#[tokio::test]
async fn pm_decisions_export_and_rehydration() {
    let db = Db::in_memory().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path();
    let bc = peckboard::ws::broadcaster::Broadcaster::new();

    let project = seed_project(&db, "p1").await;
    let expert = ensure_project_pm_expert(&db, &project).await.unwrap();

    // Bootstrap: export with no decisions still creates the file, and
    // rehydrating a decision-less project is a no-op.
    let (folder, file) = export_pm_decisions(&db, data_dir, "p1").await.unwrap();
    assert_eq!(folder, pm_decisions_folder("p1"));
    assert_eq!(file, PM_DECISIONS_FILE);
    let path = data_dir.join("reports").join(&folder).join(&file);
    assert!(path.exists(), "bootstrap export must create the file");
    assert!(
        !rehydrate_pm_expert(&db, &bc, data_dir, &expert)
            .await
            .unwrap(),
        "rehydrating with no decisions must be a no-op"
    );

    // Record decisions + a pending question via the CRUD layer, then export.
    db.record_decision("p1", "Which DB?", "SQLite", None)
        .await
        .unwrap();
    let deploy = db
        .record_decision("p1", "Deploy target?", "Heroku", None)
        .await
        .unwrap();
    db.create_pending_question("p1", "Ship dark mode?", Some("s1"))
        .await
        .unwrap();
    export_pm_decisions(&db, data_dir, "p1").await.unwrap();

    let raw = std::fs::read_to_string(&path).unwrap();
    // Frontmatter matches the report-reader conventions.
    assert!(raw.starts_with("---\n"));
    assert!(raw.contains("title: \"PM Decision Log (Proj)\""));
    assert!(raw.contains("projectName: \"Proj\""));
    assert!(raw.contains("sessionId: \"pm-expert-project-p1\""));

    // Title Case section headings, with each entry under the right one.
    let answered_at = idx(&raw, "## Answered Decisions");
    let pending_at = idx(&raw, "## Pending Questions");
    assert!(answered_at < pending_at);
    assert!(!raw.contains("## Superseded Decisions"));

    let db_q = idx(&raw, "Which DB?");
    let deploy_q = idx(&raw, "Deploy target?");
    let dark_q = idx(&raw, "Ship dark mode?");
    assert!(answered_at < db_q && db_q < pending_at);
    assert!(answered_at < deploy_q && deploy_q < pending_at);
    assert!(pending_at < dark_q);
    assert!(raw.contains("SQLite"));
    assert!(raw.contains("Heroku"));
    let decided = idx(&raw, &deploy.answered_at.clone().unwrap());
    assert!(answered_at < decided, "answered entries carry their date");

    // Superseding a decision updates the export: the old answer moves under
    // Superseded Decisions with its replacement, the new answer is live.
    db.supersede_decision(&deploy.id, "Deploy target?", "Fly.io")
        .await
        .unwrap();
    export_pm_decisions(&db, data_dir, "p1").await.unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    let answered_at = idx(&raw, "## Answered Decisions");
    let pending_at = idx(&raw, "## Pending Questions");
    let superseded_at = idx(&raw, "## Superseded Decisions");
    let old = idx(&raw, "Heroku");
    let new = idx(&raw, "Fly.io");
    assert!(superseded_at < old, "old answer lives under Superseded");
    assert!(answered_at < new && new < pending_at, "replacement is live");
    assert!(
        raw[old..].contains("Fly.io"),
        "superseded entry names its replacement"
    );

    // Rehydration: a fresh session under the same stable id is seeded with
    // the exported decisions; re-rehydrating the unchanged export no-ops.
    let did = rehydrate_pm_expert(&db, &bc, data_dir, &expert)
        .await
        .unwrap();
    assert!(did, "first rehydration should deliver a bootstrap");
    let again = rehydrate_pm_expert(&db, &bc, data_dir, &expert)
        .await
        .unwrap();
    assert!(
        !again,
        "re-rehydrating the unchanged export must be a no-op"
    );

    let events = db.list_events_by_session(&expert.id, None).await.unwrap();
    let boot = events
        .iter()
        .find(|e| e.kind == "user" && e.data.contains("pm-rehydration"))
        .expect("PM expert should have a rehydration bootstrap event");
    assert!(boot.data.contains("Which DB?"));
    assert!(boot.data.contains("SQLite"));
    assert!(boot.data.contains("Fly.io"));
    assert!(boot.data.contains("Ship dark mode?"));
}

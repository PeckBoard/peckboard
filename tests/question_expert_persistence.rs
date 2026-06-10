//! C7 — Question-expert persistence: Q&A export + rehydration.
//!
//! Proves the locked design end-to-end at the integration layer:
//!   1. Resolved user answers fed to a question-expert are persisted
//!      EAGERLY to durable report files — one GLOBAL export and one per
//!      project.
//!   2. A fresh session under the same stable id rehydrates from its
//!      export (and re-rehydration is idempotent).
//!   3. The exports ARE the user-Q&A export: they list + read through the
//!      real `/api/reports` HTTP surface, for both the global scope and a
//!      project.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewFolder, NewProject, NewUser};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::routes::reports::router as reports_router;
use peckboard::service::push::PushService;
use peckboard::service::question_expert::{
    ensure_global_question_expert, ensure_project_question_expert, record_user_answer,
    rehydrate_question_expert,
};
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;
use serde_json::Value;
use tower::ServiceExt;

async fn build_state() -> (Arc<AppState>, String) {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        port: 0,
        https_port: 0,
        host: "127.0.0.1".into(),
        data_dir: tmp.path().to_path_buf(),
        mdns: false,
    };

    let db = Db::in_memory().unwrap();
    let plugins = Arc::new(PluginManager::new(&config.data_dir));
    let jwt_secret = generate_jwt_secret();
    let provider_registry = Arc::new(ProviderRegistry::new());
    let session_manager = SessionManager::new(provider_registry.clone());
    let push_service = PushService::new(&config.data_dir);

    db.create_user(NewUser {
        id: "u1".into(),
        username: "admin".into(),
        email: None,
        password_hash: "h".into(),
        role: "admin".into(),
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
    })
    .await
    .unwrap();
    let (token, _exp) = create_token(&jwt_secret, "u1", "admin", "as1").unwrap();
    db.create_auth_session(NewAuthSession {
        id: "as1".into(),
        user_id: "u1".into(),
        token_hash: hash_token(&token),
        created_at: 1_000_000,
        expires_at: 1_000_000 + 7 * 24 * 60 * 60,
        user_agent: None,
        ip_address: None,
    })
    .await
    .unwrap();

    let state = Arc::new(AppState {
        config,
        db,
        plugins,
        builtin_plugins: Arc::new(BuiltinPluginRegistry::new()),
        jwt_secret,
        login_limiter: RateLimiter::new(60),
        password_change_limiter: RateLimiter::<String>::new(5),
        broadcaster: Broadcaster::new(),
        provider_registry,
        session_manager,
        repeating_task_manager: peckboard::repeating::RepeatingTaskManager::new(),
        mcp_tokens: peckboard::service::mcp_server::McpTokenRegistry::new(),
        push_service,
        pm_authorizations: Default::default(),
    });

    // Leak the tmp dir so the reports tree survives for the whole test.
    std::mem::forget(tmp);
    (state, token)
}

async fn get_json(state: Arc<AppState>, token: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = reports_router(state.clone())
        .with_state(state)
        .oneshot(req)
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

#[tokio::test]
async fn qa_export_persists_rehydrates_and_lists_through_reports_api() {
    let (state, token) = build_state().await;
    let db = &state.db;
    let data_dir = state.config.data_dir.clone();
    let bc = &state.broadcaster;

    // A project with its own question-expert, plus the global one.
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "F".into(),
        path: "/tmp/peck-qa-f1".into(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    let project = db
        .create_project(NewProject {
            id: "p1".into(),
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
            last_accessed_at: ts.clone(),
        })
        .await
        .unwrap();

    let global_expert = ensure_global_question_expert(db, &data_dir).await.unwrap();
    let project_expert = ensure_project_question_expert(db, &project).await.unwrap();

    // 1. Accumulate Q&A — one global, one project-scoped.
    record_user_answer(
        db,
        bc,
        &data_dir,
        None,
        None,
        "**Which database?**: PostgreSQL",
    )
    .await
    .unwrap();
    record_user_answer(db, bc, &data_dir, None, None, "**Which HTTP port?**: 8080")
        .await
        .unwrap();
    record_user_answer(
        db,
        bc,
        &data_dir,
        None,
        Some("p1"),
        "**Project test command?**: cargo test",
    )
    .await
    .unwrap();

    // The two scopes' exports are distinct and contain only their own Q&A.
    let global_file = data_dir
        .join("reports")
        .join("qa-export-global")
        .join("qa.md");
    let project_file = data_dir
        .join("reports")
        .join("qa-export-project-p1")
        .join("qa.md");
    let global_raw = std::fs::read_to_string(&global_file).unwrap();
    let project_raw = std::fs::read_to_string(&project_file).unwrap();
    assert!(global_raw.contains("PostgreSQL") && global_raw.contains("8080"));
    assert!(!global_raw.contains("cargo test"));
    assert!(project_raw.contains("cargo test"));
    assert!(!project_raw.contains("PostgreSQL"));
    assert!(project_raw.contains("projectName: \"Proj\""));

    // 2. Rehydration — a fresh session under the same stable id picks up
    //    the accumulated Q&A; a second call is a no-op (idempotent).
    let did = rehydrate_question_expert(db, bc, &data_dir, &global_expert)
        .await
        .unwrap();
    assert!(did, "first rehydration should deliver a bootstrap");
    let again = rehydrate_question_expert(db, bc, &data_dir, &global_expert)
        .await
        .unwrap();
    assert!(
        !again,
        "re-rehydrating the unchanged export must be a no-op"
    );

    let events = db
        .list_events_by_session(&global_expert.id, None)
        .await
        .unwrap();
    let boot = events
        .iter()
        .find(|e| e.kind == "user" && e.data.contains("qa-rehydration"))
        .expect("global expert should have a rehydration bootstrap event");
    assert!(boot.data.contains("PostgreSQL"));
    assert!(boot.data.contains("8080"));

    // Per-project rehydration uses the project's own export.
    assert!(
        rehydrate_question_expert(db, bc, &data_dir, &project_expert)
            .await
            .unwrap()
    );
    let p_events = db
        .list_events_by_session(&project_expert.id, None)
        .await
        .unwrap();
    assert!(
        p_events
            .iter()
            .any(|e| e.kind == "user" && e.data.contains("cargo test"))
    );

    // 3. The exports ARE the user-Q&A export: they list + read through the
    //    real reports API, for both scopes.
    let (status, listing) = get_json(state.clone(), &token, "/api/reports").await;
    assert_eq!(status, StatusCode::OK);
    let folders: Vec<&str> = listing["reports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["folder"].as_str().unwrap())
        .collect();
    assert!(
        folders.contains(&"qa-export-global"),
        "global export listed"
    );
    assert!(
        folders.contains(&"qa-export-project-p1"),
        "project export listed"
    );

    let (status, global_doc) =
        get_json(state.clone(), &token, "/api/reports/qa-export-global/qa.md").await;
    assert_eq!(status, StatusCode::OK);
    assert!(global_doc["body"].as_str().unwrap().contains("PostgreSQL"));
    assert_eq!(global_doc["title"], "User Q&A Export (Global)");

    let (status, project_doc) = get_json(
        state.clone(),
        &token,
        "/api/reports/qa-export-project-p1/qa.md",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(project_doc["body"].as_str().unwrap().contains("cargo test"));
    assert_eq!(project_doc["project_name"], "Proj");
}

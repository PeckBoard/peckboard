//! Integration tests for how `handle_worker_done` translates a worker's
//! terminal intent into a card-step transition.
//!
//! Regression coverage for the "completed card stalls in an early step" bug:
//! a worker that does the whole card in one pass must reach `done` (so
//! dependent cards unblock), while `complete_step` must remain a true
//! one-step handoff.
//!
//! All state is built on an in-memory DB; no `claude` CLI is involved.

use std::sync::Arc;

use peckboard::auth::rate_limit::RateLimiter;
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewCard, NewFolder, NewProject, NewSession};
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::mock::register_mock_provider;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::worker::orchestrator::handle_worker_done;
use peckboard::ws::broadcaster::Broadcaster;

async fn build_state() -> Arc<AppState> {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    // Keep the tempdir alive for the duration of the process by leaking it;
    // tests are short-lived and this avoids threading a guard through.
    std::mem::forget(tmp);

    let registry = Arc::new(ProviderRegistry::new());
    register_mock_provider(&registry).await;

    let db = Db::in_memory().unwrap();
    let plugins = Arc::new(PluginManager::new(&data_dir));
    let session_manager = SessionManager::new(registry.clone()).with_plugins(plugins.clone());

    Arc::new(AppState {
        config: Config {
            port: 0,
            https_port: 0,
            host: "127.0.0.1".into(),
            data_dir: data_dir.clone(),
            mdns: false,
        },
        db,
        plugins,
        jwt_secret: vec![0u8; 32],
        login_limiter: RateLimiter::new(100),
        password_change_limiter: RateLimiter::new(100),
        broadcaster: Broadcaster::new(),
        provider_registry: registry,
        session_manager,
        repeating_task_manager: peckboard::repeating::RepeatingTaskManager::new(),
        mcp_tokens: McpTokenRegistry::new(),
        push_service: PushService::new(&data_dir),
    })
}

/// Seed a folder + project + a single card at `step`, plus a worker session
/// assigned to that card. `card_workflow` and `project_default` set the card's
/// `workflow` and the project's `default_workflow` so a test can exercise how
/// `complete_step` resolves the step order. Returns `(card_id, session_id)`.
async fn seed_card_with_worker(
    state: &Arc<AppState>,
    step: &str,
    card_workflow: Option<&str>,
    project_default: Option<&str>,
) -> (String, String) {
    let ts = chrono::Utc::now().to_rfc3339();
    let card_id = uuid::Uuid::new_v4().to_string();
    let session_id = uuid::Uuid::new_v4().to_string();

    state
        .db
        .create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp".into(),
            created_at: ts.clone(),
        })
        .await
        .ok(); // folder is shared across cards in a single test; ignore dupes

    state
        .db
        .create_project(NewProject {
            id: "p1".into(),
            name: "P".into(),
            context: "ctx".into(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            default_workflow: project_default.map(str::to_string),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: true,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts.clone(),
        })
        .await
        .ok();

    state
        .db
        .create_card(NewCard {
            id: card_id.clone(),
            project_id: "p1".into(),
            title: "card".into(),
            description: "desc".into(),
            step: step.into(),
            priority: 1,
            workflow: card_workflow.map(str::to_string),
            model: None,
            effort: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();

    state
        .db
        .create_session(NewSession {
            id: session_id.clone(),
            name: "worker".into(),
            folder_id: "f1".into(),
            model: None,
            effort: None,
            is_worker: true,
            project_id: Some("p1".into()),
            card_id: Some(card_id.clone()),
            conversation_id: None,
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();

    (card_id, session_id)
}

#[tokio::test]
async fn finish_drives_card_to_done_from_any_step() {
    // A worker that calls `finish_card` must land the card on `done`
    // regardless of which step it started on — that's what unblocks
    // dependents.
    for start_step in ["backlog", "in_progress", "review"] {
        let state = build_state().await;
        let (card_id, session_id) =
            seed_card_with_worker(&state, start_step, Some("default"), Some("default")).await;

        state
            .db
            .append_event(
                &session_id,
                "finish-requested",
                serde_json::json!({ "cardId": card_id, "summary": "all done" }),
            )
            .await
            .unwrap();

        handle_worker_done(&state, &session_id).await;

        let card = state.db.get_card(&card_id).await.unwrap().unwrap();
        assert_eq!(
            card.step, "done",
            "finish_card from {start_step} should reach done"
        );
    }
}

#[tokio::test]
async fn complete_step_advances_exactly_one_step() {
    let state = build_state().await;
    let (card_id, session_id) =
        seed_card_with_worker(&state, "backlog", Some("default"), Some("default")).await;

    state
        .db
        .append_event(
            &session_id,
            "complete-step-requested",
            serde_json::json!({ "cardId": card_id, "handoffContext": "did backlog" }),
        )
        .await
        .unwrap();

    handle_worker_done(&state, &session_id).await;

    let card = state.db.get_card(&card_id).await.unwrap().unwrap();
    // backlog → in_progress, NOT all the way to done.
    assert_eq!(card.step, "in_progress");
}

async fn advance_one_step(state: &Arc<AppState>, card_id: &str, session_id: &str) -> String {
    state
        .db
        .append_event(
            session_id,
            "complete-step-requested",
            serde_json::json!({ "cardId": card_id, "handoffContext": "step done" }),
        )
        .await
        .unwrap();
    handle_worker_done(state, session_id).await;
    let card = state.db.get_card(card_id).await.unwrap().unwrap();
    card.step
}

#[tokio::test]
async fn complete_step_follows_the_cards_own_workflow() {
    // Regression: the orchestrator used a hardcoded [backlog, in_progress,
    // review, done] list and ignored the card's workflow. A `research` card
    // (backlog → research → summarize → done) sitting on `research` would not
    // match that list and jump straight to `done`, skipping `summarize`.
    let state = build_state().await;
    let (card_id, session_id) =
        seed_card_with_worker(&state, "research", Some("research"), Some("default")).await;

    let next = advance_one_step(&state, &card_id, &session_id).await;
    assert_eq!(
        next, "summarize",
        "research → summarize, not a skip to done"
    );

    let next = advance_one_step(&state, &card_id, &session_id).await;
    assert_eq!(next, "done", "summarize → done");
}

#[tokio::test]
async fn complete_step_falls_back_to_project_default_workflow() {
    // When the card names no workflow, the project's default_workflow drives
    // the step order.
    let state = build_state().await;
    let (card_id, session_id) = seed_card_with_worker(&state, "design", None, Some("full")).await;

    // full: backlog → design → implement → test → review → done
    let next = advance_one_step(&state, &card_id, &session_id).await;
    assert_eq!(next, "implement");
}

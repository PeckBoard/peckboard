//! Card session history + sealing.
//!
//! - Every worker claim on a card records a `card_sessions` run, closed with
//!   an outcome + summary; a reviewer's `finish_card` also records the
//!   card's review summary and verdict.
//! - A worker session whose card moved on to another session is SEALED, not
//!   deleted: its transcript stays, but no agent can run in it again (HTTP
//!   send answers 409 `session_sealed`, `send_or_queue` refuses) and the
//!   orchestrator never resumes it.
//! - Same-step resume (a backlog detour) still resumes the same session.
//! - Deleting the card removes every session that ever worked it.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{
    NewAuthSession, NewCard, NewFolder, NewProject, NewSession, NewUser, UpdateCard, UpdateSession,
};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::{MidTurnPolicy, SessionManager, is_session_sealed};
use peckboard::provider::message::UserMessage;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::provider::stream::SpawnConfig;
use peckboard::service::mcp_server::{McpTokenRegistry, McpToolRegistry, ToolCallContext};
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::worker::orchestrator::check_and_spawn_workers_at;
use peckboard::ws::broadcaster::Broadcaster;
use tower::ServiceExt;

/// State with the in-process mock provider registered (dispatch succeeds)
/// and an authenticated admin; returns the bearer token.
async fn build_state() -> (Arc<AppState>, String) {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        port: 0,
        https_port: 0,
        host: "127.0.0.1".into(),
        data_dir: tmp.path().to_path_buf(),
        mdns: false,
        keep_alive_hours: 0,
        provider_send_timeout_secs: 300,
    };
    let db = Db::in_memory().unwrap();
    let plugins = Arc::new(PluginManager::new(&config.data_dir, db.clone()));
    let jwt_secret = generate_jwt_secret();
    let provider_registry = Arc::new(ProviderRegistry::new());
    peckboard::provider::test_double::register_mock_provider(&provider_registry).await;
    let session_manager = SessionManager::new(provider_registry.clone());
    let push_service = PushService::new(&config.data_dir);

    let now_secs = chrono::Utc::now().timestamp();
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
        created_at: now_secs,
        expires_at: now_secs + 7 * 24 * 60 * 60,
        user_agent: None,
        ip_address: None,
    })
    .await
    .unwrap();
    std::mem::forget(tmp);

    let state = Arc::new(AppState {
        background: Default::default(),
        plugin_ws_tickets: Default::default(),
        device_registry: Default::default(),
        env_unlock: Arc::new(peckboard::service::env_vars::EnvUnlockRegistry::new()),
        config,
        db,
        plugins,
        builtin_plugins: Arc::new(BuiltinPluginRegistry::new()),
        jwt_secret,
        ssh_vault_key: vec![0u8; 32],
        mfa_vault_key: vec![0u8; 32],
        login_limiter: RateLimiter::new(60),
        password_change_limiter: RateLimiter::<String>::new(5),
        broadcaster: Broadcaster::new(),
        provider_registry,
        session_manager,
        repeating_task_manager: peckboard::repeating::RepeatingTaskManager::new(),
        run_auditor: peckboard::repeating::RunAuditor::new(),
        mcp_tokens: McpTokenRegistry::new(),
        push_service,
        remote_access: peckboard::service::remote_access::RemoteAccess::inert(),
        terminals: peckboard::terminal::TerminalManager::inert(),
        tls: Arc::new(peckboard::state::TlsState::new()),
    });
    (state, token)
}

/// Folder f1 + project p1 (task workflow, review on, mock model) + card c1
/// on `step`.
async fn seed_project(state: &AppState, step: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    let db = &state.db;
    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "F".into(),
        path: "/tmp".into(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_project(NewProject {
        id: "p1".into(),
        name: "P".into(),
        context: "".into(),
        folder_id: "f1".into(),
        worker_count: 1,
        status: "active".into(),
        workflow: "task".into(),
        model: Some("mock:echo".into()),
        effort: Some("low".into()),
        budget_usd_cents: None,
        budget_period: None,
        worktree_isolation: false,
        parallel_instructions: false,
        auto_notify_changes: true,
        worker_communication: false,
        created_at: ts.clone(),
        last_accessed_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_card(NewCard {
        id: "c1".into(),
        project_id: "p1".into(),
        title: "Ship the thing".into(),
        description: "Acceptance: it ships.".into(),
        step: step.into(),
        priority: 1,
        workflow: "task".into(),
        model: None,
        effort: None,
        blocked: false,
        block_reason: None,
        created_at: ts.clone(),
        updated_at: ts,
        system_prompt_name: None,
    })
    .await
    .unwrap();
}

/// Worker session `id` that worked `step` on c1 with a resumable
/// conversation, recorded as the card's last worker (unassigned).
async fn seed_previous_worker(state: &AppState, id: &str, step: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    state
        .db
        .create_session(NewSession {
            id: id.into(),
            name: "worker: Ship the thing".into(),
            folder_id: "f1".into(),
            model: Some("mock:echo".into()),
            is_worker: true,
            project_id: Some("p1".into()),
            card_id: Some("c1".into()),
            conversation_id: Some(format!("conv-{id}")),
            created_at: ts.clone(),
            last_activity: ts,
            worker_step: Some(step.into()),
            ..Default::default()
        })
        .await
        .unwrap();
    state
        .db
        .update_card(
            "c1",
            UpdateCard {
                last_worker_session_id: Some(Some(id.into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
}

async fn tool(
    state: &AppState,
    session_id: &str,
    name: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    let ctx = ToolCallContext {
        session_id: session_id.into(),
        project_id: Some("p1".into()),
        card_id: Some("c1".into()),
        folder_id: "f1".into(),
        db: Arc::new(state.db.clone()),
        broadcaster: state.broadcaster.clone(),
        provider_registry: None,
        data_dir: None,
        device_registry: None,
        background: None,
    };
    McpToolRegistry::new()
        .handle_tool_call(name, args, &ctx)
        .await
        .unwrap()
}

async fn spawn(state: &Arc<AppState>) {
    check_and_spawn_workers_at(state, chrono::Utc::now()).await;
}

async fn current_worker(state: &AppState) -> String {
    state
        .db
        .get_card("c1")
        .await
        .unwrap()
        .unwrap()
        .worker_session_id
        .expect("card should be claimed")
}

async fn wait_sealed(state: &AppState, id: &str) -> bool {
    for _ in 0..200 {
        if let Some(s) = state.db.get_session(id).await.unwrap()
            && s.sealed_at.is_some()
        {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    false
}

async fn request(
    state: &Arc<AppState>,
    token: &str,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    let body = match body {
        Some(b) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let app = peckboard::routes::sessions::router(state.clone())
        .merge(peckboard::routes::projects::router(state.clone()))
        .with_state(state.clone());
    let resp = app.oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn implement_review_done_records_runs_verdict_and_seals_the_implementer() {
    let (state, token) = build_state().await;
    seed_project(&state, "backlog").await;

    // Implementer claims the card (backlog → in_progress) and finishes.
    spawn(&state).await;
    let implementer = current_worker(&state).await;
    let res = tool(
        &state,
        &implementer,
        "finish_card",
        serde_json::json!({ "summary": "built it on feat/ship" }),
    )
    .await;
    assert_eq!(res["to"], "review");

    // Reviewer claims the review step in a fresh session; the implementer,
    // whose card moved on, is sealed (not deleted).
    spawn(&state).await;
    let reviewer = current_worker(&state).await;
    assert_ne!(reviewer, implementer);
    assert!(
        wait_sealed(&state, &implementer).await,
        "implementer must be sealed"
    );
    let sealed = state.db.get_session(&implementer).await.unwrap().unwrap();
    // Its run finished and moved the card on.
    assert_eq!(sealed.sealed_reason.as_deref(), Some("advanced"));

    // A sealed session takes no new turn.
    let err = state
        .session_manager
        .send_or_queue(
            &implementer,
            UserMessage::from_text("one more thing"),
            &state.db,
            &state.broadcaster,
            SpawnConfig {
                model: "mock:echo".into(),
                ..Default::default()
            },
            MidTurnPolicy::Queue,
            false,
        )
        .await
        .unwrap_err();
    assert!(is_session_sealed(&err), "{err}");
    let (status, body) = request(
        &state,
        &token,
        "POST",
        &format!("/api/sessions/{implementer}/message"),
        Some(serde_json::json!({ "text": "hello?" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "session_sealed");
    assert!(
        state
            .db
            .list_queued_messages(&implementer)
            .await
            .unwrap()
            .is_empty()
    );

    // The reviewer files a gap card for the origin card, then finishes
    // without an explicit verdict → changes_requested.
    tool(
        &state,
        &reviewer,
        "create_card",
        serde_json::json!({
            "title": "Gap: docs",
            "description": "Missing docs for origin card Ship the thing (c1).",
        }),
    )
    .await;
    let res = tool(
        &state,
        &reviewer,
        "finish_card",
        serde_json::json!({ "summary": "all good except docs; filed Gap: docs" }),
    )
    .await;
    assert_eq!(res["to"], "done");
    assert_eq!(res["verdict"], "changes_requested");

    let card = state.db.get_card("c1").await.unwrap().unwrap();
    assert_eq!(card.step, "done");
    assert_eq!(card.review_verdict.as_deref(), Some("changes_requested"));
    assert_eq!(
        card.review_summary.as_deref(),
        Some("all good except docs; filed Gap: docs")
    );
    assert!(card.reviewed_at.is_some());
    assert_eq!(card.session_count, 2);
    assert!(card.reviewer_model.is_some(), "reviewer model recorded");
    // A finished review is a dead end too: the reviewer sealed itself.
    let rev = state.db.get_session(&reviewer).await.unwrap().unwrap();
    assert_eq!(rev.sealed_reason.as_deref(), Some("reviewed"));

    // The history route, newest first.
    let (status, body) = request(
        &state,
        &token,
        "GET",
        "/api/projects/p1/cards/c1/sessions",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let runs = body["sessions"].as_array().unwrap();
    assert_eq!(runs.len(), 2, "{body}");
    assert_eq!(runs[0]["session_id"], reviewer.as_str());
    assert_eq!(runs[0]["role"], "review");
    assert_eq!(runs[0]["step"], "review");
    assert_eq!(runs[0]["outcome"], "changes_requested");
    assert_eq!(runs[0]["sealed"], true);
    assert_eq!(runs[0]["session_exists"], true);
    assert!(
        runs[0]["session_name"]
            .as_str()
            .unwrap()
            .starts_with("review: ")
    );
    assert_eq!(runs[1]["session_id"], implementer.as_str());
    assert_eq!(runs[1]["role"], "work");
    assert_eq!(runs[1]["step"], "in_progress");
    assert_eq!(runs[1]["outcome"], "finished");
    assert_eq!(runs[1]["summary"], "built it on feat/ship");
    assert_eq!(runs[1]["sealed"], true);
    assert!(runs[1]["ended_at"].is_string());

    // Deleting the card removes every session that worked it, sealed ones
    // included, plus the run history.
    state.db.delete_card_cascade("c1").await.unwrap();
    assert!(state.db.get_session(&implementer).await.unwrap().is_none());
    assert!(state.db.get_session(&reviewer).await.unwrap().is_none());
    assert!(state.db.list_card_sessions("c1").await.unwrap().is_empty());
}

#[tokio::test]
async fn orchestrator_spawns_fresh_instead_of_resuming_a_sealed_session() {
    let (state, _) = build_state().await;
    seed_project(&state, "in_progress").await;
    seed_previous_worker(&state, "w1", "in_progress").await;
    state
        .db
        .seal_session("w1", Some("superseded"))
        .await
        .unwrap();
    // Even with the resume link restored, sealed means never resumed.
    state
        .db
        .update_session(
            "w1",
            UpdateSession {
                worker_step: Some(Some("in_progress".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    spawn(&state).await;

    let worker = current_worker(&state).await;
    assert_ne!(worker, "w1", "a sealed session must never be resumed");
    let runs = state.db.list_card_sessions("c1").await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].session_id, worker);
}

#[tokio::test]
async fn backlog_detour_resumes_the_same_session_unsealed() {
    let (state, _) = build_state().await;
    // The card was worked on in_progress by w1, then moved to backlog.
    seed_project(&state, "backlog").await;
    seed_previous_worker(&state, "w1", "in_progress").await;

    spawn(&state).await;

    assert_eq!(
        current_worker(&state).await,
        "w1",
        "same-step work resumes w1"
    );
    // Give any (wrongly) spawned seal task a chance to run.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let w1 = state.db.get_session("w1").await.unwrap().unwrap();
    assert!(w1.sealed_at.is_none());
    let runs = state.db.list_card_sessions("c1").await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].session_id, "w1");
    assert!(runs[0].ended_at.is_none());
}

//! Regression tests for the card↔project association on the
//! `/api/projects/:id/cards/:card_id/*` routes.
//!
//! These routes address a card *through* a project. The card id is global,
//! so a handler that loads the card by id alone -- while resolving the
//! project (and its folder) from the URL -- ends up running one project's
//! side effects against another project's card.
//!
//! `retry-merge` is the sharp edge: it runs `merge_worktree` in the *URL*
//! project's folder using the foreign card's id/branch/worktree name. When
//! that worktree doesn't exist there the merge reads as clean, which
//! persists `MergeOutcome::done()` onto the victim card -- wiping its
//! `worktree_unmerged_reason`/`worktree_unmerged_detail` (so the UI's
//! Retry-merge affordance disappears and the real conflict goes invisible)
//! and appending a bogus `worktree-done` event to its worker transcript.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{
    NewAuthSession, NewCard, NewFolder, NewProject, NewSession, NewUser, UpdateCard,
};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::routes::projects::router as projects_router;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;
use serde_json::Value;
use tower::ServiceExt;

/// The victim card lives in project A; every request under test is made
/// against project B's URL.
const PROJECT_A: &str = "proj-a";
const PROJECT_B: &str = "proj-b";
const CARD_A: &str = "card-a";
const WORKER_SESSION_A: &str = "sess-a";

struct Fixture {
    state: Arc<AppState>,
    token: String,
}

async fn build_fixture() -> Fixture {
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
    let session_manager = SessionManager::new(provider_registry.clone());
    let push_service = PushService::new(&config.data_dir);

    let now = chrono::Utc::now().to_rfc3339();
    db.create_user(NewUser {
        id: "u1".into(),
        username: "admin".into(),
        email: None,
        password_hash: "h".into(),
        role: "admin".into(),
        created_at: now.clone(),
        updated_at: now.clone(),
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

    // Two projects, each in its own folder -- the folder is what
    // retry-merge would have run `merge_worktree` against.
    for (folder_id, project_id) in [("folder-a", PROJECT_A), ("folder-b", PROJECT_B)] {
        db.create_folder(NewFolder {
            id: folder_id.into(),
            name: folder_id.into(),
            path: tmp.path().join(folder_id).to_string_lossy().into_owned(),
            created_at: now.clone(),
        })
        .await
        .unwrap();
        db.create_project(NewProject {
            id: project_id.into(),
            name: project_id.into(),
            context: String::new(),
            folder_id: folder_id.into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "task".into(),
            model: None,
            effort: None,
            budget_usd_cents: None,
            budget_period: None,
            worktree_isolation: true,
            parallel_instructions: false,
            auto_notify_changes: true,
            worker_communication: false,
            created_at: now.clone(),
            last_accessed_at: now.clone(),
        })
        .await
        .unwrap();
    }

    // The victim: a card in project A whose worktree is still unmerged,
    // plus the worker transcript a bogus `worktree-done` would land in.
    db.create_card(NewCard {
        id: CARD_A.into(),
        project_id: PROJECT_A.into(),
        title: "victim".into(),
        description: String::new(),
        step: "in_progress".into(),
        priority: 1,
        workflow: "task".into(),
        model: None,
        effort: None,
        blocked: false,
        block_reason: None,
        created_at: now.clone(),
        updated_at: now.clone(),
        system_prompt_name: None,
    })
    .await
    .unwrap();
    db.create_session(NewSession {
        id: WORKER_SESSION_A.into(),
        name: "worker a".into(),
        folder_id: "folder-a".into(),
        model: None,
        effort: None,
        is_worker: true,
        project_id: Some(PROJECT_A.into()),
        card_id: Some(CARD_A.into()),
        conversation_id: None,
        created_at: now.clone(),
        last_activity: now.clone(),
        is_expert: false,
        expert_kind: None,
        knowledge_summary: None,
        knowledge_area: None,
        scope_path: None,
        is_permanent: false,
        repeating_task_id: None,
        system_prompt: None,
        handover_run_id: None,
        handover_to_model: None,
        pending_handover_doc: None,
        worker_step: None,
        user_id: Some("u1".into()),
        context_reset_ts: None,
        system_prompt_name: None,
        model_autoswitch: None,
        is_temp: false,
        parent_session_id: None,
        subagent_completed_at: None,
    })
    .await
    .unwrap();
    db.update_card(
        CARD_A,
        UpdateCard {
            last_worker_session_id: Some(Some(WORKER_SESSION_A.into())),
            worktree_unmerged_reason: Some(Some("conflict".into())),
            worktree_unmerged_detail: Some(Some("CONFLICT (content): merge conflict".into())),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let state = Arc::new(AppState {
        env_unlock: Arc::new(peckboard::service::env_vars::EnvUnlockRegistry::new()),
        config,
        db,
        plugins,
        builtin_plugins: Arc::new(BuiltinPluginRegistry::new()),
        jwt_secret,
        ssh_vault_key: vec![0u8; 32],
        login_limiter: RateLimiter::new(60),
        password_change_limiter: RateLimiter::<String>::new(5),
        broadcaster: Broadcaster::new(),
        provider_registry,
        session_manager,
        repeating_task_manager: peckboard::repeating::RepeatingTaskManager::new(),
        run_auditor: peckboard::repeating::RunAuditor::new(),
        mcp_tokens: McpTokenRegistry::new(),
        push_service,
        tls: Arc::new(peckboard::state::TlsState::new()),
    });
    // The data dir has to outlive the state; the process is short-lived.
    std::mem::forget(tmp);

    Fixture { state, token }
}

async fn call(f: &Fixture, method: Method, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {}", f.token))
        .body(Body::empty())
        .unwrap();
    let resp = projects_router(f.state.clone())
        .with_state(f.state.clone())
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
async fn retry_merge_under_a_foreign_project_is_404_and_touches_nothing() {
    let f = build_fixture().await;

    let (status, body) = call(
        &f,
        Method::POST,
        &format!("/api/projects/{PROJECT_B}/cards/{CARD_A}/retry-merge"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a card in another project must be indistinguishable from a missing one"
    );
    assert_eq!(body["error"], "card not found");

    // The victim's persisted merge state has to be exactly as seeded --
    // pre-fix, `merge_worktree` ran in project B's folder, found no
    // worktree, called it clean and cleared both columns.
    let card = f.state.db.get_card(CARD_A).await.unwrap().unwrap();
    assert_eq!(card.worktree_unmerged_reason.as_deref(), Some("conflict"));
    assert_eq!(
        card.worktree_unmerged_detail.as_deref(),
        Some("CONFLICT (content): merge conflict")
    );

    // ...and no bogus `worktree-done` in its worker transcript.
    let events = f
        .state
        .db
        .list_events_by_session(WORKER_SESSION_A, None)
        .await
        .unwrap();
    assert!(
        !events.iter().any(|e| e.kind == "worktree-done"),
        "cross-project retry-merge must not narrate into the victim's transcript"
    );
}

#[tokio::test]
async fn sibling_card_routes_are_scoped_to_their_project_too() {
    let f = build_fixture().await;

    for (method, suffix) in [
        (Method::DELETE, ""),
        (Method::POST, "/stop"),
        (Method::POST, "/restart"),
        (Method::POST, "/cancel-wont-do"),
        (Method::GET, "/reports"),
    ] {
        let (status, _) = call(
            &f,
            method.clone(),
            &format!("/api/projects/{PROJECT_B}/cards/{CARD_A}{suffix}"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{method} /api/projects/:id/cards/:card_id{suffix} must reject a foreign card"
        );
    }

    // Nothing was deleted or mutated along the way.
    let card = f.state.db.get_card(CARD_A).await.unwrap().unwrap();
    assert_eq!(card.step, "in_progress");
    assert_eq!(card.worktree_unmerged_reason.as_deref(), Some("conflict"));
}

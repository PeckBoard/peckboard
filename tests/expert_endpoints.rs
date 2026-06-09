//! HTTP-level regression tests for the experts API surface (card C2).
//!
//! Two guarantees are locked in here:
//!
//! 1. `GET /api/sessions` must NEVER leak expert sessions into the
//!    ordinary chat list — experts are hidden by design.
//! 2. `GET /api/experts` exposes every expert with the full metadata the
//!    new frontend view needs (expert_kind, knowledge_summary,
//!    knowledge_area, scope_path, project_id, card_id, is_permanent,
//!    last_activity), and honours the optional `?project_id=` filter.
//!
//! The router is driven through `tower::ServiceExt::oneshot` against a
//! real `AppState` so the auth middleware + JSON serialization contract
//! are exercised exactly as a browser would hit them.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewCard, NewFolder, NewProject, NewSession, NewUser};
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::routes::sessions::router;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;
use serde_json::Value;
use tower::ServiceExt;

/// Build a minimal-but-real `AppState` backed by an in-memory DB and a
/// throwaway data dir, plus a bearer token authorised against it.
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

    // Auth: the require_auth middleware validates the JWT signature and
    // then checks the auth_session row keyed by the token's `jti`. Mint a
    // user + auth_session and a matching token so requests pass.
    let now_secs = 1_000_000i64;
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

    let state = Arc::new(AppState {
        config,
        db,
        plugins,
        jwt_secret,
        login_limiter: RateLimiter::new(60),
        password_change_limiter: RateLimiter::<String>::new(5),
        broadcaster: Broadcaster::new(),
        provider_registry,
        session_manager,
        repeating_task_manager: peckboard::repeating::RepeatingTaskManager::new(),
        mcp_tokens: McpTokenRegistry::new(),
        push_service,
    });

    // Keep the tempdir alive for the lifetime of the test by leaking it;
    // the process exits at test end so this is harmless and avoids
    // threading the guard through every call site.
    std::mem::forget(tmp);

    (state, token)
}

/// Seed one folder, one project, one plain chat session, and one
/// project-scoped expert with every metadata field populated.
async fn seed(state: &AppState) {
    let ts = chrono::Utc::now().to_rfc3339();
    state
        .db
        .create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
    state
        .db
        .create_project(NewProject {
            id: "p1".into(),
            name: "P".into(),
            context: "".into(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            default_workflow: None,
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

    state
        .db
        .create_card(NewCard {
            id: "c1".into(),
            project_id: "p1".into(),
            title: "Card".into(),
            description: "".into(),
            step: "backlog".into(),
            priority: 1,
            workflow: None,
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
            id: "plain".into(),
            name: "Chat".into(),
            folder_id: "f1".into(),
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();

    state
        .db
        .create_session(NewSession {
            id: "exp-p1".into(),
            name: "Auth expert".into(),
            folder_id: "f1".into(),
            project_id: Some("p1".into()),
            card_id: Some("c1".into()),
            is_expert: true,
            expert_kind: Some("knowledge".into()),
            knowledge_summary: Some("Knows the auth layer".into()),
            knowledge_area: Some("authentication".into()),
            scope_path: Some("src/auth".into()),
            is_permanent: false,
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();

    // A second expert scoped to a different (global) area, to prove the
    // ?project_id= filter narrows the list.
    state
        .db
        .create_session(NewSession {
            id: "exp-global".into(),
            name: "Question expert".into(),
            folder_id: "f1".into(),
            project_id: None,
            is_expert: true,
            expert_kind: Some("question".into()),
            is_permanent: true,
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
}

/// Used by the experts endpoint, which deliberately did NOT get
/// pagination — expert counts stay small (one per scope, maybe a few
/// dozen total even on a heavily-used instance), so the simple array
/// response is intentional. If we ever paginate experts too, this
/// helper becomes the wrapped-shape one below.
async fn get_json(state: Arc<AppState>, token: &str, uri: &str) -> Vec<Value> {
    let req = Request::builder()
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = router(state.clone())
        .with_state(state)
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "GET {uri} should be 200");
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// `/api/sessions` returns `{items, next_cursor}`; helper unwraps `items`
/// so tests that only care about the row list can stay terse.
async fn get_paged_items(state: Arc<AppState>, token: &str, uri: &str) -> Vec<Value> {
    let req = Request::builder()
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = router(state.clone())
        .with_state(state)
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "GET {uri} should be 200");
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    v["items"]
        .as_array()
        .unwrap_or_else(|| panic!("GET {uri} response missing items array: {v}"))
        .clone()
}

#[tokio::test]
async fn experts_endpoint_lists_experts_and_chat_list_hides_them() {
    let (state, token) = build_state().await;
    seed(&state).await;

    // GET /api/sessions must contain ONLY the plain chat session.
    let sessions = get_paged_items(state.clone(), &token, "/api/sessions").await;
    let session_ids: Vec<&str> = sessions.iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert_eq!(
        session_ids,
        vec!["plain"],
        "chat list must omit experts, got {session_ids:?}"
    );

    // GET /api/experts must contain BOTH experts and never the plain one.
    let experts = get_json(state.clone(), &token, "/api/experts").await;
    let mut expert_ids: Vec<&str> = experts.iter().map(|s| s["id"].as_str().unwrap()).collect();
    expert_ids.sort();
    assert_eq!(expert_ids, vec!["exp-global", "exp-p1"]);

    // The project-scoped expert carries every metadata field the frontend
    // view depends on.
    let exp = experts
        .iter()
        .find(|s| s["id"] == "exp-p1")
        .expect("exp-p1 present");
    assert_eq!(exp["name"], "Auth expert");
    assert_eq!(exp["is_expert"], true);
    assert_eq!(exp["expert_kind"], "knowledge");
    assert_eq!(exp["knowledge_summary"], "Knows the auth layer");
    assert_eq!(exp["knowledge_area"], "authentication");
    assert_eq!(exp["scope_path"], "src/auth");
    assert_eq!(exp["project_id"], "p1");
    assert_eq!(exp["card_id"], "c1");
    assert_eq!(exp["is_permanent"], false);
    assert!(exp["last_activity"].is_string());

    // ?project_id= narrows to a single project's experts (global excluded).
    let scoped = get_json(state.clone(), &token, "/api/experts?project_id=p1").await;
    let scoped_ids: Vec<&str> = scoped.iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert_eq!(scoped_ids, vec!["exp-p1"]);
}

#[tokio::test]
async fn experts_endpoint_requires_auth() {
    let (state, _token) = build_state().await;
    seed(&state).await;

    let req = Request::builder()
        .uri("/api/experts")
        .body(Body::empty())
        .unwrap();
    let resp = router(state.clone())
        .with_state(state)
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "unauthenticated request must be rejected"
    );
}

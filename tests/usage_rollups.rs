//! HTTP-level tests for the usage rollup endpoints.
//!
//! Seeds usage across two projects, two cards, two worker sessions, and two
//! expert sessions, then asserts each of the four rollups
//! (`/api/usage/{sessions,projects,cards,experts}`) attributes tokens to the
//! right entity and prices a non-zero `est_cost`, plus that the
//! single-session breakdown (`/api/usage/sessions/:id`) surfaces the
//! lifetime token + peak-context totals.
//!
//! This locks in the *wire* contract the frontend panels read against — a
//! later refactor that mis-joins a session to the wrong project, sums
//! context instead of taking the peak, or prices tokens at zero would break
//! these.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{
    NewAuthSession, NewCard, NewFolder, NewProject, NewSession, NewUsageEvent, NewUser,
};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::routes::usage::router;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
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
        keep_alive_hours: 0,
    };
    let db = Db::in_memory().unwrap();
    let plugins = Arc::new(PluginManager::new(&config.data_dir, db.clone()));
    let jwt_secret = generate_jwt_secret();
    let provider_registry = Arc::new(ProviderRegistry::new());
    let session_manager = SessionManager::new(provider_registry.clone());
    let push_service = PushService::new(&config.data_dir);

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
        builtin_plugins: Arc::new(BuiltinPluginRegistry::new()),
        jwt_secret,
        login_limiter: RateLimiter::new(60),
        password_change_limiter: RateLimiter::<String>::new(5),
        broadcaster: Broadcaster::new(),
        provider_registry,
        session_manager,
        repeating_task_manager: peckboard::repeating::RepeatingTaskManager::new(),
        run_auditor: peckboard::repeating::RunAuditor::new(),
        mcp_tokens: McpTokenRegistry::new(),
        push_service,
    });
    std::mem::forget(tmp);
    (state, token)
}

#[allow(clippy::too_many_arguments)]
async fn seed_usage(
    db: &Db,
    session_id: &str,
    ts: i64,
    model: &str,
    input: i64,
    output: i64,
    total: i64,
    context: i64,
) {
    db.record_usage_event(NewUsageEvent {
        id: format!("ue-{session_id}-{ts}"),
        session_id: session_id.into(),
        model: Some(model.into()),
        ts,
        input_tokens: input,
        output_tokens: output,
        total_tokens: total,
        context_tokens: context,
        ..Default::default()
    })
    .await
    .unwrap();
}

/// Seed two projects, two cards, two worker sessions, two expert sessions,
/// and usage events for each session. Worker w1 used Opus on project Alpha /
/// card "Card One"; w2 used Sonnet on Beta / "Card Two". Both experts have
/// their own usage. w1 gets two usage rows so the peak-context assertion has
/// something to pick a maximum from.
async fn seed(state: &AppState) {
    let db = &state.db;
    let ts = chrono::Utc::now().to_rfc3339();

    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "F".into(),
        path: "/tmp/f".into(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();

    for (id, name) in [("p1", "Alpha"), ("p2", "Beta")] {
        db.create_project(NewProject {
            id: id.into(),
            name: name.into(),
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
            last_accessed_at: ts.clone(),
        })
        .await
        .unwrap();
    }

    for (id, project, title) in [("c1", "p1", "Card One"), ("c2", "p2", "Card Two")] {
        db.create_card(NewCard {
            id: id.into(),
            project_id: project.into(),
            title: title.into(),
            description: "desc".into(),
            step: "backlog".into(),
            priority: 1,
            workflow: "task".into(),
            model: None,
            effort: None,
            blocked: false,
            block_reason: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();
    }

    // Worker sessions tied to project + card.
    for (id, name, project, card) in [
        ("w1", "Worker One", "p1", "c1"),
        ("w2", "Worker Two", "p2", "c2"),
    ] {
        db.create_session(NewSession {
            id: id.into(),
            name: name.into(),
            folder_id: "f1".into(),
            is_worker: true,
            project_id: Some(project.into()),
            card_id: Some(card.into()),
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    }

    // Expert sessions (not tied to a project/card).
    for (id, name, kind) in [
        ("e1", "Expert One", "knowledge"),
        ("e2", "Expert Two", "question"),
    ] {
        db.create_session(NewSession {
            id: id.into(),
            name: name.into(),
            folder_id: "f1".into(),
            is_expert: true,
            expert_kind: Some(kind.into()),
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    }

    // w1: two Opus turns. Context grows 2000 -> 3000 so MAX picks 3000.
    seed_usage(
        db,
        "w1",
        10,
        "claude:claude-opus-4-8",
        1000,
        400,
        1400,
        2000,
    )
    .await;
    seed_usage(db, "w1", 20, "claude:claude-opus-4-8", 600, 200, 800, 3000).await;
    // w2: one Sonnet turn.
    seed_usage(
        db,
        "w2",
        10,
        "claude:claude-sonnet-4-6",
        300,
        100,
        400,
        1500,
    )
    .await;
    // experts.
    seed_usage(db, "e1", 10, "claude:claude-opus-4-8", 50, 25, 75, 500).await;
    seed_usage(db, "e2", 10, "claude:claude-haiku-4-5", 20, 10, 30, 200).await;
}

async fn get_json(state: Arc<AppState>, token: &str, uri: &str) -> (StatusCode, Value) {
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
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, json)
}

/// Find the entity object with the given `id` in a rollup array.
fn find<'a>(arr: &'a Value, id: &str) -> &'a Value {
    arr.as_array()
        .expect("rollup body is an array")
        .iter()
        .find(|e| e["id"] == id)
        .unwrap_or_else(|| panic!("entity {id} not found in {arr}"))
}

#[tokio::test]
async fn rollups_attribute_tokens_to_the_right_entity_and_price_them() {
    let (state, token) = build_state().await;
    seed(&state).await;

    // ── Sessions ──────────────────────────────────────────────────────
    let (status, sessions) = get_json(state.clone(), &token, "/api/usage/sessions").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(sessions.as_array().unwrap().len(), 4);
    let w1 = find(&sessions, "w1");
    // w1 sums its two Opus turns: input 1000+600, output 400+200.
    assert_eq!(w1["input_tokens"], 1600);
    assert_eq!(w1["output_tokens"], 600);
    assert_eq!(w1["kind"], "session");
    // Peak context, not sum: max(2000, 3000) = 3000.
    assert_eq!(w1["context_tokens"], 3000);
    // SessionUsage extras.
    assert_eq!(w1["total_tokens_used"], 2200); // 1600 + 600 billed slices
    assert_eq!(w1["total_context_tokens"], 3000);
    assert!(w1["est_cost"].as_f64().unwrap() > 0.0);

    // ── Projects ──────────────────────────────────────────────────────
    let (_, projects) = get_json(state.clone(), &token, "/api/usage/projects").await;
    assert_eq!(projects.as_array().unwrap().len(), 2);
    let alpha = find(&projects, "p1");
    assert_eq!(alpha["name"], "Alpha");
    assert_eq!(alpha["kind"], "project");
    assert_eq!(alpha["input_tokens"], 1600); // inherits w1's usage
    assert_eq!(alpha["output_tokens"], 600);
    assert!(alpha["est_cost"].as_f64().unwrap() > 0.0);
    let beta = find(&projects, "p2");
    assert_eq!(beta["input_tokens"], 300); // w2 only
    assert!(beta["est_cost"].as_f64().unwrap() > 0.0);

    // ── Cards ─────────────────────────────────────────────────────────
    let (_, cards) = get_json(state.clone(), &token, "/api/usage/cards").await;
    assert_eq!(cards.as_array().unwrap().len(), 2);
    let card_one = find(&cards, "c1");
    assert_eq!(card_one["name"], "Card One");
    assert_eq!(card_one["kind"], "card");
    assert_eq!(card_one["input_tokens"], 1600);
    assert!(card_one["est_cost"].as_f64().unwrap() > 0.0);
    assert_eq!(find(&cards, "c2")["input_tokens"], 300);

    // ── Experts ───────────────────────────────────────────────────────
    let (_, experts) = get_json(state.clone(), &token, "/api/usage/experts").await;
    assert_eq!(experts.as_array().unwrap().len(), 2);
    let e1 = find(&experts, "e1");
    assert_eq!(e1["name"], "Expert One");
    assert_eq!(e1["kind"], "expert");
    assert_eq!(e1["input_tokens"], 50);
    assert!(e1["est_cost"].as_f64().unwrap() > 0.0);
    assert_eq!(find(&experts, "e2")["input_tokens"], 20);
}

#[tokio::test]
async fn card_and_session_rollups_carry_project_id_projects_do_not() {
    let (state, token) = build_state().await;
    seed(&state).await;

    // Card rows carry their owning project so the cards panel can filter by a
    // selected project: c1 -> p1, c2 -> p2.
    let (_, cards) = get_json(state.clone(), &token, "/api/usage/cards").await;
    assert_eq!(find(&cards, "c1")["project_id"], "p1");
    assert_eq!(find(&cards, "c2")["project_id"], "p2");

    // Session rows carry their owning project (and role flags) so the
    // dashboard can group sessions under per-project pages: w1 -> p1.
    let (_, sessions) = get_json(state.clone(), &token, "/api/usage/sessions").await;
    assert_eq!(find(&sessions, "w1")["project_id"], "p1");
    assert_eq!(find(&sessions, "w1")["is_worker"], true);

    // The project rollup itself has no owning-project meaning, so the field
    // is null there (serialized, since the struct always carries it).
    let (_, projects) = get_json(state.clone(), &token, "/api/usage/projects").await;
    assert!(find(&projects, "p1")["project_id"].is_null());

    // Experts seeded without a project keep a null project_id.
    let (_, experts) = get_json(state.clone(), &token, "/api/usage/experts").await;
    assert!(find(&experts, "e1")["project_id"].is_null());
}

#[tokio::test]
async fn single_session_breakdown_and_unknown_session() {
    let (state, token) = build_state().await;
    seed(&state).await;

    let (status, w1) = get_json(state.clone(), &token, "/api/usage/sessions/w1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(w1["id"], "w1");
    assert_eq!(w1["name"], "Worker One");
    assert_eq!(w1["total_tokens_used"], 2200);
    assert_eq!(w1["total_context_tokens"], 3000);
    assert!(w1["est_cost"].as_f64().unwrap() > 0.0);

    // A session that exists but has no usage rows resolves to zeros, not 404.
    state
        .db
        .create_session(NewSession {
            id: "noUsage".into(),
            name: "Idle".into(),
            folder_id: "f1".into(),
            created_at: chrono::Utc::now().to_rfc3339(),
            last_activity: chrono::Utc::now().to_rfc3339(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (status, empty) = get_json(state.clone(), &token, "/api/usage/sessions/noUsage").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty["name"], "Idle");
    assert_eq!(empty["total_tokens_used"], 0);
    assert_eq!(empty["total_context_tokens"], 0);
    assert_eq!(empty["est_cost"].as_f64().unwrap(), 0.0);

    // Unknown session id → 404.
    let (status, _) = get_json(state.clone(), &token, "/api/usage/sessions/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Every session must have a usage page regardless of type or activity: a
/// session (or expert) with NO usage rows still appears in the listings,
/// with all-zero totals.
#[tokio::test]
async fn zero_usage_sessions_appear_in_listings() {
    let (state, token) = build_state().await;
    seed(&state).await;
    let ts = chrono::Utc::now().to_rfc3339();

    // A never-used chat session and a never-used expert.
    state
        .db
        .create_session(NewSession {
            id: "idle1".into(),
            name: "Idle Chat".into(),
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
            id: "idleE".into(),
            name: "Idle Expert".into(),
            folder_id: "f1".into(),
            is_expert: true,
            expert_kind: Some("knowledge".into()),
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();

    let (status, sessions) = get_json(state.clone(), &token, "/api/usage/sessions").await;
    assert_eq!(status, StatusCode::OK);
    // 4 seeded sessions with usage + the 2 idle ones.
    assert_eq!(sessions.as_array().unwrap().len(), 6);
    let idle = find(&sessions, "idle1");
    assert_eq!(idle["name"], "Idle Chat");
    assert_eq!(idle["input_tokens"], 0);
    assert_eq!(idle["total_tokens_used"], 0);
    assert_eq!(idle["est_cost"], 0.0);
    assert_eq!(idle["is_worker"], false);

    let (_, experts) = get_json(state.clone(), &token, "/api/usage/experts").await;
    assert_eq!(experts.as_array().unwrap().len(), 3);
    let idle_e = find(&experts, "idleE");
    assert_eq!(idle_e["input_tokens"], 0);
    assert_eq!(idle_e["est_cost"], 0.0);
}

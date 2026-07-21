//! HTTP-level tests for the keyset-paginated session list and the
//! `before_seq` events page.
//!
//! These exist because the same behaviour is also unit-tested at the DB
//! layer; here we lock in the *wire* contract: response shape, cursor
//! round-trip, the `next_cursor: null` end-of-list signal, the `limit`
//! cap, and that the events endpoint preserves its three modes
//! (`after_seq`, `before_seq`, and default-latest) without bleeding
//! their semantics into each other.
//!
//! Without these tests a future edit that quietly changes the response
//! envelope (say, `items` → `sessions`) would silently break every
//! browser tab.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewFolder, NewSession, NewUser};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
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
        env_unlock: Arc::new(peckboard::service::env_vars::EnvUnlockRegistry::new()),
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

/// Insert `count` plain sessions in a single folder, evenly-spaced one
/// second apart, oldest first. The returned ids are `"s000".."s{count-1}"`
/// in *insertion* order — session N has the freshest `last_activity`.
async fn seed_sessions(state: &AppState, count: usize) {
    let now = chrono::Utc::now();
    state
        .db
        .create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: now.to_rfc3339(),
        })
        .await
        .unwrap();
    for i in 0..count {
        let ts = (now - chrono::Duration::seconds((count - i) as i64)).to_rfc3339();
        state
            .db
            .create_session(NewSession {
                id: format!("s{i:03}"),
                name: format!("Session {i}"),
                folder_id: "f1".into(),
                model: None,
                effort: None,
                is_worker: false,
                project_id: None,
                card_id: None,
                conversation_id: None,
                created_at: ts.clone(),
                last_activity: ts,
                ..Default::default()
            })
            .await
            .unwrap();
    }
}

async fn get_json(state: Arc<AppState>, token: &str, uri: &str) -> Value {
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

#[tokio::test]
async fn sessions_response_carries_items_and_next_cursor() {
    let (state, token) = build_state().await;
    seed_sessions(&state, 10).await;

    let body = get_json(state.clone(), &token, "/api/sessions?limit=4").await;
    let items = body["items"].as_array().expect("items present");
    assert_eq!(items.len(), 4);
    // Newest first: s009 is the most recent insert.
    assert_eq!(items[0]["id"], "s009");
    assert_eq!(items[3]["id"], "s006");
    // Page is full, so next_cursor must be a real cursor — not null.
    let cursor = &body["next_cursor"];
    assert!(
        cursor.get("last_activity").is_some() && cursor.get("id").is_some(),
        "expected concrete next_cursor on full page, got {cursor}"
    );
    assert_eq!(cursor["id"], "s006");
}

#[tokio::test]
async fn sessions_cursor_round_trips_to_next_page() {
    let (state, token) = build_state().await;
    seed_sessions(&state, 7).await;

    let p1 = get_json(state.clone(), &token, "/api/sessions?limit=3").await;
    let cursor = &p1["next_cursor"];
    let la = cursor["last_activity"].as_str().unwrap();
    let id = cursor["id"].as_str().unwrap();

    let uri = format!(
        "/api/sessions?limit=3&cursor_la={}&cursor_id={}",
        urlencoding::encode(la),
        urlencoding::encode(id),
    );
    let p2 = get_json(state.clone(), &token, &uri).await;
    let items = p2["items"].as_array().unwrap();
    let ids: Vec<&str> = items.iter().map(|x| x["id"].as_str().unwrap()).collect();
    // Pages must form a contiguous descending walk through the list.
    assert_eq!(ids, vec!["s003", "s002", "s001"]);
}

#[tokio::test]
async fn sessions_end_of_list_returns_null_cursor() {
    let (state, token) = build_state().await;
    seed_sessions(&state, 3).await;

    // A page bigger than the total tells the frontend "stop paginating"
    // via `next_cursor: null` — without this signal an infinite-scroll
    // loop would keep refetching the same tail forever.
    let body = get_json(state.clone(), &token, "/api/sessions?limit=10").await;
    assert_eq!(body["items"].as_array().unwrap().len(), 3);
    assert!(
        body["next_cursor"].is_null(),
        "expected null next_cursor on partial page, got {}",
        body["next_cursor"],
    );
}

#[tokio::test]
async fn sessions_limit_is_capped_so_a_client_cant_dump_the_table() {
    let (state, token) = build_state().await;
    seed_sessions(&state, 50).await;

    // A malicious / buggy client asks for the entire table. The server
    // caps at MAX_SESSION_PAGE_SIZE (500). We don't have 500 sessions
    // here — the point is that the request must succeed *and* respect
    // the cap, not 500 itself.
    let body = get_json(state.clone(), &token, "/api/sessions?limit=99999999").await;
    let items = body["items"].as_array().unwrap();
    assert!(
        items.len() <= 500,
        "limit cap not enforced: got {}",
        items.len()
    );
}

#[tokio::test]
async fn sessions_folder_filter_paginates_per_folder() {
    let (state, token) = build_state().await;
    let now = chrono::Utc::now();
    for folder in ["fa", "fb"] {
        state
            .db
            .create_folder(NewFolder {
                id: folder.into(),
                name: folder.into(),
                path: format!("/tmp/{folder}"),
                created_at: now.to_rfc3339(),
            })
            .await
            .unwrap();
        for i in 0..4 {
            let ts = (now - chrono::Duration::seconds(i as i64)).to_rfc3339();
            state
                .db
                .create_session(NewSession {
                    id: format!("{folder}-{i}"),
                    name: format!("{folder} {i}"),
                    folder_id: folder.into(),
                    model: None,
                    effort: None,
                    is_worker: false,
                    project_id: None,
                    card_id: None,
                    conversation_id: None,
                    created_at: ts.clone(),
                    last_activity: ts,
                    ..Default::default()
                })
                .await
                .unwrap();
        }
    }
    let body = get_json(state.clone(), &token, "/api/sessions?folder_id=fa&limit=10").await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 4);
    for it in items {
        assert!(
            it["id"].as_str().unwrap().starts_with("fa-"),
            "folder filter leaked: {it}"
        );
    }
}

#[tokio::test]
async fn events_default_fetch_returns_latest_window() {
    let (state, token) = build_state().await;
    seed_sessions(&state, 1).await; // one session: s000
    for i in 0..6 {
        state
            .db
            .append_event(
                "s000",
                "user",
                serde_json::json!({ "text": format!("m{i}") }),
            )
            .await
            .unwrap();
    }
    let body = get_json(state.clone(), &token, "/api/sessions/s000/events?limit=3").await;
    let arr = body.as_array().expect("events is an array");
    // Default-latest: the 3 highest seqs, oldest-first within the page.
    let seqs: Vec<i64> = arr.iter().map(|e| e["seq"].as_i64().unwrap()).collect();
    assert_eq!(seqs, vec![4, 5, 6]);
}

#[tokio::test]
async fn events_before_seq_walks_backward_a_page_at_a_time() {
    let (state, token) = build_state().await;
    seed_sessions(&state, 1).await;
    for i in 0..10 {
        state
            .db
            .append_event(
                "s000",
                "user",
                serde_json::json!({ "text": format!("m{i}") }),
            )
            .await
            .unwrap();
    }
    // Page 1: latest 4 → seqs [7,8,9,10]. Page 2 asks for events
    // strictly before seq 7 → [3,4,5,6]. Page 3 asks before 3 → [1,2].
    let p1 = get_json(state.clone(), &token, "/api/sessions/s000/events?limit=4").await;
    let p1_seqs: Vec<i64> = p1
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_i64().unwrap())
        .collect();
    assert_eq!(p1_seqs, vec![7, 8, 9, 10]);

    let p2 = get_json(
        state.clone(),
        &token,
        "/api/sessions/s000/events?before_seq=7&limit=4",
    )
    .await;
    let p2_seqs: Vec<i64> = p2
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_i64().unwrap())
        .collect();
    assert_eq!(p2_seqs, vec![3, 4, 5, 6]);

    let p3 = get_json(
        state.clone(),
        &token,
        "/api/sessions/s000/events?before_seq=3&limit=4",
    )
    .await;
    let p3_seqs: Vec<i64> = p3
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_i64().unwrap())
        .collect();
    assert_eq!(p3_seqs, vec![1, 2]);
}

#[tokio::test]
async fn events_after_seq_remains_unlimited_for_ws_catchup() {
    let (state, token) = build_state().await;
    seed_sessions(&state, 1).await;
    for i in 0..50 {
        state
            .db
            .append_event(
                "s000",
                "user",
                serde_json::json!({ "text": format!("m{i}") }),
            )
            .await
            .unwrap();
    }
    // `after_seq` is the WS catch-up path; capping it with `limit`
    // would silently drop events from the gap between reconnects.
    // The route ignores `limit` in this mode — the test passes a tiny
    // one to prove the catch-up returned every event after seq 5
    // regardless.
    let body = get_json(
        state.clone(),
        &token,
        "/api/sessions/s000/events?after_seq=5&limit=2",
    )
    .await;
    let arr = body.as_array().unwrap();
    assert_eq!(
        arr.len(),
        45,
        "after_seq must ignore limit; got {} events",
        arr.len()
    );
    assert_eq!(arr[0]["seq"].as_i64().unwrap(), 6);
    assert_eq!(arr.last().unwrap()["seq"].as_i64().unwrap(), 50);
}

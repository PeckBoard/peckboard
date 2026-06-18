//! HTTP-level tests for `GET /api/usage/trends`.
//!
//! Seeds `usage_events` at deliberately-spaced timestamps across several hour
//! buckets and asserts the endpoint groups them into the right buckets, sums
//! tokens/cost per bucket, orders points monotonically by `bucket_ts`, and
//! keeps each entity's series isolated (a `?entity=session&id=w1` series must
//! not pick up another session's tokens). Also covers day granularity, the
//! operation-kind trends (which reuse the operations endpoint's derivation),
//! auth, the empty-window case, and parameter validation.

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
use peckboard::routes::usage::trends::router;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;
use serde_json::Value;
use tower::ServiceExt;

/// Milliseconds in one hour — the `hour` bucket width, mirrored from the
/// endpoint so the seed timestamps land on known bucket boundaries.
const H: i64 = 3_600_000;

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
async fn seed_usage(db: &Db, session_id: &str, id: &str, ts: i64, model: &str, total: i64) {
    db.record_usage_event(NewUsageEvent {
        id: id.into(),
        session_id: session_id.into(),
        model: Some(model.into()),
        ts,
        // Bill the whole turn as input so `est_cost` is non-zero and `tokens`
        // (the provider roll-up) is exactly `total`.
        input_tokens: total,
        total_tokens: total,
        ..Default::default()
    })
    .await
    .unwrap();
}

/// Seed two projects (Alpha/Beta), a card each, two worker sessions (w1→p1/c1,
/// w2→p2/c2), and usage rows spaced across three hour buckets. w1 has rows in
/// buckets 0, 1, 2; w2 has one large row in bucket 1, used to prove a
/// session-scoped series doesn't leak across sessions but the `overall` series
/// does include it.
async fn seed(db: &Db) {
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

    let opus = "claude:claude-opus-4-8";
    // w1: bucket 0 holds two rows (10 + H/2), buckets 1 and 2 one each.
    seed_usage(db, "w1", "w1-a", 10, opus, 1000).await;
    seed_usage(db, "w1", "w1-b", H / 2, opus, 700).await; // bucket 0 total = 1700
    seed_usage(db, "w1", "w1-c", H, opus, 55).await; // bucket 1
    seed_usage(db, "w1", "w1-d", 2 * H + 7, opus, 440).await; // bucket 2
    // w2: a single large row in bucket 1 — must NOT appear in a w1 series.
    seed_usage(
        db,
        "w2",
        "w2-a",
        H + 100,
        "claude:claude-sonnet-4-6",
        99_999,
    )
    .await;
}

async fn get_json(state: Arc<AppState>, token: Option<&str>, uri: &str) -> (StatusCode, Value) {
    let mut req = Request::builder().uri(uri);
    if let Some(token) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let resp = router(state.clone())
        .with_state(state)
        .oneshot(req.body(Body::empty()).unwrap())
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

/// The series in `body` whose `entity_id` matches.
fn series<'a>(body: &'a Value, entity_id: &str) -> &'a Value {
    body.as_array()
        .expect("trends body is an array")
        .iter()
        .find(|s| s["entity_id"] == entity_id)
        .unwrap_or_else(|| panic!("series {entity_id} not found in {body}"))
}

/// `(bucket_ts, tokens)` pairs of a series, asserting strict ascending
/// `bucket_ts` order along the way.
fn points(series: &Value) -> Vec<(i64, i64)> {
    let pts = series["points"].as_array().expect("points array");
    let mut out = Vec::new();
    let mut prev: Option<i64> = None;
    for p in pts {
        let bucket_ts = p["bucket_ts"].as_i64().unwrap();
        if let Some(prev) = prev {
            assert!(
                bucket_ts > prev,
                "bucket_ts must be strictly ascending, got {prev} then {bucket_ts}"
            );
        }
        prev = Some(bucket_ts);
        assert!(
            p["est_cost"].as_f64().unwrap() > 0.0,
            "every non-empty bucket should price a positive est_cost: {p}"
        );
        out.push((bucket_ts, p["tokens"].as_i64().unwrap()));
    }
    out
}

#[tokio::test]
async fn trends_bucket_usage_by_hour_session_filter_and_overall() {
    let (state, token) = build_state().await;
    seed(&state.db).await;
    let to = (10 * H).to_string();

    // ── Session-scoped: only w1's rows, bucketed by hour. ────────────────
    let (status, body) = get_json(
        state.clone(),
        Some(&token),
        &format!("/api/usage/trends?entity=session&id=w1&bucket=hour&from=0&to={to}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.as_array().unwrap().len(),
        1,
        "one series for one session"
    );
    let w1 = series(&body, "w1");
    assert_eq!(w1["metric"], "tokens");
    // Three buckets: 0 (1000+700), H (55), 2H (440). w2's 99_999 is absent.
    assert_eq!(points(w1), vec![(0, 1700), (H, 55), (2 * H, 440)]);

    // ── Overall: no entity ⇒ a single install-wide series that DOES include
    //    w2's bucket-1 row. ────────────────────────────────────────────────
    let (_, body) = get_json(
        state.clone(),
        Some(&token),
        &format!("/api/usage/trends?bucket=hour&from=0&to={to}"),
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    let overall = series(&body, "overall");
    // Bucket 1 now sums w1 (55) + w2 (99_999).
    assert_eq!(points(overall), vec![(0, 1700), (H, 100_054), (2 * H, 440)]);
}

#[tokio::test]
async fn trends_project_filter_and_multi_series() {
    let (state, token) = build_state().await;
    seed(&state.db).await;
    let to = (10 * H).to_string();

    // Single project: p1 inherits exactly w1's series.
    let (status, body) = get_json(
        state.clone(),
        Some(&token),
        &format!("/api/usage/trends?entity=project&id=p1&from=0&to={to}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "project trends failed: {body}");
    assert_eq!(
        points(series(&body, "p1")),
        vec![(0, 1700), (H, 55), (2 * H, 440)]
    );

    // No id ⇒ one series per project. p2 holds only w2's bucket-1 row.
    let (_, body) = get_json(
        state.clone(),
        Some(&token),
        &format!("/api/usage/trends?entity=project&from=0&to={to}"),
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 2);
    assert_eq!(
        points(series(&body, "p1")),
        vec![(0, 1700), (H, 55), (2 * H, 440)]
    );
    assert_eq!(points(series(&body, "p2")), vec![(H, 99_999)]);
}

#[tokio::test]
async fn trends_day_bucket_collapses_all_hours() {
    let (state, token) = build_state().await;
    seed(&state.db).await;

    // All of w1's rows (ts 10 .. 2H+7) fall inside day-bucket 0.
    let (_, body) = get_json(
        state.clone(),
        Some(&token),
        &format!(
            "/api/usage/trends?entity=session&id=w1&bucket=day&from=0&to={}",
            3 * 86_400_000i64
        ),
    )
    .await;
    // 1700 + 55 + 440 = 2195 in a single day bucket.
    assert_eq!(points(series(&body, "w1")), vec![(0, 2195)]);
}

#[tokio::test]
async fn trends_operation_kind_series() {
    let (state, token) = build_state().await;
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
    db.create_session(NewSession {
        id: "w1".into(),
        name: "Worker".into(),
        folder_id: "f1".into(),
        is_worker: true,
        created_at: ts.clone(),
        last_activity: ts.clone(),
        ..Default::default()
    })
    .await
    .unwrap();

    // An Edit tool call, then the turn that contained it. `append_event`
    // stamps `ts = now`, so seed the usage turn at now + 1h to guarantee the
    // turn's usage ts is at/after the edit event (the operations derivation
    // attributes a turn to events whose ts <= the turn's usage ts).
    db.append_event(
        "w1",
        "agent-tool-start",
        serde_json::json!({"name": "Edit", "toolUseId": "t1", "input": {"file_path": "/src/a.rs"}}),
    )
    .await
    .unwrap();
    let turn_ts = chrono::Utc::now().timestamp_millis() + H;
    seed_usage(db, "w1", "u1", turn_ts, "claude:claude-opus-4-8", 1234).await;

    let (status, body) = get_json(
        state.clone(),
        Some(&token),
        &format!(
            "/api/usage/trends?entity=operation&from=0&to={}",
            turn_ts + H
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let fu = series(&body, "file_update");
    assert_eq!(fu["metric"], "tokens");
    let pts = points(fu); // also asserts est_cost > 0 and ascending order
    assert_eq!(pts.len(), 1, "one file_update bucket");
    assert_eq!(pts[0].0 % H, 0, "bucket_ts is an hour boundary");
}

#[tokio::test]
async fn trends_empty_window_returns_empty_array() {
    let (state, token) = build_state().await;
    seed(&state.db).await;

    // Window ends before the first usage row (ts=10), so nothing matches.
    let (status, body) = get_json(
        state.clone(),
        Some(&token),
        "/api/usage/trends?entity=session&id=w1&from=0&to=5",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.as_array().unwrap().len(),
        0,
        "empty series, not a 404/500"
    );
}

#[tokio::test]
async fn trends_rejects_bad_parameters() {
    let (state, token) = build_state().await;
    for bad in [
        "/api/usage/trends?bucket=minute",
        "/api/usage/trends?metric=latency",
        "/api/usage/trends?entity=teapot",
    ] {
        let (status, _) = get_json(state.clone(), Some(&token), bad).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad} should be a 400");
    }
}

#[tokio::test]
async fn trends_requires_auth() {
    let (state, _token) = build_state().await;
    let (status, _) = get_json(state, None, "/api/usage/trends?entity=session&id=w1").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

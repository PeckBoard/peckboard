//! HTTP-level tests for `GET /api/usage/operations`.
//!
//! Seeds a worker session whose event log + per-turn usage rows contain all
//! three operation kinds — `Edit`/`Write` file updates, an `ask_expert`
//! consultation (ask turn on the worker + reply turn on an expert session),
//! and a `question`/`question-resolved` Q&A — plus a second session in a
//! different project to prove the `session_id` / `project_id` scope filters
//! don't leak. Asserts each kind is tallied with a non-zero `est_cost`, which
//! is the contract the frontend operation-cost panel reads against.
//!
//! Events are seeded with explicit `ts`/`seq` via `create_event` (not
//! `append_event`, which stamps wall-clock time) so each tool/question event
//! lands inside a known turn — mirroring production, where a turn's
//! end-of-turn `agent-usage` row carries a `ts` at or after every event in it.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{
    NewAuthSession, NewCard, NewEvent, NewFolder, NewProject, NewSession, NewUsageEvent, NewUser,
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
use serde_json::{Value, json};
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
async fn ev(db: &Db, id: &str, sid: &str, seq: i32, ts: i64, kind: &str, data: Value) {
    db.create_event(NewEvent {
        id: id.into(),
        session_id: sid.into(),
        seq,
        ts,
        kind: kind.into(),
        data: data.to_string(),
    })
    .await
    .unwrap();
}

async fn usage(db: &Db, id: &str, sid: &str, ts: i64, input: i64, output: i64, total: i64) {
    usage_with_cache(db, id, sid, ts, input, output, 0, total).await;
}

#[allow(clippy::too_many_arguments)]
async fn usage_with_cache(
    db: &Db,
    id: &str,
    sid: &str,
    ts: i64,
    input: i64,
    output: i64,
    cache_read: i64,
    total: i64,
) {
    db.record_usage_event(NewUsageEvent {
        id: id.into(),
        session_id: sid.into(),
        model: Some("claude:claude-opus-4-8".into()),
        ts,
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        total_tokens: total,
        context_tokens: input + cache_read,
        ..Default::default()
    })
    .await
    .unwrap();
}

/// Seed: project p1 / card c1 / worker s1 plus a knowledge expert eK (p1), and
/// a second project p2 / worker s2. s1's log carries an Edit + Write (turn 1),
/// an `ask_expert` ask (turn 2), and a question/resolved pair (turns 3-4); eK
/// carries the matching `ask_expert` reply. s2 carries a lone Edit so the
/// scope filters have something to exclude.
async fn seed(db: &Db) {
    let now = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "F".into(),
        path: "/tmp/f".into(),
        created_at: now.clone(),
    })
    .await
    .unwrap();
    for pid in ["p1", "p2"] {
        db.create_project(NewProject {
            id: pid.into(),
            name: pid.into(),
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
            created_at: now.clone(),
            last_accessed_at: now.clone(),
        })
        .await
        .unwrap();
    }
    for (cid, pid) in [("c1", "p1"), ("c2", "p2")] {
        db.create_card(NewCard {
            id: cid.into(),
            project_id: pid.into(),
            title: cid.into(),
            description: "d".into(),
            step: "backlog".into(),
            priority: 1,
            workflow: "task".into(),
            model: None,
            effort: None,
            blocked: false,
            block_reason: None,
            created_at: now.clone(),
            updated_at: now.clone(),
        })
        .await
        .unwrap();
    }
    for (sid, pid, cid) in [("s1", "p1", "c1"), ("s2", "p2", "c2")] {
        db.create_session(NewSession {
            id: sid.into(),
            name: sid.into(),
            folder_id: "f1".into(),
            is_worker: true,
            project_id: Some(pid.into()),
            card_id: Some(cid.into()),
            created_at: now.clone(),
            last_activity: now.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    }
    db.create_session(NewSession {
        id: "eK".into(),
        name: "Expert".into(),
        folder_id: "f1".into(),
        is_expert: true,
        expert_kind: Some("knowledge".into()),
        project_id: Some("p1".into()),
        created_at: now.clone(),
        last_activity: now.clone(),
        ..Default::default()
    })
    .await
    .unwrap();

    // ── s1, turn 1: the user's prompt, two Reads, an Edit + a Write, then
    //    the turn's usage row (with a cache-read slice the file_read
    //    attribution splits across the two read files). ──────────────────────
    ev(
        db,
        "s1-0",
        "s1",
        1,
        998,
        "user",
        json!({ "text": "fix auth" }),
    )
    .await;
    ev(
        db,
        "s1-r1",
        "s1",
        2,
        999,
        "agent-tool-start",
        json!({ "toolUseId": "tu_r1", "name": "Read", "input": { "file_path": "src/auth.rs" } }),
    )
    .await;
    ev(
        db,
        "s1-r2",
        "s1",
        3,
        1000,
        "agent-tool-start",
        json!({ "toolUseId": "tu_r2", "name": "Read", "input": { "file_path": "docs/README.md" } }),
    )
    .await;
    ev(
        db,
        "s1-1",
        "s1",
        4,
        1001,
        "agent-tool-start",
        json!({ "toolUseId": "tu_1", "name": "Edit", "input": { "file_path": "src/auth.rs" } }),
    )
    .await;
    ev(
        db,
        "s1-2",
        "s1",
        5,
        1002,
        "agent-tool-start",
        json!({ "toolUseId": "tu_2", "name": "Write", "input": { "file_path": "tests/new.rs" } }),
    )
    .await;
    usage_with_cache(db, "u1", "s1", 1010, 1000, 400, 800, 2200).await;

    // ── s1, turn 2: an ask_expert consultation (ask mode). ────────────────
    ev(
        db,
        "s1-3",
        "s1",
        6,
        1100,
        "agent-tool-start",
        json!({
            "toolUseId": "tu_ask",
            "name": "mcp__peckboard__ask_expert",
            "input": { "question": "How is auth wired?", "expert_id": "eK" }
        }),
    )
    .await;
    usage(db, "u2", "s1", 1110, 800, 300, 1100).await;

    // ── eK: the expert's answering turn (ask_expert reply mode). ──────────
    ev(
        db,
        "eK-1",
        "eK",
        1,
        1200,
        "agent-tool-start",
        json!({
            "toolUseId": "tu_reply",
            "name": "mcp__peckboard__ask_expert",
            "input": { "reply_to_session_id": "s1", "answer": "Via JWT middleware." }
        }),
    )
    .await;
    usage(db, "uE", "eK", 1210, 500, 200, 700).await;

    // ── s1, turns 3-4: a question, then its resolution (new turn). ────────
    ev(
        db,
        "q1",
        "s1",
        7,
        1300,
        "question",
        json!({ "questions": [{ "question": "Which port?", "header": "Setup" }], "source": "mcp" }),
    )
    .await;
    usage(db, "u3", "s1", 1310, 200, 50, 250).await;
    ev(
        db,
        "s1-5",
        "s1",
        8,
        1400,
        "question-resolved",
        json!({ "question_id": "q1", "answers": { "0": "8080" } }),
    )
    .await;
    usage(db, "u4", "s1", 1410, 150, 40, 190).await;

    // ── s2 (project p2): a lone Edit + usage, for scope-isolation checks. ──
    ev(
        db,
        "s2-1",
        "s2",
        1,
        1000,
        "agent-tool-start",
        json!({ "toolUseId": "tu_x", "name": "Edit", "input": { "file_path": "src/leak.rs" } }),
    )
    .await;
    usage(db, "u5", "s2", 1010, 100, 20, 120).await;
}

async fn get(state: Arc<AppState>, token: &str, uri: &str) -> (StatusCode, Value) {
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
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

fn labels(arr: &Value) -> Vec<String> {
    arr.as_array()
        .expect("operations body is an array")
        .iter()
        .map(|o| o["label"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn operations_tally_each_kind_with_non_zero_cost() {
    let (state, token) = build_state().await;
    seed(&state.db).await;

    // ── file_update: one row per edited file, each priced. ────────────────
    let (status, files) = get(
        state.clone(),
        &token,
        "/api/usage/operations?kind=file_update",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let file_labels = labels(&files);
    assert!(
        file_labels.contains(&"src/auth.rs".to_string()),
        "{file_labels:?}"
    );
    assert!(
        file_labels.contains(&"tests/new.rs".to_string()),
        "{file_labels:?}"
    );
    for op in files.as_array().unwrap() {
        assert_eq!(op["kind"], "file_update");
        assert!(op["tokens"].as_i64().unwrap() > 0, "{op}");
        assert!(op["est_cost"].as_f64().unwrap() > 0.0, "{op}");
    }

    // ── ask_expert: one consultation, ask turn + reply turn summed. ───────
    let (status, asks) = get(
        state.clone(),
        &token,
        "/api/usage/operations?kind=ask_expert",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let asks = asks.as_array().unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = &asks[0];
    assert_eq!(ask["kind"], "ask_expert");
    assert_eq!(ask["label"], "eK");
    // Asking turn (u2 total 1100) + answering turn (uE total 700) = 1800.
    assert_eq!(ask["tokens"], 1800);
    assert!(ask["est_cost"].as_f64().unwrap() > 0.0, "{ask}");

    // ── qa: one question/answer pair, asking + answer turn. ───────────────
    let (status, qa) = get(state.clone(), &token, "/api/usage/operations?kind=qa").await;
    assert_eq!(status, StatusCode::OK);
    let qa = qa.as_array().unwrap();
    assert_eq!(qa.len(), 1, "{qa:?}");
    assert_eq!(qa[0]["kind"], "qa");
    assert_eq!(qa[0]["label"], "Which port?");
    // u3 (250) + u4 (190) = 440.
    assert_eq!(qa[0]["tokens"], 440);
    assert!(qa[0]["est_cost"].as_f64().unwrap() > 0.0, "{}", qa[0]);
}

#[tokio::test]
async fn scope_filters_do_not_leak_across_sessions_or_projects() {
    let (state, token) = build_state().await;
    seed(&state.db).await;

    // Global sees every edited file, including s2's.
    let (_, all) = get(
        state.clone(),
        &token,
        "/api/usage/operations?kind=file_update",
    )
    .await;
    assert!(labels(&all).contains(&"src/leak.rs".to_string()));

    // session_id=s1 excludes s2's edit.
    let (_, s1) = get(
        state.clone(),
        &token,
        "/api/usage/operations?kind=file_update&session_id=s1",
    )
    .await;
    let s1_labels = labels(&s1);
    assert!(
        s1_labels.contains(&"src/auth.rs".to_string()),
        "{s1_labels:?}"
    );
    assert!(
        !s1_labels.contains(&"src/leak.rs".to_string()),
        "{s1_labels:?}"
    );

    // project_id=p1 also excludes s2 (which lives in p2).
    let (_, p1) = get(
        state.clone(),
        &token,
        "/api/usage/operations?kind=file_update&project_id=p1",
    )
    .await;
    assert!(!labels(&p1).contains(&"src/leak.rs".to_string()));
}

#[tokio::test]
async fn file_read_attributes_cache_read_spend_per_file() {
    let (state, token) = build_state().await;
    seed(&state.db).await;

    let (status, reads) = get(
        state.clone(),
        &token,
        "/api/usage/operations?kind=file_read",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let read_labels = labels(&reads);
    assert!(
        read_labels.contains(&"src/auth.rs".to_string()),
        "{read_labels:?}"
    );
    assert!(
        read_labels.contains(&"docs/README.md".to_string()),
        "{read_labels:?}"
    );
    // Turn 1's 800 cache-read tokens split evenly across the two files read.
    for op in reads.as_array().unwrap() {
        assert_eq!(op["kind"], "file_read");
        assert_eq!(op["tokens"], 400, "{op}");
        assert!(op["est_cost"].as_f64().unwrap() > 0.0, "{op}");
    }

    // Scoped to s2 (which never Read anything): empty.
    let (_, s2) = get(
        state.clone(),
        &token,
        "/api/usage/operations?kind=file_read&session_id=s2",
    )
    .await;
    assert_eq!(s2.as_array().unwrap().len(), 0, "{s2:?}");
}

#[tokio::test]
async fn session_turns_break_down_per_prompt_with_files_read() {
    let (state, token) = build_state().await;
    seed(&state.db).await;

    let (status, turns) = get(state.clone(), &token, "/api/usage/sessions/s1/turns").await;
    assert_eq!(status, StatusCode::OK);
    let turns = turns.as_array().unwrap();
    assert_eq!(turns.len(), 4, "{turns:?}");

    // Turn 1: prompt + the files it read/edited + its cache-read slice.
    let t1 = &turns[0];
    assert_eq!(t1["prompt"], "fix auth");
    assert_eq!(t1["cache_read_tokens"], 800);
    assert_eq!(t1["context_tokens"], 1800);
    assert!(t1["est_cost"].as_f64().unwrap() > 0.0, "{t1}");
    let read: Vec<&str> = t1["files_read"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(read, vec!["src/auth.rs", "docs/README.md"]);
    let edited: Vec<&str> = t1["files_edited"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(edited, vec!["src/auth.rs", "tests/new.rs"]);

    // Later turns had no prompt or file activity.
    let t2 = &turns[1];
    assert_eq!(t2["prompt"], Value::Null);
    assert_eq!(t2["files_read"].as_array().unwrap().len(), 0);

    // Unknown session is a 404.
    let (status, _) = get(state.clone(), &token, "/api/usage/sessions/nope/turns").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn session_rollups_carry_role_flags_and_project() {
    let (state, token) = build_state().await;
    seed(&state.db).await;

    let (status, sessions) = get(state.clone(), &token, "/api/usage/sessions").await;
    assert_eq!(status, StatusCode::OK);
    let arr = sessions.as_array().unwrap();
    let s1 = arr.iter().find(|s| s["id"] == "s1").expect("s1 present");
    assert_eq!(s1["is_worker"], true);
    assert_eq!(s1["is_expert"], false);
    assert_eq!(s1["project_id"], "p1");
    let ek = arr.iter().find(|s| s["id"] == "eK").expect("eK present");
    assert_eq!(ek["is_worker"], false);
    assert_eq!(ek["is_expert"], true);
}

#[tokio::test]
async fn unknown_kind_is_rejected() {
    let (state, token) = build_state().await;
    let (status, _) = get(state, &token, "/api/usage/operations?kind=bogus").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn operations_requires_auth() {
    let (state, _token) = build_state().await;
    let req = Request::builder()
        .uri("/api/usage/operations?kind=file_update")
        .body(Body::empty())
        .unwrap();
    let resp = router(state.clone())
        .with_state(state)
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// A turn that used several models is several `usage_events` rows sharing a
/// `turn_seq` (one per model); the turns endpoint must fold them into ONE
/// turn with summed slices, the peak context, and a per-model breakdown —
/// not render phantom turns.
#[tokio::test]
async fn multi_model_turn_folds_into_one_turn_with_breakdown() {
    let (state, token) = build_state().await;
    let db = &state.db;
    let now = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: "f9".into(),
        name: "F9".into(),
        path: "/tmp/f9".into(),
        created_at: now.clone(),
    })
    .await
    .unwrap();
    db.create_session(NewSession {
        id: "s9".into(),
        name: "multi-model".into(),
        folder_id: "f9".into(),
        created_at: now.clone(),
        last_activity: now.clone(),
        ..Default::default()
    })
    .await
    .unwrap();

    // Turn 1: opus main loop + a haiku subagent, same turn_seq.
    for (id, model, input, output, cache_read, context) in [
        ("u1", "claude-opus-4-8", 100i64, 200i64, 5000i64, 5100i64),
        ("u2", "claude-haiku-4-5", 40, 10, 0, 0),
    ] {
        db.record_usage_event(NewUsageEvent {
            id: id.into(),
            session_id: "s9".into(),
            model: Some(model.into()),
            turn_seq: Some(1),
            ts: 100,
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            total_tokens: input + output + cache_read,
            context_tokens: context,
            ..Default::default()
        })
        .await
        .unwrap();
    }
    // Turn 2: single-model row, auto turn boundary.
    db.record_usage_event(NewUsageEvent {
        id: "u3".into(),
        session_id: "s9".into(),
        model: Some("claude-opus-4-8".into()),
        turn_seq: Some(2),
        ts: 200,
        input_tokens: 10,
        output_tokens: 20,
        total_tokens: 30,
        context_tokens: 5200,
        ..Default::default()
    })
    .await
    .unwrap();

    let (status, turns) = get(state.clone(), &token, "/api/usage/sessions/s9/turns").await;
    assert_eq!(status, StatusCode::OK);
    let turns = turns.as_array().unwrap();
    assert_eq!(turns.len(), 2, "two turns, not three rows: {turns:?}");

    let t1 = &turns[0];
    assert_eq!(t1["turn_seq"], 1);
    assert_eq!(t1["input_tokens"], 140); // 100 + 40 summed across models
    assert_eq!(t1["output_tokens"], 210);
    assert_eq!(t1["cache_read_tokens"], 5000);
    assert_eq!(t1["context_tokens"], 5100); // peak, from the main-model row
    assert_eq!(t1["model"], "claude-opus-4-8"); // context-carrying row wins
    let models = t1["models"].as_array().unwrap();
    assert_eq!(models.len(), 2, "per-model breakdown present");
    assert!(t1["est_cost"].as_f64().unwrap() > 0.0);

    let t2 = &turns[1];
    assert_eq!(t2["turn_seq"], 2);
    assert_eq!(t2["input_tokens"], 10);
    assert_eq!(
        t2["models"].as_array().unwrap().len(),
        0,
        "single-model turn has no breakdown"
    );
}

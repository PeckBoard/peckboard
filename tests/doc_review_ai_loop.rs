//! The review tools are review-sessions-only, end to end over `/mcp`.
//!
//! `get_review_doc` / `submit_review_revision` sit in BOTH hidden-tool lists
//! (worker and chat), and are re-admitted for sessions whose `expert_kind`
//! is `doc-review`. Two things have to hold for that to be safe, and both
//! are checked here against the real Axum route rather than the name lists:
//! an ordinary worker/chat session never sees the tools, and calling one by
//! name from such a session is refused rather than silently operating on
//! someone else's document.

use std::net::SocketAddr;
use std::sync::Arc;

use peckboard::auth::rate_limit::RateLimiter;
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewSession};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::service::doc_reviews::EXPERT_KIND;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;

async fn build_state() -> Arc<AppState> {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    std::mem::forget(tmp);

    let registry = Arc::new(ProviderRegistry::new());
    let db = Db::in_memory().unwrap();
    let plugins = Arc::new(PluginManager::new(&data_dir, db.clone()));
    let session_manager = SessionManager::new(registry.clone()).with_plugins(plugins.clone());

    Arc::new(AppState {
        env_unlock: Arc::new(peckboard::service::env_vars::EnvUnlockRegistry::new()),
        config: Config {
            port: 0,
            https_port: 0,
            host: "127.0.0.1".into(),
            data_dir,
            mdns: false,
            keep_alive_hours: 0,
            provider_send_timeout_secs: 300,
        },
        db,
        plugins,
        builtin_plugins: Arc::new(BuiltinPluginRegistry::new()),
        jwt_secret: vec![0u8; 32],
        ssh_vault_key: vec![0u8; 32],
        mfa_vault_key: vec![0u8; 32],
        login_limiter: RateLimiter::new(100),
        password_change_limiter: RateLimiter::new(100),
        broadcaster: Broadcaster::new(),
        provider_registry: registry,
        session_manager,
        repeating_task_manager: peckboard::repeating::RepeatingTaskManager::new(),
        run_auditor: peckboard::repeating::RunAuditor::new(),
        mcp_tokens: McpTokenRegistry::new(),
        tls: Arc::new(peckboard::state::TlsState::new()),
        push_service: PushService::new(&std::env::temp_dir()),
    })
}

struct Fixture {
    url: String,
    state: Arc<AppState>,
    /// Bearer tokens for the review session, a worker, and a plain chat.
    review_token: String,
    worker_token: String,
    chat_token: String,
    review_id: String,
    comment_id: String,
}

/// Boot `/mcp` over loopback with three sessions: the review's own session
/// (bound to a review with one open annotation), a worker, and a chat.
async fn fixture() -> Fixture {
    let state = build_state().await;
    let ts = chrono::Utc::now().to_rfc3339();

    state
        .db
        .create_folder(NewFolder {
            id: "f1".into(),
            name: "Folder".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
    let session = |id: &str, is_worker: bool, expert_kind: Option<&str>| NewSession {
        id: id.into(),
        name: id.into(),
        folder_id: "f1".into(),
        is_worker,
        is_expert: expert_kind.is_some(),
        expert_kind: expert_kind.map(str::to_string),
        created_at: ts.clone(),
        last_activity: ts.clone(),
        ..Default::default()
    };
    for s in [
        session("review", false, Some(EXPERT_KIND)),
        session("worker", true, None),
        session("chat", false, None),
    ] {
        state.db.create_session(s).await.unwrap();
    }

    let review = state
        .db
        .create_doc_review(
            "Launch plan",
            "file",
            "f1:docs/launch.md",
            Some("f1"),
            None,
            "# Launch\n\nShip on Tuesday.\n",
        )
        .await
        .unwrap();
    state
        .db
        .set_doc_review_session(&review.id, Some("review"))
        .await
        .unwrap();
    let comment = state
        .db
        .add_doc_review_comment(
            &review.id,
            1,
            (3, 3),
            Some("Ship on Tuesday."),
            "wrong",
            "it is Thursday",
        )
        .await
        .unwrap();

    let review_token = state.mcp_tokens.issue_token("review".into(), None).await;
    let worker_token = state.mcp_tokens.issue_token("worker".into(), None).await;
    let chat_token = state.mcp_tokens.issue_token("chat".into(), None).await;

    let app = peckboard::routes::mcp::router(state.clone()).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    Fixture {
        url: format!("http://{addr}/mcp"),
        state,
        review_token,
        worker_token,
        chat_token,
        review_id: review.id,
        comment_id: comment.id,
    }
}

async fn rpc(url: &str, token: &str, method: &str, params: serde_json::Value) -> serde_json::Value {
    reqwest::Client::new()
        .post(url)
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn tool_names(url: &str, token: &str) -> Vec<String> {
    let body = rpc(url, token, "tools/list", serde_json::json!({})).await;
    body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn only_a_review_session_is_offered_the_review_tools() {
    let f = fixture().await;

    let review = tool_names(&f.url, &f.review_token).await;
    for tool in ["get_review_doc", "submit_review_revision"] {
        assert!(
            review.iter().any(|n| n == tool),
            "the review session must see {tool}: {review:?}"
        );
    }
    // It is still an ordinary chat otherwise — the review tools are added,
    // not swapped in for everything else.
    assert!(review.iter().any(|n| n == "ask_user"), "{review:?}");

    for (label, token) in [("worker", &f.worker_token), ("chat", &f.chat_token)] {
        let names = tool_names(&f.url, token).await;
        for tool in ["get_review_doc", "submit_review_revision"] {
            assert!(
                !names.iter().any(|n| n == tool),
                "a {label} session must not see {tool}: {names:?}"
            );
        }
    }
}

#[tokio::test]
async fn a_review_session_reads_and_revises_its_own_document() {
    let f = fixture().await;

    let body = rpc(
        &f.url,
        &f.review_token,
        "tools/call",
        serde_json::json!({ "name": "get_review_doc", "arguments": {} }),
    )
    .await;
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let doc: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(doc["review_id"], f.review_id);
    assert_eq!(doc["version"], 1);
    assert_eq!(doc["markdown"], "# Launch\n\nShip on Tuesday.\n");
    assert_eq!(doc["open_comments"][0]["id"], f.comment_id);
    assert_eq!(doc["open_comments"][0]["kind"], "wrong");

    let body = rpc(
        &f.url,
        &f.review_token,
        "tools/call",
        serde_json::json!({
            "name": "submit_review_revision",
            "arguments": {
                "markdown": "# Launch\n\nShip on Thursday.\n",
                "note": "fixed the ship day",
                "resolutions": [
                    { "comment_id": f.comment_id, "action": "fixed", "note": "Tuesday → Thursday" }
                ],
            },
        }),
    )
    .await;
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let out: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(out["version"], 2, "got: {out}");

    let review = f
        .state
        .db
        .get_doc_review(&f.review_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(review.current_version, 2);
    assert_eq!(review.status, "annotating");
    let head = f
        .state
        .db
        .get_doc_review_version(&f.review_id, 2)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(head.markdown, "# Launch\n\nShip on Thursday.\n");
    assert_eq!(head.created_by, "assistant");
    let comments = f
        .state
        .db
        .list_doc_review_comments(&f.review_id, false)
        .await
        .unwrap();
    assert_eq!(comments[0].status, "fixed");
}

#[tokio::test]
async fn calling_the_review_tools_by_name_from_a_chat_is_refused() {
    let f = fixture().await;

    for name in ["get_review_doc", "submit_review_revision"] {
        let body = rpc(
            &f.url,
            &f.chat_token,
            "tools/call",
            serde_json::json!({
                "name": name,
                "arguments": { "markdown": "# Hijacked\n", "note": "n" },
            }),
        )
        .await;
        // Tool-level failure, surfaced as an error result rather than a
        // transport error — what the model sees and can react to.
        let rendered = body.to_string();
        assert!(
            rendered.contains("not bound to a document review"),
            "{name} must refuse a non-review session: {rendered}"
        );
    }

    let review = f
        .state
        .db
        .get_doc_review(&f.review_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(review.current_version, 1, "the document was not touched");
}

/// A clarifying question parks the review, and the answer resumes it —
/// driven through the real seams (`ask_user` over `/mcp`, then the shared
/// question resolver the HTTP route and the plugin host both call).
#[tokio::test]
async fn asking_and_answering_a_question_walks_the_review_status() {
    let f = fixture().await;
    f.state
        .db
        .set_doc_review_status(&f.review_id, "running")
        .await
        .unwrap();

    let body = rpc(
        &f.url,
        &f.review_token,
        "tools/call",
        serde_json::json!({
            "name": "ask_user",
            "arguments": {
                "questions": [{
                    "question": "Ship Thursday or next Tuesday?",
                    "header": "Clarify",
                }],
            },
        }),
    )
    .await;
    assert!(body["result"].is_object(), "got: {body}");
    let status = async || {
        f.state
            .db
            .get_doc_review(&f.review_id)
            .await
            .unwrap()
            .unwrap()
            .status
    };
    assert_eq!(status().await, "needs_input");

    let question_id = f
        .state
        .db
        .list_events_by_session("review", None)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "question")
        .expect("a question event")
        .id;
    peckboard::service::questions::resolve_question(
        f.state.clone(),
        "u1".into(),
        "review".into(),
        serde_json::json!({ "question_id": question_id, "answers": { "0": "Thursday" } }),
    )
    .await
    .unwrap();
    assert_eq!(status().await, "running", "the answer resumes the pass");
}

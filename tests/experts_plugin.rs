//! End-to-end test of the **experts WASM plugin** against the real core host
//! functions. Loads the actual compiled `peckboard_experts_plugin.wasm`,
//! approves it, and drives its three MCP tools (`spin_up_experts` /
//! `list_experts` / `ask_expert`) through `PluginManager::invoke_mcp_tool`,
//! exercising the full platform built in Phase B: the `mcp.tool.invoke`
//! dispatch, the trusted invocation context, and the session / project-file /
//! dispatch host functions.
//!
//! The wasm is built out-of-tree (`peck-plugins/experts/build.sh`) and this
//! repo's `cargo test` has no `wasm32` toolchain, so the test **skips** with a
//! note when the artifact is absent — it validates locally (and in any CI that
//! pre-builds the plugin) without breaking the default `cargo test`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewProject, NewSession};
use peckboard::plugin::host::LiveHost;
use peckboard::plugin::manager::PluginManager;
use serde_json::{Value, json};

/// Path to the out-of-tree compiled plugin, or `None` if it hasn't been built.
fn experts_wasm() -> Option<PathBuf> {
    let p =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../peck-plugins/experts/dist/plugin.wasm");
    p.exists().then_some(p)
}

/// Records the live dispatch/resume calls the plugin makes, so the test can
/// assert `ask_expert` actually delivered and stays scoped.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<String>>,
}
impl LiveHost for Recorder {
    fn dispatch_capture(&self, session_id: String, _prompt: String) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("dispatch:{session_id}"));
    }
    fn resume_session(&self, session_id: String, _text: String) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("resume:{session_id}"));
    }
}

#[tokio::test]
async fn experts_plugin_drives_all_three_tools_end_to_end() {
    let Some(wasm) = experts_wasm() else {
        eprintln!(
            "SKIP experts_plugin_drives_all_three_tools_end_to_end: plugin wasm not built \
             (run peck-plugins/experts/build.sh)"
        );
        return;
    };

    // A data dir with the plugin dropped into <data>/plugins/, plus a real
    // project directory the file host functions can walk.
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path();
    let plugins_dir = data_dir.join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::copy(&wasm, plugins_dir.join("peckboard_experts_plugin.wasm")).unwrap();

    // A small, multi-directory codebase so partitioning produces ≥1 expert.
    let repo = data_dir.join("repo");
    for (rel, body) in [
        ("src/auth/login.rs", "fn login() {}"),
        ("src/auth/token.rs", "fn token() {}"),
        ("src/ws/socket.rs", "fn socket() {}"),
        ("web/app.ts", "export const x = 1;"),
        ("README.md", "# repo"),
    ] {
        let path = repo.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    let db = Db::open(data_dir).unwrap();
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "Repo".into(),
        path: repo.to_string_lossy().to_string(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_project(NewProject {
        id: "p1".into(),
        name: "Proj".into(),
        context: String::new(),
        folder_id: "f1".into(),
        worker_count: 1,
        status: "active".into(),
        workflow: "task".into(),
        model: None,
        effort: None,
        parallel_instructions: false,
        auto_notify_changes: false,
        worker_communication: false,
        created_at: ts.clone(),
        last_accessed_at: ts,
    })
    .await
    .unwrap();

    // The asking session, in the same scope — so a reply (resume_session back
    // to it) is visible to the expert. Mirrors a real worker/chat caller.
    db.create_session(NewSession {
        id: "caller-1".into(),
        name: "Caller".into(),
        folder_id: "f1".into(),
        project_id: Some("p1".into()),
        is_worker: true,
        created_at: chrono::Utc::now().to_rfc3339(),
        last_activity: chrono::Utc::now().to_rfc3339(),
        ..Default::default()
    })
    .await
    .unwrap();

    let plugins = PluginManager::new(data_dir, db.clone());
    plugins.load_all().await.unwrap();
    let recorder = Arc::new(Recorder::default());
    plugins.set_live_host(recorder.clone());

    // Approve the plugin (computes the canonical grant fingerprint and runs
    // `init`, flipping it active) — the same path the operator's approval takes.
    let info = plugins
        .decide("peckboard_experts_plugin", true)
        .await
        .unwrap()
        .expect("experts plugin should be loaded");
    assert_eq!(info.status, "approved", "plugin must be active: {info:?}");

    // The trusted caller context core would build from the verified session.
    let ctx = json!({ "sessionId": "caller-1", "projectId": "p1", "folderId": "f1" });

    // ── spin_up_experts: partition the repo, create knowledge experts ──
    let res = invoke(
        &plugins,
        "spin_up_experts",
        json!({ "max_experts": 2 }),
        &ctx,
    )
    .await;
    let experts = res["experts"]
        .as_array()
        .unwrap_or_else(|| panic!("spin_up_experts returned no experts array: {res}"));
    assert!(!experts.is_empty(), "expected ≥1 expert, got {res}");
    // Every created expert covers some of the repo's files.
    assert!(
        experts
            .iter()
            .all(|e| e["files"].as_u64().unwrap_or(0) >= 1)
    );
    // Each knowledge expert fired a capture run through the live host, PLUS the
    // durable question expert fired one priming run (its answer-only role setup).
    let captures = recorder
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.starts_with("dispatch:"))
        .count();
    assert_eq!(
        captures,
        experts.len() + 1,
        "every knowledge expert captures, plus one question-expert priming run"
    );

    // ── list_experts: the experts just created come back ──
    let res = invoke(&plugins, "list_experts", json!({}), &ctx).await;
    let listed = res["experts"].as_array().unwrap();
    // spin_up creates the knowledge experts PLUS the durable question + PM experts.
    assert_eq!(
        listed.len(),
        experts.len() + 2,
        "list_experts mismatch: {res}"
    );
    let kind_count = |k: &str| listed.iter().filter(|e| e["expert_kind"] == k).count();
    assert_eq!(kind_count("knowledge"), experts.len());
    assert_eq!(kind_count("question"), 1, "one durable question expert");
    assert_eq!(kind_count("pm"), 1, "one durable PM expert");
    let pm_id = listed.iter().find(|e| e["expert_kind"] == "pm").unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let question_id = listed
        .iter()
        .find(|e| e["expert_kind"] == "question")
        .unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    // The question expert was primed with its answer-only role on creation
    // (re-homing core's old question-expert system prompt).
    assert!(
        recorder
            .calls
            .lock()
            .unwrap()
            .contains(&format!("dispatch:{question_id}")),
        "question expert must be primed with its answer-only role on creation"
    );

    // ── ask_expert (ask mode): deliver a question to a knowledge expert ──
    let target = listed
        .iter()
        .find(|e| e["expert_kind"] == "knowledge")
        .unwrap()["session_id"]
        .as_str()
        .unwrap();
    let res = invoke(
        &plugins,
        "ask_expert",
        json!({ "expert_id": target, "question": "How does auth work?" }),
        &ctx,
    )
    .await;
    assert_eq!(res["delivered"], true, "ask_expert did not deliver: {res}");
    assert_eq!(res["expert_id"], target);
    assert!(
        recorder
            .calls
            .lock()
            .unwrap()
            .contains(&format!("resume:{target}")),
        "ask_expert should resume the target expert"
    );

    // ── ask_expert (reply mode): an expert answers back to the asker ──
    let res = invoke(
        &plugins,
        "ask_expert",
        json!({ "answer": "It uses tokens.", "reply_to_session_id": "caller-1" }),
        &json!({ "sessionId": target, "projectId": "p1", "folderId": "f1" }),
    )
    .await;
    assert_eq!(res["delivered"], true, "reply mode did not deliver: {res}");

    // ── a session the plugin doesn't own is refused (scope safety) ──
    let res = invoke(
        &plugins,
        "ask_expert",
        json!({ "expert_id": "not-an-expert", "question": "hi" }),
        &ctx,
    )
    .await;
    assert!(
        res.get("error").is_some(),
        "consulting a non-expert id must error, got {res}"
    );

    // ── pm_record_decision (worker ADD) + pm_check_decisions round-trip ──
    // Exercises the plugin's PM decision store through the real host store_*
    // functions (data_store permission granted via approval).
    let res = invoke(
        &plugins,
        "pm_record_decision",
        json!({ "title": "Auth model", "decision": "Use signed JWTs, 15-min expiry." }),
        &ctx,
    )
    .await;
    assert_eq!(res["decision"]["title"], "Auth model", "pm_record: {res}");
    let decision_id = res["decision"]["id"].as_str().unwrap().to_string();

    let res = invoke(
        &plugins,
        "pm_check_decisions",
        json!({ "planned_change": "switch auth to sessions" }),
        &ctx,
    )
    .await;
    let decisions = res["decisions"].as_array().unwrap();
    assert!(
        decisions.iter().any(|d| d["id"] == decision_id.as_str()),
        "pm_check should return the recorded decision: {res}"
    );

    // A non-PM-expert worker may NOT supersede — authorization gate holds.
    let res = invoke(
        &plugins,
        "pm_record_decision",
        json!({
            "title": "Auth model v2",
            "decision": "Switch to opaque tokens.",
            "supersedes_decision_id": decision_id,
        }),
        &ctx,
    )
    .await;
    assert!(
        res.get("error")
            .and_then(|e| e.as_str())
            .is_some_and(|e| e.contains("PM expert")),
        "a worker superseding a decision must be refused: {res}"
    );

    // ── The authenticated bridge end to end: escalate → answer → supersede ──
    // 1) The PM expert escalates a question to the user (→ a pending decision).
    let pm_ctx = json!({ "sessionId": pm_id, "projectId": "p1", "folderId": "f1" });
    let res = invoke(
        &plugins,
        "pm_escalate_to_user",
        json!({ "question": "Ship v2 auth now or next quarter?" }),
        &pm_ctx,
    )
    .await;
    let pending_id = res["pending_id"].as_str().unwrap().to_string();

    // 2) The authenticated UI lists the board for the project (user authority).
    let board = invoke_authed(
        &plugins,
        "GET",
        "/api/plugin-ui/pm/decisions",
        "project_id=p1",
        "",
        &json!({}),
    )
    .await;
    assert_eq!(
        board["pending"].as_array().unwrap().len(),
        1,
        "board: {board}"
    );
    // The experts list endpoint returns every expert (user sees all).
    let ex = invoke_authed(
        &plugins,
        "GET",
        "/api/plugin-ui/experts",
        "",
        "",
        &json!({}),
    )
    .await;
    assert!(
        ex["experts"].as_array().unwrap().len() >= 3,
        "experts: {ex}"
    );

    // 3) The user answers the pending question via the authed endpoint — this
    //    marks it answered, ISSUES a supersession grant, and resumes the PM
    //    expert (the resume goes through the live host).
    let resumes_before = recorder
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| **c == format!("resume:{pm_id}"))
        .count();
    let ans = invoke_authed(
        &plugins,
        "POST",
        "/api/plugin-ui/pm/answer",
        "",
        &json!({ "project_id": "p1", "question_id": pending_id, "answer": "Ship next quarter." })
            .to_string(),
        &json!({}),
    )
    .await;
    assert_eq!(
        ans["pending_count"], 0,
        "answering clears the pending count: {ans}"
    );
    assert_eq!(
        recorder
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| **c == format!("resume:{pm_id}"))
            .count(),
        resumes_before + 1,
        "answering must resume the PM expert"
    );

    // 4) The PM expert may now supersede (it holds the issued authorization).
    let res = invoke(
        &plugins,
        "pm_record_decision",
        json!({
            "title": "Auth model v2",
            "decision": "Opaque tokens, shipping next quarter.",
            "supersedes_decision_id": decision_id,
        }),
        &pm_ctx,
    )
    .await;
    assert_eq!(
        res["decision"]["title"], "Auth model v2",
        "PM expert with a grant must supersede: {res}"
    );

    // ── session.user.answer: the question expert learns from a user answer ──
    // Core fires this notification (under the answering user's authority) when a
    // user answers a worker's ask_user question. The plugin must deliver the Q&A
    // to the project's question expert via resume_session — re-homing the feed
    // that used to live in core's `record_user_answer`.
    let q_resumes_before = recorder
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| **c == format!("resume:{question_id}"))
        .count();
    plugins
        .dispatch_authed(
            peckboard::plugin::hooks::USER_ANSWER_HOOK,
            "admin",
            json!({
                "asker_session_id": "caller-1",
                "project_id": "p1",
                "qa_text": "**Which datastore should workers use?**: Postgres.",
            }),
        )
        .await;
    assert_eq!(
        recorder
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| **c == format!("resume:{question_id}"))
            .count(),
        q_resumes_before + 1,
        "a user answer must feed the project's question expert"
    );
}

/// Invoke a plugin tool and unwrap its `Allow` payload (the tool result Value).
async fn invoke(plugins: &PluginManager, tool: &str, args: Value, ctx: &Value) -> Value {
    plugins
        .invoke_mcp_tool(tool, args, ctx.clone())
        .await
        .unwrap_or_else(|| panic!("no active plugin claimed tool '{tool}'"))
        .unwrap_or_else(|e| panic!("tool '{tool}' failed: {e}"))
}

/// Drive an authenticated `/api/plugin-ui/*` request through the bridge (as user
/// "admin") and return the parsed JSON body.
async fn invoke_authed(
    plugins: &PluginManager,
    method: &str,
    path: &str,
    query: &str,
    body: &str,
    _unused: &Value,
) -> Value {
    use peckboard::plugin::hooks::PluginHttpOutcome;
    let headers = std::collections::BTreeMap::new();
    match plugins
        .serve_http_authed("admin", method, path, query, &headers, body)
        .await
    {
        PluginHttpOutcome::Served { status, body, .. } => {
            assert!(
                (200..300).contains(&status),
                "{method} {path} -> {status}: {body}",
                body = String::from_utf8_lossy(&body)
            );
            serde_json::from_slice(&body)
                .unwrap_or_else(|e| panic!("{method} {path} returned non-JSON: {e}"))
        }
        PluginHttpOutcome::NoRoute => panic!("no plugin route for {method} {path}"),
    }
}

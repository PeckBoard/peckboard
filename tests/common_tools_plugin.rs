//! End-to-end test of the **common-tools WASM plugin** against the real core
//! host functions. Loads the actual compiled
//! `peckboard_common_tools_plugin.wasm`, approves it, and drives its MCP tools
//! through `PluginManager::invoke_mcp_tool` — exercising the `mcp.tool.invoke`
//! dispatch plus the host capabilities added for it: the project-file host
//! functions (`search_files` / `list_files` / `read_file`), the SSRF-contained
//! `peckboard_http_fetch` (`fetch_web`), and the allowlisted `peckboard_exec`
//! (`git`).
//!
//! The wasm is built out-of-tree (`peck-plugins/common-tools/build.sh`) and
//! this repo's `cargo test` has no `wasm32` toolchain, so the test **skips**
//! with a note when the artifact is absent — it validates locally (and in any
//! CI that pre-builds the plugin) without breaking the default `cargo test`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewProject, NewSession};
use peckboard::plugin::host::LiveHost;
use peckboard::plugin::manager::PluginManager;
use serde_json::{Value, json};

const PLUGIN_ID: &str = "peckboard_common_tools_plugin";

/// A `LiveHost` that records the `ask_user` prompts `run_command` emits, so the
/// test can drive the two-step approval: it captures `(session_id, token)`, and
/// the test then emits the matching `question` + `question-resolved` events the
/// real `AppLiveHost` / answer route would produce.
#[derive(Default)]
struct ApprovalRecorder {
    asks: Mutex<Vec<(String, String)>>,
}
impl LiveHost for ApprovalRecorder {
    fn dispatch_capture(&self, _session_id: String, _prompt: String) {}
    fn resume_session(&self, _session_id: String, _text: String) {}
    fn ask_user(
        &self,
        session_id: String,
        _question: String,
        _options: Vec<String>,
        token: String,
    ) {
        self.asks.lock().unwrap().push((session_id, token));
    }
}

/// Path to the out-of-tree compiled plugin, or `None` if it hasn't been built.
fn plugin_wasm() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "../peck-plugins/common-tools/target/wasm32-unknown-unknown/release/\
         peckboard_common_tools_plugin.wasm",
    );
    p.exists().then_some(p)
}

#[tokio::test]
async fn common_tools_plugin_drives_tools_end_to_end() {
    let Some(wasm) = plugin_wasm() else {
        eprintln!(
            "SKIP common_tools_plugin_drives_tools_end_to_end: plugin wasm not built \
             (run peck-plugins/common-tools/build.sh)"
        );
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path();
    let plugins_dir = data_dir.join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::copy(&wasm, plugins_dir.join(format!("{PLUGIN_ID}.wasm"))).unwrap();

    // A small project the file host functions can walk and search.
    let repo = data_dir.join("repo");
    for (rel, body) in [
        ("Cargo.toml", "[package]\nname = \"demo\"\n"),
        (
            "src/main.rs",
            "fn main() {\n    println!(\"hello needle\");\n}\n",
        ),
        ("README.md", "# demo repo\n"),
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
        last_accessed_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_session(NewSession {
        id: "caller-1".into(),
        name: "Caller".into(),
        folder_id: "f1".into(),
        project_id: Some("p1".into()),
        is_worker: true,
        created_at: ts.clone(),
        last_activity: ts,
        ..Default::default()
    })
    .await
    .unwrap();

    let plugins = PluginManager::new(data_dir, db.clone());
    plugins.load_all().await.unwrap();
    let recorder = Arc::new(ApprovalRecorder::default());
    plugins.set_live_host(recorder.clone());
    let info = plugins
        .decide(PLUGIN_ID, true)
        .await
        .unwrap()
        .expect("common-tools plugin should be loaded");
    assert_eq!(info.status, "approved", "plugin must be active: {info:?}");

    let ctx = json!({ "sessionId": "caller-1", "projectId": "p1", "folderId": "f1" });

    // ── math: pure compute, no host calls ──
    let res = invoke(
        &plugins,
        "math",
        json!({ "expression": "sqrt(16) + 2^3" }),
        &ctx,
    )
    .await;
    assert_eq!(res["result"], json!(12.0), "math: {res}");

    // ── list_files: walks the caller's folder ──
    let res = invoke(&plugins, "list_files", json!({}), &ctx).await;
    let paths: Vec<&str> = res["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["path"].as_str())
        .collect();
    assert!(paths.contains(&"src/main.rs"), "list_files: {res}");
    assert!(paths.contains(&"Cargo.toml"), "list_files: {res}");

    // ── read_file: a line window from a file under the folder ──
    let res = invoke(
        &plugins,
        "read_file",
        json!({ "path": "src/main.rs", "start_line": 1, "line_count": 1 }),
        &ctx,
    )
    .await;
    assert!(
        res["content"].as_str().unwrap().contains("fn main"),
        "read_file: {res}"
    );

    // ── search_files: literal content search across the folder ──
    let res = invoke(&plugins, "search_files", json!({ "query": "needle" }), &ctx).await;
    assert_eq!(res["match_count"], json!(1), "search_files: {res}");
    assert_eq!(res["matches"][0]["path"], json!("src/main.rs"));

    // ── fetch_web: the SSRF guard rejects a loopback target (no network) ──
    let err = try_invoke(
        &plugins,
        "fetch_web",
        json!({ "url": "http://127.0.0.1:1/" }),
        &ctx,
    )
    .await
    .expect_err("fetching loopback must be refused");
    assert!(
        err.contains("public address") || err.contains("dns resolution"),
        "fetch_web SSRF guard: {err}"
    );

    // ── git: a mutating subcommand is refused plugin-side ──
    let err = try_invoke(&plugins, "git", json!({ "subcommand": "push" }), &ctx)
        .await
        .expect_err("mutating git subcommand must be refused");
    assert!(err.contains("not permitted"), "git allowlist: {err}");

    // ── git status: runs through peckboard_exec in the folder (when git is
    // installed and the dir is a repo). ──
    if git_init(&repo) {
        let res = invoke(&plugins, "git", json!({ "subcommand": "status" }), &ctx).await;
        assert_eq!(res["exit_code"], json!(0), "git status: {res}");
        assert_eq!(res["timed_out"], json!(false), "git status: {res}");
        assert!(
            res["command"].as_str().unwrap().starts_with("git status"),
            "git status command echo: {res}"
        );
    } else {
        eprintln!("note: git not available; skipped the git-status exec assertion");
    }

    // ── write_file: write a new file, then read it back ──
    let res = invoke(
        &plugins,
        "write_file",
        json!({ "path": "out/generated.txt", "content": "written by plugin" }),
        &ctx,
    )
    .await;
    assert_eq!(res["ok"], json!(true), "write_file: {res}");
    assert_eq!(
        std::fs::read_to_string(repo.join("out/generated.txt")).unwrap(),
        "written by plugin"
    );
    let res = invoke(
        &plugins,
        "read_file",
        json!({ "path": "out/generated.txt" }),
        &ctx,
    )
    .await;
    assert!(
        res["content"]
            .as_str()
            .unwrap()
            .contains("written by plugin"),
        "read back: {res}"
    );

    // ── file_outline: deterministic symbol parse + content hash ──
    let res = invoke(
        &plugins,
        "file_outline",
        json!({ "path": "src/main.rs" }),
        &ctx,
    )
    .await;
    assert_eq!(res["language"], json!("rust"), "file_outline: {res}");
    let main_sym = res["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == json!("main"))
        .unwrap_or_else(|| panic!("fn main not in outline: {res}"));
    assert_eq!(main_sym["kind"], json!("fn"), "file_outline: {res}");
    assert_eq!(main_sym["start_line"], json!(1), "file_outline: {res}");
    assert_eq!(main_sym["end_line"], json!(3), "file_outline: {res}");
    let hash = res["hash"]
        .as_str()
        .expect("outline returns a hash")
        .to_string();

    // read_file reports the same whole-file hash.
    let res = invoke(
        &plugins,
        "read_file",
        json!({ "path": "src/main.rs" }),
        &ctx,
    )
    .await;
    assert_eq!(res["hash"], json!(hash), "read_file hash: {res}");

    // ── read_symbol: just one function's body, not the whole file ──
    let res = invoke(
        &plugins,
        "read_symbol",
        json!({ "path": "src/main.rs", "name": "main" }),
        &ctx,
    )
    .await;
    assert_eq!(res["hash"], json!(hash), "read_symbol hash: {res}");
    let body = res["symbols"][0]["content"].as_str().unwrap();
    assert!(body.contains("hello needle"), "read_symbol body: {res}");

    // ── edit_file: hash-guarded positional edit ──
    // A stale/wrong hash is rejected before anything is written.
    let err = try_invoke(
        &plugins,
        "edit_file",
        json!({
            "path": "src/main.rs",
            "original_hash": "0000000000000000",
            "edits": [{ "op": "delete", "start_line": 1, "end_line": 1 }],
        }),
        &ctx,
    )
    .await
    .expect_err("a wrong hash must be rejected");
    assert!(err.contains("hash mismatch"), "edit_file guard: {err}");

    // With the right hash the edit applies and the new hash comes back.
    let res = invoke(
        &plugins,
        "edit_file",
        json!({
            "path": "src/main.rs",
            "original_hash": hash,
            "edits": [{
                "op": "update", "start_line": 2, "end_line": 2,
                "text": "    println!(\"hello edited\");",
            }],
        }),
        &ctx,
    )
    .await;
    assert_eq!(res["ok"], json!(true), "edit_file: {res}");
    let new_hash = res["hash"]
        .as_str()
        .expect("edit returns new hash")
        .to_string();
    assert_ne!(new_hash, hash, "hash must change after an edit");
    assert_eq!(
        std::fs::read_to_string(repo.join("src/main.rs")).unwrap(),
        "fn main() {\n    println!(\"hello edited\");\n}\n"
    );

    // The pre-edit hash is now stale — the guard catches the lost update.
    let err = try_invoke(
        &plugins,
        "edit_file",
        json!({
            "path": "src/main.rs",
            "original_hash": hash,
            "edits": [{ "op": "delete", "start_line": 2, "end_line": 2 }],
        }),
        &ctx,
    )
    .await
    .expect_err("the stale hash must be rejected");
    assert!(err.contains("hash mismatch"), "stale hash: {err}");

    // write_file's returned hash chains straight into edit_file.
    let res = invoke(
        &plugins,
        "write_file",
        json!({ "path": "out/notes.txt", "content": "alpha\nbeta\n" }),
        &ctx,
    )
    .await;
    let wh = res["hash"]
        .as_str()
        .expect("write_file returns hash")
        .to_string();
    let res = invoke(
        &plugins,
        "edit_file",
        json!({
            "path": "out/notes.txt",
            "original_hash": wh,
            "edits": [{ "op": "insert", "line": 2, "text": "middle" }],
        }),
        &ctx,
    )
    .await;
    assert_eq!(res["ok"], json!(true), "edit after write: {res}");
    assert_eq!(
        std::fs::read_to_string(repo.join("out/notes.txt")).unwrap(),
        "alpha\nmiddle\nbeta\n"
    );

    // ── run_command: the full two-step interactive approval ──
    // First call: a non-allowlisted command → the plugin asks the user and
    // returns awaiting_approval (nothing is executed yet).
    let res = invoke(
        &plugins,
        "run_command",
        json!({ "command": "echo", "args": ["approved-hi"] }),
        &ctx,
    )
    .await;
    assert_eq!(
        res["status"],
        json!("awaiting_approval"),
        "first call: {res}"
    );

    // The plugin emitted exactly one prompt for our session; grab its token.
    let (sess, token) = {
        let asks = recorder.asks.lock().unwrap();
        assert_eq!(asks.len(), 1, "expected one approval prompt: {asks:?}");
        asks[0].clone()
    };
    assert_eq!(sess, "caller-1");

    // Re-call before answering → still awaiting (and no second prompt).
    let res = invoke(
        &plugins,
        "run_command",
        json!({ "command": "echo", "args": ["approved-hi"] }),
        &ctx,
    )
    .await;
    assert_eq!(
        res["status"],
        json!("awaiting_approval"),
        "still pending: {res}"
    );
    assert_eq!(
        recorder.asks.lock().unwrap().len(),
        1,
        "no re-prompt while pending"
    );

    // Simulate the real ask/answer surface: the question event the AppLiveHost
    // would have written (carrying the token), then the user's answer.
    let q_event = db
        .append_event(
            "caller-1",
            "question",
            json!({
                "approval_token": token,
                "questions": [{ "question": "Approve running echo?" }],
            }),
        )
        .await
        .unwrap();
    db.append_event(
        "caller-1",
        "question-resolved",
        json!({ "question_id": q_event.id, "answers": { "0": "Approve once" } }),
    )
    .await
    .unwrap();

    // Re-call after approval → the plugin reads the real answer and runs it.
    let res = invoke(
        &plugins,
        "run_command",
        json!({ "command": "echo", "args": ["approved-hi"] }),
        &ctx,
    )
    .await;
    assert_eq!(
        res["approved_via"],
        json!("approved_once"),
        "ran after approval: {res}"
    );
    assert_eq!(res["exit_code"], json!(0), "echo exit: {res}");
    assert!(
        res["stdout"].as_str().unwrap().contains("approved-hi"),
        "echo output: {res}"
    );

    // A denied command is refused (the tool call errors).
    let res = invoke(
        &plugins,
        "run_command",
        json!({ "command": "whoami" }),
        &ctx,
    )
    .await;
    assert_eq!(
        res["status"],
        json!("awaiting_approval"),
        "whoami first: {res}"
    );
    let token2 = recorder.asks.lock().unwrap().last().unwrap().1.clone();
    let q2 = db
        .append_event(
            "caller-1",
            "question",
            json!({ "approval_token": token2, "questions": [{ "question": "Approve whoami?" }] }),
        )
        .await
        .unwrap();
    db.append_event(
        "caller-1",
        "question-resolved",
        json!({ "question_id": q2.id, "answers": { "0": "Deny" } }),
    )
    .await
    .unwrap();
    let err = try_invoke(
        &plugins,
        "run_command",
        json!({ "command": "whoami" }),
        &ctx,
    )
    .await
    .expect_err("a denied command must be refused");
    assert!(err.contains("denied"), "deny: {err}");
}

/// `git init` the repo dir for the exec test; returns false if git is absent.
fn git_init(repo: &std::path::Path) -> bool {
    std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(repo)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Invoke a tool, asserting it is claimed and succeeds; returns its value.
async fn invoke(plugins: &PluginManager, tool: &str, args: Value, ctx: &Value) -> Value {
    plugins
        .invoke_mcp_tool(tool, args, ctx.clone())
        .await
        .unwrap_or_else(|| panic!("no active plugin claimed tool '{tool}'"))
        .unwrap_or_else(|e| panic!("tool '{tool}' failed: {e}"))
}

/// Invoke a tool, returning `Ok(value)` on Allow or `Err(reason)` on Cancel —
/// for the cases where the tool is *expected* to refuse.
async fn try_invoke(
    plugins: &PluginManager,
    tool: &str,
    args: Value,
    ctx: &Value,
) -> Result<Value, String> {
    plugins
        .invoke_mcp_tool(tool, args, ctx.clone())
        .await
        .unwrap_or_else(|| panic!("no active plugin claimed tool '{tool}'"))
        .map_err(|e| e.to_string())
}

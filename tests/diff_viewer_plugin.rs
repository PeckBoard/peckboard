//! End-to-end test of the **diff-viewer WASM plugin** against the real core
//! host functions. Loads the actual compiled `diff-viewer.wasm`, approves it,
//! and drives its authenticated `/api/plugin-ui/diff/*` endpoints through
//! `PluginManager::serve_http_authed` — exercising the multi-repo support added
//! to the plugin: repo discovery across the caller's folder (the folder root or
//! any subfolder that is a git work tree), and git/file operations scoped to a
//! selected repo via `git -C <prefix>` plus path-prefixing.
//!
//! The wasm is built out-of-tree (`peck-plugins/diff-viewer/build.sh`) and this
//! repo's `cargo test` has no `wasm32` toolchain, so the test **skips** with a
//! note when the artifact (or git) is absent.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewProject};
use peckboard::plugin::hooks::PluginHttpOutcome;
use peckboard::plugin::manager::PluginManager;
use serde_json::Value;

const PLUGIN_ID: &str = "diff-viewer";

/// Path to the out-of-tree compiled plugin, or `None` if it hasn't been built.
fn plugin_wasm() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../peck-plugins/diff-viewer/dist/plugin.wasm");
    p.exists().then_some(p)
}

/// Run a git command in `dir`; returns false if git is missing or the command
/// fails (the whole test then skips, since discovery needs git).
fn git(dir: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Create a git repo at `dir` with an `origin/main` ref pointing at the initial
/// commit, then a working-tree change (`a.txt` modified) and an untracked file
/// (`new.txt` added). Returns false if any git step fails.
fn make_repo(dir: &Path, body_v1: &str) -> bool {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("a.txt"), body_v1).unwrap();
    if !git(dir, &["init", "-q"]) {
        return false;
    }
    if !git(dir, &["add", "a.txt"]) || !git(dir, &["commit", "-q", "-m", "init"]) {
        return false;
    }
    // Fabricate a local origin/main ref at the initial commit, then diverge the
    // working tree from it.
    if !git(dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]) {
        return false;
    }
    std::fs::write(dir.join("a.txt"), "v2\n").unwrap();
    std::fs::write(dir.join("new.txt"), "fresh\n").unwrap();
    true
}

/// Drive an authed `/api/plugin-ui/diff/*` request scoped to project `p1`,
/// returning `(status, parsed-json-body)`.
async fn authed(
    plugins: &PluginManager,
    method: &str,
    path: &str,
    query: &str,
    body: &str,
) -> (u16, Value) {
    let mut headers = BTreeMap::new();
    headers.insert("x-peckboard-project-id".to_string(), "p1".to_string());
    match plugins
        .serve_http_authed("u1", method, path, query, &headers, body)
        .await
    {
        PluginHttpOutcome::Served { status, body, .. } => {
            let v: Value = serde_json::from_slice(&body).unwrap_or_else(|e| {
                panic!(
                    "{method} {path} returned non-JSON ({e}): {}",
                    String::from_utf8_lossy(&body)
                )
            });
            (status, v)
        }
        PluginHttpOutcome::NoRoute => panic!("no plugin route for {method} {path}"),
    }
}

#[tokio::test]
async fn diff_viewer_plugin_multi_repo_end_to_end() {
    let Some(wasm) = plugin_wasm() else {
        eprintln!(
            "SKIP diff_viewer_plugin_multi_repo_end_to_end: plugin wasm not built \
             (run peck-plugins/diff-viewer/build.sh)"
        );
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path();
    let plugins_dir = data_dir.join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::copy(&wasm, plugins_dir.join(format!("{PLUGIN_ID}.wasm"))).unwrap();

    // A folder that is NOT itself a repo but contains two repos as subfolders,
    // plus a plain (non-repo) subfolder that discovery must ignore.
    let root = data_dir.join("workspace");
    std::fs::create_dir_all(root.join("plain")).unwrap();
    std::fs::write(root.join("plain/notes.txt"), "just a file\n").unwrap();
    if !make_repo(&root.join("repoA"), "v1\n") || !make_repo(&root.join("repoB"), "v1\n") {
        eprintln!("SKIP diff_viewer_plugin_multi_repo_end_to_end: git unavailable");
        return;
    }

    let db = Db::open(data_dir).unwrap();
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "Workspace".into(),
        path: root.to_string_lossy().to_string(),
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

    let plugins = PluginManager::new(data_dir, db.clone());
    plugins.load_all().await.unwrap();
    let info = plugins
        .decide(PLUGIN_ID, true)
        .await
        .unwrap()
        .expect("diff-viewer plugin should be loaded");
    assert_eq!(info.status, "approved", "plugin must be active: {info:?}");

    // ── discovery: both subfolder repos found, the plain dir & root are not ──
    let (status, body) = authed(&plugins, "GET", "/api/plugin-ui/diff/repos", "", "").await;
    assert_eq!(status, 200, "repos: {body}");
    let prefixes: Vec<&str> = body["repos"]
        .as_array()
        .expect("repos array")
        .iter()
        .filter_map(|r| r["prefix"].as_str())
        .collect();
    assert!(prefixes.contains(&"repoA"), "repos discovered: {body}");
    assert!(prefixes.contains(&"repoB"), "repos discovered: {body}");
    assert!(!prefixes.contains(&""), "folder root is not a repo: {body}");
    assert!(
        !prefixes.contains(&"plain"),
        "plain dir is not a repo: {body}"
    );

    // ── files: scoped to repoA (modified a.txt + untracked new.txt) ──
    let (status, body) = authed(
        &plugins,
        "GET",
        "/api/plugin-ui/diff/files",
        "repo=repoA",
        "",
    )
    .await;
    assert_eq!(status, 200, "files: {body}");
    assert_eq!(
        body["base_available"],
        serde_json::json!(true),
        "files: {body}"
    );
    let mut entries: Vec<(String, String)> = body["files"]
        .as_array()
        .expect("files array")
        .iter()
        .map(|f| {
            (
                f["path"].as_str().unwrap_or("").to_string(),
                f["status"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        vec![
            ("a.txt".to_string(), "modified".to_string()),
            ("new.txt".to_string(), "added".to_string()),
        ],
        "repoA changed files: {body}"
    );

    // ── one file's two sides: origin/main "v1" vs working-tree "v2" ──
    let (status, body) = authed(
        &plugins,
        "GET",
        "/api/plugin-ui/diff/file",
        "repo=repoA&path=a.txt",
        "",
    )
    .await;
    assert_eq!(status, 200, "file: {body}");
    assert_eq!(
        body["old"]["text"],
        serde_json::json!("v1\n"),
        "old side: {body}"
    );
    assert_eq!(
        body["new"]["text"],
        serde_json::json!("v2\n"),
        "new side: {body}"
    );
    assert_eq!(
        body["status"],
        serde_json::json!("modified"),
        "file status: {body}"
    );

    // ── save: edits land in the right repo (path is prefixed with repoA) ──
    let (status, body) = authed(
        &plugins,
        "POST",
        "/api/plugin-ui/diff/save",
        "",
        r#"{"repo":"repoA","path":"a.txt","content":"v3\n"}"#,
    )
    .await;
    assert_eq!(status, 200, "save: {body}");
    assert_eq!(
        std::fs::read_to_string(root.join("repoA/a.txt")).unwrap(),
        "v3\n",
        "save wrote into repoA, not the folder root"
    );
    // repoB is untouched by a repoA save.
    assert_eq!(
        std::fs::read_to_string(root.join("repoB/a.txt")).unwrap(),
        "v2\n",
        "repoB must be unaffected"
    );

    // ── a repo prefix that tries to escape the folder is rejected ──
    let (status, body) = authed(
        &plugins,
        "GET",
        "/api/plugin-ui/diff/files",
        "repo=..%2Fescape",
        "",
    )
    .await;
    assert_eq!(status, 400, "escape attempt must 400: {body}");
}

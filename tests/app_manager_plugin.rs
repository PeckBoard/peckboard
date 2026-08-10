//! Integration test for the **app-manager WASM plugin** against the
//! real core host functions, mirroring `tests/ssh_fleet_plugin.rs`.
//!
//! Covers: plugin load/approval, `app_targets` (local + a remote target
//! seeded directly into the plugin's own `data_store`, proving the
//! namespacing and store shape), `app_list`'s structural shape against the
//! real local host, and the catalog/target whitelist that `app_status`,
//! `app_install`, and `app_remove` enforce before touching a shell.
//!
//! Deliberately does NOT kick off a real package install/remove — that
//! would mutate the CI host, may need root or network, and could hang on a
//! missing sudo password. The job-launch/poll/state-machine logic is
//! covered by `peck-plugins/app-manager/test/jobs.test.ts` against a
//! mocked exec host function instead.
//!
//! The wasm is built out-of-tree (`peck-plugins/app-manager/build.sh`);
//! this test **skips** with a note when the artifact is absent.

use std::path::PathBuf;

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewProject, NewSession};
use peckboard::plugin::manager::PluginManager;
use serde_json::{Value, json};

const PLUGIN_ID: &str = "app-manager";

fn plugin_wasm_path(plugin_id: &str) -> Option<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let in_tree = manifest_dir.join(format!("peck-plugins/{}/dist/plugin.wasm", plugin_id));
    if in_tree.exists() {
        return Some(in_tree);
    }
    let legacy = manifest_dir.join(format!("../peck-plugins/{}/dist/plugin.wasm", plugin_id));
    legacy.exists().then_some(legacy)
}

fn plugin_wasm() -> Option<PathBuf> {
    plugin_wasm_path(PLUGIN_ID)
}

async fn invoke(plugins: &PluginManager, tool: &str, args: Value, ctx: &Value) -> Value {
    plugins
        .invoke_mcp_tool(tool, args, ctx.clone())
        .await
        .expect("plugin should own this tool")
        .unwrap_or_else(|e| panic!("{tool} failed: {e}"))
}

#[tokio::test]
async fn app_manager_plugin_end_to_end() {
    let Some(wasm) = plugin_wasm() else {
        eprintln!(
            "SKIP app_manager_plugin_end_to_end: plugin wasm not built \
             (run peck-plugins/app-manager/build.sh)"
        );
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path();
    let plugins_dir = data_dir.join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::copy(&wasm, plugins_dir.join(format!("{PLUGIN_ID}.wasm"))).unwrap();

    let db = Db::open(data_dir).unwrap();
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "Test folder".into(),
        path: data_dir.to_string_lossy().to_string(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_project(NewProject {
        id: "proj-1".into(),
        name: "Test project".into(),
        context: String::new(),
        folder_id: "f1".into(),
        worker_count: 1,
        status: "active".into(),
        workflow: "fast-develop-software".into(),
        model: None,
        effort: None,
        budget_usd_cents: None,
        budget_period: None,
        worktree_isolation: false,
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
        project_id: Some("proj-1".into()),
        is_worker: true,
        created_at: ts.clone(),
        last_activity: ts,
        ..Default::default()
    })
    .await
    .unwrap();

    let plugins = PluginManager::new(data_dir, db.clone());
    plugins.load_all().await.unwrap();
    let info = plugins
        .decide(PLUGIN_ID, true)
        .await
        .unwrap()
        .expect("app-manager plugin should be loaded");
    assert_eq!(info.status, "approved", "plugin must be active: {info:?}");

    let ctx = json!({ "sessionId": "caller-1", "projectId": "proj-1", "folderId": "f1" });

    // ── app_targets: local is always present, with no configuration ────────
    let res = invoke(&plugins, "app_targets", json!({}), &ctx).await;
    let targets = res["targets"].as_array().expect("targets array");
    assert_eq!(targets.len(), 1, "only local by default: {res}");
    assert_eq!(targets[0]["id"], json!("local"));
    assert_eq!(targets[0]["kind"], json!("local"));

    // ── seed a remote target directly into the plugin's OWN data_store,
    // proving app_targets surfaces it and plugin data stores are correctly
    // namespaced per plugin (no MCP tool adds a remote target in this phase
    // — that lands with the UI-page card, see src/targets.ts).
    let target_json = json!({
        "id": "t1",
        "kind": "remote",
        "label": "Example Box",
        "hostname": "example.com",
        "port": 22,
        "username": "root",
        "key_id": "vault-key-1",
    })
    .to_string();
    db.plugin_store_put_blocking(PLUGIN_ID, "targets", "t1", &target_json)
        .unwrap();

    let res = invoke(&plugins, "app_targets", json!({}), &ctx).await;
    let targets = res["targets"].as_array().expect("targets array");
    assert_eq!(targets.len(), 2, "local + seeded remote: {res}");
    assert!(
        targets
            .iter()
            .any(|t| t["id"] == json!("t1") && t["hostname"] == json!("example.com")),
        "seeded remote target should be listed: {res}"
    );

    // ── app_list: structural shape against the real local host, scoped to
    // one catalog app so the test stays fast and doesn't depend on which
    // apps happen to be installed on the CI/dev host.
    let res = invoke(
        &plugins,
        "app_list",
        json!({ "targets": ["local"], "apps": ["git"] }),
        &ctx,
    )
    .await;
    let target_block = &res["targets"][0];
    assert_eq!(target_block["target"]["id"], json!("local"));
    assert!(
        target_block["package_manager"].is_string() || target_block["package_manager"].is_null(),
        "package_manager should be a pm string or null: {res}"
    );
    let git_entry = &target_block["apps"][0];
    assert_eq!(git_entry["id"], json!("git"));
    assert!(
        git_entry["installed"].is_boolean(),
        "installed should be a bool: {res}"
    );

    // ── install-record provenance: seed a snapshot-bracket install record
    // exactly as a settled install job writes it (see
    // peck-plugins/app-manager/src/provenance.ts), proving the store
    // round-trip through the plugin's own `installs` collection and that
    // app_list surfaces the recorded packages: the app row carries the
    // package-DB version, and every added package appears with a version,
    // labelled with the app that pulled it in.
    let install_record = json!({
        "job_id": "j99",
        "target_id": "local",
        "app_id": "git",
        "installed_at": "2026-08-09T00:00:00.000Z",
        "method": "apt",
        "tracking": "tracked",
        "primary": { "name": "git", "version": "1:2.43.0-1" },
        "added": [
            { "name": "git-man", "version": "1:2.43.0-1" },
            { "name": "liberror-perl", "version": "0.17029-2" }
        ],
        "changed": []
    })
    .to_string();
    db.plugin_store_put_blocking(PLUGIN_ID, "installs", "local:git", &install_record)
        .unwrap();

    let res = invoke(
        &plugins,
        "app_list",
        json!({ "targets": ["local"], "apps": ["git"] }),
        &ctx,
    )
    .await;
    let target_block = &res["targets"][0];
    let git_entry = &target_block["apps"][0];
    assert_eq!(
        git_entry["package_version"],
        json!("1:2.43.0-1"),
        "app_list should carry the recorded package-DB version: {res}"
    );
    assert_eq!(
        git_entry["package_tracking"],
        json!("tracked"),
        "a snapshot-bracketed install is tracked: {res}"
    );
    let added = target_block["added_packages"]
        .as_array()
        .expect("added_packages array");
    assert_eq!(added.len(), 2, "both recorded packages surface: {res}");
    assert!(
        added
            .iter()
            .all(|p| p["version"].is_string() && p["installed_with"] == json!("git")),
        "every added package carries a version and its installing app: {res}"
    );
    assert!(
        added
            .iter()
            .any(|p| p["name"] == json!("git-man") && p["version"] == json!("1:2.43.0-1")),
        "git-man 1:2.43.0-1 should be listed: {res}"
    );

    // ── dependency graph: seed a stored graph exactly as refreshDepGraph
    // writes it (peck-plugins/app-manager/src/deps.ts, `depgraphs` collection,
    // keyed by target id), proving the store round-trip through the wasm and
    // the derived views. The graph is a DAG: libssl3 has TWO parents (git and
    // node), so it must be flagged shared, listed under both apps, and never
    // offered as removal collateral while the other app still needs it.
    let dep_graph = json!({
        "target_id": "local",
        "pm": "apt",
        "at": "2026-08-09T00:00:00.000Z",
        "depth": 2,
        "truncated": false,
        "nodes": [
            { "name": "git", "version": "1:2.43.0-1", "kind": "app", "binaries": ["/usr/bin/git"] },
            { "name": "nodejs", "version": "18.19.0+dfsg-6", "kind": "app" },
            { "name": "git-man", "version": "1:2.43.0-1", "kind": "binary" },
            { "name": "libssl3", "version": "3.0.13-1", "kind": "library" }
        ],
        "edges": [
            { "from": "git", "to": "git-man", "kind": "depends" },
            { "from": "git", "to": "libssl3", "kind": "depends" },
            { "from": "nodejs", "to": "libssl3", "kind": "depends" }
        ]
    })
    .to_string();
    db.plugin_store_put_blocking(PLUGIN_ID, "depgraphs", "local", &dep_graph)
        .unwrap();

    let res = invoke(&plugins, "app_deps", json!({ "target": "local" }), &ctx).await;
    assert_eq!(
        res["graph"]["at"],
        json!("2026-08-09T00:00:00.000Z"),
        "the stored snapshot round-trips with its timestamp: {res}"
    );
    assert_eq!(
        res["graph"]["node_count"],
        json!(4),
        "all nodes surface: {res}"
    );
    let nodes = res["nodes"].as_array().expect("nodes array");
    let ssl = nodes
        .iter()
        .find(|n| n["name"] == json!("libssl3"))
        .expect("libssl3 node");
    assert_eq!(
        ssl["shared"],
        json!(true),
        "multi-parent node is shared: {res}"
    );
    assert_eq!(
        nodes
            .iter()
            .find(|n| n["name"] == json!("git"))
            .expect("git node")["binaries"],
        json!(["/usr/bin/git"]),
        "app-node binaries round-trip: {res}"
    );

    let apps = res["apps"].as_array().expect("apps array");
    let git_deps = apps
        .iter()
        .find(|a| a["id"] == json!("git"))
        .expect("git deps entry");
    assert_eq!(git_deps["tracked"], json!(true));
    let child_names = |entry: &Value| -> Vec<String> {
        entry["tree"][0]["children"]
            .as_array()
            .expect("tree children")
            .iter()
            .filter_map(|c| c["name"].as_str().map(String::from))
            .collect()
    };
    let git_children = child_names(git_deps);
    assert!(
        git_children.contains(&"git-man".into()) && git_children.contains(&"libssl3".into()),
        "git's tree lists both dependencies: {res}"
    );
    let node_deps = apps
        .iter()
        .find(|a| a["id"] == json!("node"))
        .expect("node deps entry");
    assert!(
        child_names(node_deps).contains(&"libssl3".into()),
        "the shared dependency appears under node as well, not only under git: {res}"
    );

    // Removal safety (autoremove semantics): removing git frees git-man only;
    // libssl3 survives because node still needs it, and says so.
    let also: Vec<&str> = git_deps["also_removed"]
        .as_array()
        .expect("also_removed")
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert_eq!(
        also,
        vec!["git-man"],
        "shared dep is never collateral: {res}"
    );
    assert!(
        git_deps["kept"].as_array().expect("kept").iter().any(|k| {
            k["name"] == json!("libssl3")
                && k["needed_by"]
                    .as_array()
                    .is_some_and(|n| n.contains(&json!("Node.js")))
        }),
        "libssl3 is kept and attributed to Node.js: {res}"
    );

    // Reverse view: select the library, see every app that requires it.
    let lib = res["libraries"]
        .as_array()
        .expect("libraries array")
        .iter()
        .find(|l| l["name"] == json!("libssl3"))
        .expect("libssl3 reverse entry");
    assert_eq!(
        lib["required_by"],
        json!(["Git", "Node.js"]),
        "reverse view lists every requiring app: {res}"
    );

    // ── catalog/target whitelist: app_status/app_install/app_remove refuse
    // an unknown app or target BEFORE touching a shell — no process spawned.
    let res = invoke(
        &plugins,
        "app_status",
        json!({ "app": "not-a-real-app", "target": "local" }),
        &ctx,
    )
    .await;
    assert!(
        res["error"]
            .as_str()
            .unwrap_or_default()
            .contains("unknown app"),
        "app_status should reject an unknown app: {res}"
    );

    let res = invoke(
        &plugins,
        "app_install",
        json!({ "app": "not-a-real-app", "target": "local" }),
        &ctx,
    )
    .await;
    assert!(
        res["error"]
            .as_str()
            .unwrap_or_default()
            .contains("unknown app"),
        "app_install should reject an unknown app: {res}"
    );

    let res = invoke(
        &plugins,
        "app_remove",
        json!({ "app": "git", "target": "no-such-target" }),
        &ctx,
    )
    .await;
    assert!(
        res["error"]
            .as_str()
            .unwrap_or_default()
            .contains("unknown target"),
        "app_remove should reject an unknown target: {res}"
    );
}

/// The AI-session install flow end to end against the real host functions:
/// the picker options come from `peckboard_list_models` (thinking-only — a
/// non-thinking mock model is absent), `POST /install` creates a TEMP
/// session on the picked model and dispatches a `sudo -A` prompt through a
/// recording LiveHost, progress is read from the slim event tail, success
/// is decided by the detect probe after `agent-end`, the chosen model is
/// persisted as the default — and an abandoned session (deleted before the
/// run ends) lands failed with no provenance record.
#[tokio::test]
async fn app_manager_session_install_flow() {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use peckboard::plugin::hooks::PluginHttpOutcome;
    use peckboard::plugin::host::LiveHost;
    use peckboard::provider::mock::register_mock_provider;
    use peckboard::provider::registry::ProviderRegistry;

    let Some(wasm) = plugin_wasm() else {
        eprintln!(
            "SKIP app_manager_session_install_flow: plugin wasm not built \
             (run peck-plugins/app-manager/build.sh)"
        );
        return;
    };

    struct RecordingLive {
        dispatched: Arc<Mutex<Vec<(String, String)>>>,
    }
    impl LiveHost for RecordingLive {
        fn dispatch_capture(&self, session_id: String, prompt: String) {
            self.dispatched.lock().unwrap().push((session_id, prompt));
        }
        fn resume_session(&self, _session_id: String, _text: String) {}
    }

    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path();
    let plugins_dir = data_dir.join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::copy(&wasm, plugins_dir.join(format!("{PLUGIN_ID}.wasm"))).unwrap();

    let db = Db::open(data_dir).unwrap();
    let plugins = Arc::new(PluginManager::new(data_dir, db.clone()));
    plugins.load_all().await.unwrap();
    let info = plugins
        .decide(PLUGIN_ID, true)
        .await
        .unwrap()
        .expect("app-manager plugin should be loaded");
    assert_eq!(info.status, "approved", "plugin must be active: {info:?}");

    // Bind the model catalog (mock provider: only `plan-review` is tagged
    // reasoning) and a LiveHost that records dispatches instead of spawning.
    let registry = Arc::new(ProviderRegistry::new());
    register_mock_provider(&registry).await;
    plugins.set_provider_registry(&registry);
    let dispatched = Arc::new(Mutex::new(Vec::new()));
    plugins.set_live_host(Arc::new(RecordingLive {
        dispatched: dispatched.clone(),
    }));

    let headers = BTreeMap::new();
    let authed = |method: &'static str, path_and_query: String, body: String| {
        let plugins = plugins.clone();
        let headers = headers.clone();
        async move {
            // Route matching sees the bare path; the query rides separately.
            let (path, query) = match path_and_query.split_once('?') {
                Some((p, q)) => (p.to_string(), q.to_string()),
                None => (path_and_query, String::new()),
            };
            match plugins
                .serve_http_authed("u1", method, &path, &query, &headers, &body)
                .await
            {
                PluginHttpOutcome::Served { status, body, .. } => {
                    let text = String::from_utf8_lossy(&body).to_string();
                    let v: Value = serde_json::from_str(&text)
                        .unwrap_or_else(|_| panic!("non-JSON response ({status}): {text}"));
                    (status, v)
                }
                PluginHttpOutcome::NoRoute => panic!("no route for {method} {path}"),
            }
        }
    };

    // ── picker options: thinking models only, no default stored yet ──────
    let (status, v) = authed(
        "GET",
        "/api/plugin-ui/app-manager/install-options".into(),
        String::new(),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let ids: Vec<&str> = v["models"]
        .as_array()
        .expect("models array")
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert!(
        ids.contains(&"mock:plan-review"),
        "thinking mock model offered: {v}"
    );
    assert!(
        !ids.iter().any(|id| id.contains("happy-path")),
        "a non-thinking model must never be offered: {v}"
    );
    assert!(v["default_model"].is_null(), "no default yet: {v}");

    // ── refuse a model outside the offered catalog ────────────────────
    let (_s, v) = authed(
        "POST",
        "/api/plugin-ui/app-manager/install".into(),
        json!({ "target": "local", "app": "git", "model": "mock:happy-path" }).to_string(),
    )
    .await;
    assert!(
        v["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not in the selectable catalog"),
        "non-thinking model refused: {v}"
    );
    assert!(dispatched.lock().unwrap().is_empty());

    // ── start a session install (git) ─────────────────────────────
    let (status, v) = authed(
        "POST",
        "/api/plugin-ui/app-manager/install".into(),
        json!({ "target": "local", "app": "git", "model": "mock:plan-review" }).to_string(),
    )
    .await;
    assert_eq!(status, 200, "install should start: {v}");
    let session_id = v["session_id"].as_str().expect("session_id").to_string();

    // The temp session exists on the picked model, in the shared install
    // folder core registered server-side.
    let session = db
        .get_session(&session_id)
        .await
        .unwrap()
        .expect("install session row");
    assert!(session.is_temp, "install session must be temp");
    assert_eq!(session.name, "Install Git");
    assert_eq!(session.model.as_deref(), Some("mock:plan-review"));
    let folder = db
        .get_folder(&session.folder_id)
        .await
        .unwrap()
        .expect("install folder row");
    assert!(
        folder.path.ends_with("peckboard-installs/app-manager"),
        "session lives in the shared install folder: {}",
        folder.path
    );

    // The prompt went through the LiveHost with the askpass sudo rule.
    {
        let d = dispatched.lock().unwrap();
        assert_eq!(d.len(), 1, "exactly one dispatch");
        assert_eq!(d[0].0, session_id);
        assert!(d[0].1.contains("sudo -A"), "prompt: {}", d[0].1);
        assert!(d[0].1.contains("Install Git"), "prompt: {}", d[0].1);
    }

    // The chosen account+model became the stored default.
    let (_s, v) = authed(
        "GET",
        "/api/plugin-ui/app-manager/install-options".into(),
        String::new(),
    )
    .await;
    assert_eq!(v["default_model"], json!("mock:plan-review"));

    // ── progress from the slim event tail; settle on agent-end ─────────
    db.append_event(&session_id, "agent-start", json!({ "model": "mock" }))
        .await
        .unwrap();
    db.append_event(&session_id, "agent-tool-start", json!({ "name": "Bash" }))
        .await
        .unwrap();
    db.append_event(&session_id, "agent-end", json!({}))
        .await
        .unwrap();

    let (status, v) = authed(
        "GET",
        "/api/plugin-ui/app-manager/status?target=local&app=git".into(),
        String::new(),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let job = &v["job"];
    // git is certainly installed on the host running this suite (the repo
    // itself is a git checkout), so the detect probe confirms the install.
    assert_eq!(job["status"], json!("succeeded"), "job: {v}");
    assert_eq!(job["is_session"], json!(true), "job: {v}");
    let activity = job["activity"].as_array().expect("activity lines");
    assert!(
        activity.contains(&json!("Tool: Bash")),
        "tool-level activity from the slim tail: {v}"
    );
    assert!(
        job["log_tail"].as_str().unwrap_or_default().is_empty(),
        "a session job must not fake a log tail: {v}"
    );

    // ── abandoned session: deleted before the run ends → failed, and no
    // provenance record may appear for the app ───────────────────────
    let (status, v) = authed(
        "POST",
        "/api/plugin-ui/app-manager/install".into(),
        json!({ "target": "local", "app": "ripgrep", "model": "mock:plan-review" }).to_string(),
    )
    .await;
    assert_eq!(status, 200, "second install should start: {v}");
    let session2 = v["session_id"].as_str().expect("session_id").to_string();
    assert!(db.delete_session(&session2).await.unwrap());

    let (status, v) = authed(
        "GET",
        "/api/plugin-ui/app-manager/status?target=local&app=ripgrep".into(),
        String::new(),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    let job = &v["job"];
    assert_eq!(
        job["status"],
        json!("failed"),
        "an abandoned session must land failed, never running/succeeded: {v}"
    );
    assert!(
        job["message"]
            .as_str()
            .unwrap_or_default()
            .contains("ended before completing"),
        "clear unknown-state note: {v}"
    );

    // No install record was written for the abandoned job: the dashboard
    // row shows no recorded package version, no "installed with" packages,
    // and no provenance note — all of which only render from a record.
    let (_s, v) = authed(
        "GET",
        "/api/plugin-ui/app-manager/apps?target=local".into(),
        String::new(),
    )
    .await;
    if let Some(rows) = v["apps"].as_array() {
        let rg = rows
            .iter()
            .find(|a| a["id"] == json!("ripgrep"))
            .expect("ripgrep row");
        assert!(
            rg["package_version"].is_null()
                && rg["provenance_note"].is_null()
                && rg["added_packages"].as_array().is_none_or(Vec::is_empty),
            "no bogus provenance record for an abandoned install: {rg}"
        );
    }
}

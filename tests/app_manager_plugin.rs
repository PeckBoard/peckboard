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

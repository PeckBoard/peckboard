//! End-to-end test of the **session-control WASM plugin** against the real core
//! host functions. Loads the compiled `peckboard_session_control_plugin.wasm`,
//! approves it, and drives its MCP tools through `PluginManager::invoke_mcp_tool`
//! — exercising the `mcp.tool.invoke` dispatch plus the session-control host
//! functions (`peckboard_interrupt_session` / `_terminate_agent` /
//! `_clear_session` / `_send_message`) and the `LiveHost` seam they fan out to.
//!
//! The wasm is built out-of-tree (`peck-plugins/session-control/build.sh`) and
//! this repo's `cargo test` has no `wasm32` toolchain, so the test **skips**
//! with a note when the artifact is absent.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewSession};
use peckboard::plugin::host::{LiveAttachment, LiveHost};
use peckboard::plugin::manager::PluginManager;
use serde_json::{Value, json};

const PLUGIN_ID: &str = "session-control";

/// Records the control actions the plugin's tools fan out to, so the test can
/// assert the full plugin → host-fn → LiveHost chain reached the seam with the
/// right session id / payload.
#[derive(Default)]
struct ControlRecorder {
    interrupts: Mutex<Vec<String>>,
    terminates: Mutex<Vec<String>>,
    clears: Mutex<Vec<String>>,
    messages: Mutex<Vec<(String, String, usize)>>, // (session, text, attachment_count)
}
impl LiveHost for ControlRecorder {
    fn dispatch_capture(&self, _session_id: String, _prompt: String, _clear_first: bool) {}
    fn resume_session(&self, _session_id: String, _text: String) {}
    fn interrupt_session(&self, session_id: String) {
        self.interrupts.lock().unwrap().push(session_id);
    }
    fn terminate_agent(&self, session_id: String) {
        self.terminates.lock().unwrap().push(session_id);
    }
    fn clear_session(&self, session_id: String) {
        self.clears.lock().unwrap().push(session_id);
    }
    fn send_message(&self, session_id: String, text: String, attachments: Vec<LiveAttachment>) {
        self.messages
            .lock()
            .unwrap()
            .push((session_id, text, attachments.len()));
    }
}

fn plugin_wasm() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "peck-plugins/session-control/target/wasm32-unknown-unknown/release/\
         peckboard_session_control_plugin.wasm",
    );
    p.exists().then_some(p)
}

#[tokio::test]
async fn session_control_plugin_drives_tools_end_to_end() {
    let Some(wasm) = plugin_wasm() else {
        eprintln!(
            "SKIP session_control_plugin_drives_tools_end_to_end: plugin wasm not built \
             (run peck-plugins/session-control/build.sh)"
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
        name: "F".into(),
        path: "/tmp/sc".into(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    // Same-folder caller/target; plus a second folder with a foreign target
    // used to assert the cross-folder gate.
    db.create_folder(NewFolder {
        id: "f2".into(),
        name: "F2".into(),
        path: "/tmp/sc2".into(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    for sid in ["caller-1", "target-1"] {
        db.create_session(NewSession {
            id: sid.into(),
            name: sid.into(),
            folder_id: "f1".into(),
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    }
    db.create_session(NewSession {
        id: "foreign-1".into(),
        name: "foreign-1".into(),
        folder_id: "f2".into(),
        created_at: ts.clone(),
        last_activity: ts.clone(),
        ..Default::default()
    })
    .await
    .unwrap();

    let plugins = PluginManager::new(data_dir, db.clone());
    plugins.load_all().await.unwrap();
    let recorder = Arc::new(ControlRecorder::default());
    plugins.set_live_host(recorder.clone());
    let info = plugins
        .decide(PLUGIN_ID, true)
        .await
        .unwrap()
        .expect("session-control plugin should be loaded");
    assert_eq!(info.status, "approved", "plugin must be active: {info:?}");

    let ctx = json!({ "sessionId": "caller-1", "folderId": "f1" });

    // interrupt → reaches LiveHost with the target id.
    let res = invoke(
        &plugins,
        "interrupt_session",
        json!({ "session_id": "target-1" }),
        &ctx,
    )
    .await;
    assert_eq!(res["ok"], json!(true), "interrupt: {res}");
    assert_eq!(res["action"], json!("interrupt"));
    assert_eq!(recorder.interrupts.lock().unwrap().as_slice(), ["target-1"]);

    // terminate + clear likewise.
    invoke(
        &plugins,
        "terminate_agent",
        json!({ "session_id": "target-1" }),
        &ctx,
    )
    .await;
    assert_eq!(recorder.terminates.lock().unwrap().as_slice(), ["target-1"]);
    invoke(
        &plugins,
        "clear_session",
        json!({ "session_id": "target-1" }),
        &ctx,
    )
    .await;
    assert_eq!(recorder.clears.lock().unwrap().as_slice(), ["target-1"]);

    // send_message delivers text, no attachments.
    invoke(
        &plugins,
        "send_message",
        json!({ "session_id": "target-1", "text": "stop and wait" }),
        &ctx,
    )
    .await;
    // send_image delivers a base64 image as one attachment (host decodes it).
    let png_b64 = "iVBORw0KGgo="; // arbitrary valid base64
    invoke(
        &plugins,
        "send_image",
        json!({
            "session_id": "target-1",
            "image_base64": png_b64,
            "mime_type": "image/png",
            "caption": "see this",
        }),
        &ctx,
    )
    .await;
    let msgs = recorder.messages.lock().unwrap().clone();
    assert_eq!(msgs.len(), 2, "two send calls: {msgs:?}");
    assert_eq!(msgs[0], ("target-1".into(), "stop and wait".into(), 0));
    assert_eq!(msgs[1], ("target-1".into(), "see this".into(), 1));

    // find_session lists sessions folder-blind (no LiveHost needed). With no
    // query it returns every session; a query narrows by substring.
    let all = invoke(&plugins, "find_session", json!({}), &ctx).await;
    let sessions = all["sessions"].as_array().expect("sessions array");
    assert_eq!(sessions.len(), 3, "all sessions listed: {all}");
    let filtered = invoke(&plugins, "find_session", json!({ "query": "target" }), &ctx).await;
    let hits = filtered["sessions"].as_array().expect("sessions array");
    assert_eq!(hits.len(), 1, "query narrows: {filtered}");
    assert_eq!(hits[0]["session_id"], json!("target-1"));

    // Unknown target id → clean "not found" error, no LiveHost call.
    let err = try_invoke(
        &plugins,
        "interrupt_session",
        json!({ "session_id": "nope" }),
        &ctx,
    )
    .await
    .expect_err("unknown session must error");
    assert!(err.contains("not found"), "got: {err}");
    assert_eq!(
        recorder.interrupts.lock().unwrap().len(),
        1,
        "no extra interrupt recorded"
    );

    // Cross-folder without a grant: plugin asks (awaiting_approval) rather
    // than dispatching. Host would also refuse if the plugin skipped the ask.
    let pending = invoke(
        &plugins,
        "interrupt_session",
        json!({ "session_id": "foreign-1" }),
        &ctx,
    )
    .await;
    assert_eq!(
        pending["status"],
        json!("awaiting_approval"),
        "cross-folder must ask: {pending}"
    );
    assert_eq!(
        recorder.interrupts.lock().unwrap().len(),
        1,
        "no cross-folder interrupt without grant"
    );

    // Always grant → cross-folder interrupt reaches LiveHost.
    peckboard::plugin::session_control_auth::grant_always(&db, PLUGIN_ID, "caller-1").unwrap();
    let xf = invoke(
        &plugins,
        "interrupt_session",
        json!({ "session_id": "foreign-1" }),
        &ctx,
    )
    .await;
    assert_eq!(xf["ok"], json!(true), "cross-folder after Always: {xf}");
    assert_eq!(
        recorder.interrupts.lock().unwrap().as_slice(),
        ["target-1", "foreign-1"]
    );
}

/// `read_session`: the read twin of the control tools. Same cross-folder
/// approval gate, plus an ownership boundary the mutating tools don't have —
/// a read discloses transcript content, so one user's approval must not hand
/// an agent another user's private chat.
#[tokio::test]
async fn read_session_respects_the_folder_gate_and_session_ownership() {
    let Some(wasm) = plugin_wasm() else {
        eprintln!(
            "SKIP read_session_respects_the_folder_gate_and_session_ownership: plugin wasm \
             not built (run peck-plugins/session-control/build.sh)"
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
    for f in ["f1", "f2"] {
        db.create_folder(NewFolder {
            id: f.into(),
            name: f.into(),
            path: data_dir.join(f).to_string_lossy().to_string(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
    }
    for u in ["u1", "u2"] {
        db.create_user(peckboard::db::models::NewUser {
            id: u.into(),
            username: u.into(),
            email: None,
            password_hash: "h".into(),
            role: "user".into(),
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();
    }
    // caller + mate: same folder, same owner. other: another folder, same
    // owner. private: another folder AND another owner.
    for (id, folder, owner) in [
        ("caller", "f1", "u1"),
        ("mate", "f1", "u1"),
        ("other", "f2", "u1"),
        ("private", "f2", "u2"),
    ] {
        db.create_session(NewSession {
            id: id.into(),
            name: id.into(),
            folder_id: folder.into(),
            user_id: Some(owner.into()),
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
        db.append_event(id, "user", json!({ "text": format!("secret of {id}") }))
            .await
            .unwrap();
    }

    let plugins = PluginManager::new(data_dir, db.clone());
    plugins.load_all().await.unwrap();
    plugins.set_live_host(Arc::new(ControlRecorder::default()));
    let info = plugins
        .decide(PLUGIN_ID, true)
        .await
        .unwrap()
        .expect("session-control plugin should be loaded");
    assert_eq!(info.status, "approved", "plugin must be active: {info:?}");

    let ctx = json!({ "sessionId": "caller", "folderId": "f1" });

    // Same folder → the summarized event tail, no approval needed.
    let same = invoke(
        &plugins,
        "read_session",
        json!({ "session_id": "mate" }),
        &ctx,
    )
    .await;
    assert_eq!(same["session_id"], json!("mate"), "same folder: {same}");
    assert!(
        same.to_string().contains("secret of mate"),
        "same-folder read returns events: {same}"
    );

    // Unknown id → clean not found.
    let err = try_invoke(
        &plugins,
        "read_session",
        json!({ "session_id": "nope" }),
        &ctx,
    )
    .await
    .expect_err("unknown session must error");
    assert!(err.contains("not found"), "got: {err}");

    // Cross-folder without a grant: asks, and leaks nothing.
    let pending = invoke(
        &plugins,
        "read_session",
        json!({ "session_id": "other" }),
        &ctx,
    )
    .await;
    assert_eq!(
        pending["status"],
        json!("awaiting_approval"),
        "cross-folder read must ask: {pending}"
    );
    assert!(
        !pending.to_string().contains("secret of other"),
        "nothing leaks before approval: {pending}"
    );

    // Always grant → the cross-folder read goes through.
    peckboard::plugin::session_control_auth::grant_always(&db, PLUGIN_ID, "caller").unwrap();
    let granted = invoke(
        &plugins,
        "read_session",
        json!({ "session_id": "other" }),
        &ctx,
    )
    .await;
    assert!(
        granted.to_string().contains("secret of other"),
        "granted cross-folder read returns events: {granted}"
    );

    // Another user's plain chat stays unreadable even WITH the grant, in
    // not-found framing so ids can't be probed.
    let denied = try_invoke(
        &plugins,
        "read_session",
        json!({ "session_id": "private" }),
        &ctx,
    )
    .await
    .expect_err("another user's chat must stay unreadable");
    assert!(denied.contains("not found"), "got: {denied}");
    assert!(
        !denied.contains("secret of private"),
        "no content leaks: {denied}"
    );

    // last_n caps the tail.
    let capped = invoke(
        &plugins,
        "read_session",
        json!({ "session_id": "mate", "last_n": 1 }),
        &ctx,
    )
    .await;
    assert_eq!(capped["event_count"], json!(1), "last_n honoured: {capped}");
}

async fn invoke(plugins: &PluginManager, tool: &str, args: Value, ctx: &Value) -> Value {
    plugins
        .invoke_mcp_tool(tool, args, ctx.clone())
        .await
        .unwrap_or_else(|| panic!("no active plugin claimed tool '{tool}'"))
        .unwrap_or_else(|e| panic!("tool '{tool}' failed: {e}"))
}

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

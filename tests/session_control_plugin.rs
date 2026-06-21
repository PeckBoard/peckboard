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
    fn dispatch_capture(&self, _session_id: String, _prompt: String) {}
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
        "../peck-plugins/session-control/target/wasm32-unknown-unknown/release/\
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
    // Caller and target are plain chat sessions; the control plugin has NO
    // folder boundary, but here they share a folder anyway.
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

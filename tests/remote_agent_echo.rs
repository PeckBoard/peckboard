//! `remote_agent_*` MCP bridge — echo round-trip through a fake device.
//!
//! Proves the full session → `handle_tool_call` → `DeviceRegistry` →
//! device → result loop with no real daemon: the "device" is a task
//! holding a `DeviceConnection` that answers every `ServerFrame::Request`
//! by feeding an `AgentFrame::Result` back through
//! `DeviceRegistry::handle_frame`, exactly as the `/ws/agent` socket task
//! does. Also locks the security edges: ownership ("not found" for a
//! foreign device), the server-side kill-switch (disabled refused even
//! with a live socket), and offline reporting.

use std::sync::Arc;

use peckboard::db::Db;
use peckboard::db::models::{NewDevice, NewFolder, NewSession, NewUser, device_status};
use peckboard::service::mcp_server::{McpToolRegistry, ToolCallContext};
use peckboard::ws::agent::DeviceRegistry;
use peckboard::ws::broadcaster::Broadcaster;
use peckboard_agent_protocol::{AgentFrame, ServerFrame};

async fn seed_folder(db: &Db, id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: id.into(),
        name: id.into(),
        path: format!("/tmp/remote-agent-echo/{id}"),
        created_at: ts,
    })
    .await
    .unwrap();
}

async fn seed_user(db: &Db, id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_user(NewUser {
        id: id.into(),
        username: id.into(),
        email: None,
        password_hash: "h".into(),
        role: "member".into(),
        created_at: ts.clone(),
        updated_at: ts,
    })
    .await
    .unwrap();
}

async fn seed_session(db: &Db, id: &str, owner: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_session(NewSession {
        id: id.into(),
        name: id.into(),
        folder_id: "f1".into(),
        model: None,
        effort: None,
        is_worker: false,
        project_id: None,
        card_id: None,
        conversation_id: None,
        created_at: ts.clone(),
        last_activity: ts,
        is_expert: false,
        expert_kind: None,
        knowledge_summary: None,
        knowledge_area: None,
        scope_path: None,
        is_permanent: false,
        repeating_task_id: None,
        system_prompt: None,
        handover_run_id: None,
        handover_to_model: None,
        pending_handover_doc: None,
        worker_step: None,
        user_id: Some(owner.to_string()),
        model_autoswitch: None,
        context_reset_ts: None,
        system_prompt_name: None,
        is_temp: false,
        parent_session_id: None,
        subagent_completed_at: None,
    })
    .await
    .unwrap();
}

async fn seed_device(db: &Db, id: &str, user_id: &str, status: &str) {
    db.insert_device(NewDevice {
        id: id.into(),
        user_id: user_id.into(),
        name: format!("{id}-name"),
        platform: "linux".into(),
        secret_hash: format!("hash-{id}"),
        status: status.into(),
        last_seen_at: None,
        created_at: chrono::Utc::now().to_rfc3339(),
    })
    .await
    .unwrap();
}

struct Fixture {
    db: Arc<Db>,
    registry: Arc<DeviceRegistry>,
    broadcaster: Arc<Broadcaster>,
    tools: McpToolRegistry,
}

impl Fixture {
    /// Folder f1, users u-a / u-b with one chat session each, and devices:
    /// d-online (u-a, active), d-foreign (u-b, active), d-disabled (u-a),
    /// d-offline (u-a, active, never connected).
    async fn new() -> Self {
        let db = Arc::new(Db::in_memory().unwrap());
        seed_folder(&db, "f1").await;
        seed_user(&db, "u-a").await;
        seed_user(&db, "u-b").await;
        seed_session(&db, "chat-a", "u-a").await;
        seed_session(&db, "chat-b", "u-b").await;
        seed_device(&db, "d-online", "u-a", device_status::ACTIVE).await;
        seed_device(&db, "d-foreign", "u-b", device_status::ACTIVE).await;
        seed_device(&db, "d-disabled", "u-a", device_status::DISABLED).await;
        seed_device(&db, "d-offline", "u-a", device_status::ACTIVE).await;
        Fixture {
            db,
            registry: Arc::new(DeviceRegistry::default()),
            broadcaster: Broadcaster::new(),
            tools: McpToolRegistry::new(),
        }
    }

    /// Register a live connection for `device_id` and spawn the fake
    /// daemon: every `Request` is answered with `ok: true` and a payload
    /// echoing the capability and args (screenshot requests answer with
    /// an image payload instead).
    fn connect_fake_device(&self, device_id: &str) {
        let conn = self.registry.connect(device_id);
        let registry = self.registry.clone();
        let broadcaster = self.broadcaster.clone();
        let device_id = device_id.to_string();
        let mut outbound = conn.outbound;
        tokio::spawn(async move {
            while let Some(frame) = outbound.recv().await {
                if let ServerFrame::Request {
                    corr_id,
                    capability,
                    args,
                } = frame
                {
                    let payload = if capability == "screenshot" {
                        serde_json::json!({ "image_base64": "aGVsbG8=", "mime": "image/png" })
                    } else {
                        serde_json::json!({ "capability": capability, "echo": args })
                    };
                    registry.handle_frame(
                        &broadcaster,
                        &device_id,
                        AgentFrame::Result {
                            corr_id,
                            ok: true,
                            payload: Some(payload),
                            error: None,
                        },
                    );
                }
            }
        });
    }

    fn ctx(&self, session_id: &str) -> ToolCallContext {
        ToolCallContext {
            session_id: session_id.into(),
            project_id: None,
            card_id: None,
            folder_id: "f1".into(),
            db: self.db.clone(),
            broadcaster: self.broadcaster.clone(),
            provider_registry: None,
            data_dir: None,
            device_registry: Some(self.registry.clone()),
        }
    }

    async fn call(
        &self,
        session_id: &str,
        tool: &str,
        args: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        self.tools
            .handle_tool_call(tool, args, &self.ctx(session_id))
            .await
    }
}

#[tokio::test]
async fn echo_round_trips_through_a_fake_device() {
    let fx = Fixture::new().await;
    fx.connect_fake_device("d-online");

    let out = fx
        .call(
            "chat-a",
            "remote_agent_echo",
            serde_json::json!({ "device_id": "d-online", "message": "hello device" }),
        )
        .await
        .unwrap();

    assert_eq!(out["ok"], true);
    assert_eq!(out["device_id"], "d-online");
    assert_eq!(out["result"]["capability"], "echo");
    // device_id is routing, not payload — the daemon must only see the rest.
    assert_eq!(
        out["result"]["echo"],
        serde_json::json!({ "message": "hello device" })
    );
    // The reply resolved the pending slot; nothing left in flight.
    assert_eq!(fx.registry.in_flight("d-online"), 0);
}

#[tokio::test]
async fn foreign_device_is_not_found_not_forbidden() {
    let fx = Fixture::new().await;
    fx.connect_fake_device("d-online");

    let err = fx
        .call(
            "chat-b",
            "remote_agent_echo",
            serde_json::json!({ "device_id": "d-online", "message": "hi" }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("device not found"), "got: {msg}");
    // No oracle: the owner's device name must not leak into the error.
    assert!(!msg.contains("d-online-name"), "got: {msg}");
}

#[tokio::test]
async fn disabled_device_is_refused_even_with_a_live_socket() {
    let fx = Fixture::new().await;
    // Kill-switch flipped while the socket is still up: the bridge must
    // refuse server-side, not forward and hope the daemon complies.
    fx.connect_fake_device("d-disabled");

    let err = fx
        .call(
            "chat-a",
            "remote_agent_echo",
            serde_json::json!({ "device_id": "d-disabled", "message": "hi" }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("disabled"), "got: {err}");
}

#[tokio::test]
async fn offline_device_reports_offline() {
    let fx = Fixture::new().await;

    let err = fx
        .call(
            "chat-a",
            "remote_agent_echo",
            serde_json::json!({ "device_id": "d-offline", "message": "hi" }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("offline"), "got: {err}");
}

#[tokio::test]
async fn list_shows_only_the_callers_devices_with_live_state() {
    let fx = Fixture::new().await;
    fx.connect_fake_device("d-online");

    let out = fx
        .call("chat-a", "remote_agent_list", serde_json::json!({}))
        .await
        .unwrap();

    let devices = out["devices"].as_array().unwrap();
    let ids: Vec<&str> = devices
        .iter()
        .map(|d| d["device_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"d-online"));
    assert!(ids.contains(&"d-disabled"));
    assert!(ids.contains(&"d-offline"));
    assert!(!ids.contains(&"d-foreign"), "another user's device leaked");

    let online = devices
        .iter()
        .find(|d| d["device_id"] == "d-online")
        .unwrap();
    assert_eq!(online["online"], true);
    assert_eq!(online["in_flight"], 0);
    let offline = devices
        .iter()
        .find(|d| d["device_id"] == "d-offline")
        .unwrap();
    assert_eq!(offline["online"], false);
}

#[tokio::test]
async fn screenshot_result_uses_the_image_convention() {
    let fx = Fixture::new().await;
    fx.connect_fake_device("d-online");

    let out = fx
        .call(
            "chat-a",
            "remote_agent_screenshot",
            serde_json::json!({ "device_id": "d-online" }),
        )
        .await
        .unwrap();

    // routes/mcp.rs strips `_image_base64` into an MCP image block.
    assert_eq!(out["_image_base64"], "aGVsbG8=");
    assert_eq!(out["_image_mime"], "image/png");
    assert!(
        out["result"].get("image_base64").is_none(),
        "image bytes must not ride along in the text block"
    );
}

#[tokio::test]
async fn tools_are_unavailable_without_a_registry_handle() {
    let fx = Fixture::new().await;
    let mut ctx = fx.ctx("chat-a");
    ctx.device_registry = None;

    let err = fx
        .tools
        .handle_tool_call("remote_agent_list", serde_json::json!({}), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unavailable"), "got: {err}");
}

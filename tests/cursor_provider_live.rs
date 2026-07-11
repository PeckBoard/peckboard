//! Live end-to-end tests for the Cursor (`cursor-agent`) provider.
//!
//! These drive the **real** `cursor-agent` CLI, so they need it installed
//! (on `PATH`) and logged in (`cursor-agent status`), plus network access to
//! Cursor's backend. They cost real tokens and aren't deterministic, so they
//! are `#[ignore]`d and excluded from the normal `cargo test` run. Run them
//! explicitly when validating the provider against an installed CLI:
//!
//! ```bash
//! cargo test --test cursor_provider_live -- --ignored --nocapture
//! ```
//!
//! Coverage:
//! * `cursor_discovery_lists_latest_models` — model discovery shells out to
//!   `cursor-agent models`, parses its plain-text table, and surfaces the full
//!   live model list (not just the static seed).
//! * `cursor_auto_turn_completes_end_to_end` — a `cursor:auto` turn flows
//!   through the real dispatcher and event-log pipeline: start → text → end.

use std::sync::Arc;
use std::time::Duration;

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewSession};
use peckboard::plugin::settings::{PluginSettingsStore, SettingsSchema};
use peckboard::provider::agent::AgentProvider;
use peckboard::provider::cursor::{CursorProvider, default_models};
use peckboard::provider::manager::SessionManager;
use peckboard::provider::message::UserMessage;
use peckboard::provider::registry::{ProviderInfo, ProviderRegistry};
use peckboard::provider::stream::SpawnConfig;
use peckboard::ws::broadcaster::Broadcaster;

/// An empty settings schema yields exactly the provider's built-in defaults
/// (`cli_path = cursor-agent`, `discover_models = true`, `auto_approve = true`,
/// the default timeout), which is what production uses out of the box.
fn cursor_settings_store(db: Db) -> PluginSettingsStore {
    PluginSettingsStore::new("cursor", SettingsSchema::new(vec![]), db)
}

#[tokio::test]
#[ignore = "requires an authenticated cursor-agent CLI + network; run with --ignored"]
async fn cursor_discovery_lists_latest_models() {
    let db = Db::in_memory().unwrap();
    let provider = CursorProvider::new(cursor_settings_store(db));

    let models = provider
        .dynamic_models()
        .await
        .expect("discovery should return a model list");

    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    eprintln!("cursor discovered {} models: {ids:?}", ids.len());

    // The live list is far larger than the 10-model static seed; if discovery
    // had silently fallen back we'd see exactly the seed count.
    assert!(
        ids.len() > default_models().len(),
        "expected the live list to exceed the static seed ({} models), got {}",
        default_models().len(),
        ids.len(),
    );

    // `auto` is always present, and the registry requires bare (prefix-free) ids.
    assert!(ids.contains(&"auto"), "expected `auto` in {ids:?}");
    for m in &models {
        assert!(!m.id.contains(':'), "id {} must be prefix-free", m.id);
        assert!(!m.id.is_empty(), "empty model id parsed from CLI output");
        assert!(
            m.display_name.ends_with("(Cursor)"),
            "display name {:?} should be tagged",
            m.display_name,
        );
    }

    // Breadth check: the catalog spans multiple model families, proving we
    // parsed the whole table rather than a stray line.
    assert!(
        ids.iter().any(|id| id.contains("claude")),
        "expected a claude-family model in {ids:?}",
    );
    assert!(
        ids.iter().any(|id| id.contains("gpt")),
        "expected a gpt-family model in {ids:?}",
    );
}

#[tokio::test]
#[ignore = "requires an authenticated cursor-agent CLI + network; run with --ignored"]
async fn cursor_auto_turn_completes_end_to_end() {
    // Build a real dispatcher with the cursor provider registered.
    let registry = Arc::new(ProviderRegistry::new());
    let db = Db::in_memory().unwrap();
    let provider = Arc::new(CursorProvider::new(cursor_settings_store(db.clone())));
    registry
        .register(
            provider,
            ProviderInfo {
                id: "cursor".into(),
                display_name: "Cursor".into(),
                models: default_models(),
                effort_levels: vec![],
            },
        )
        .await;
    let manager = SessionManager::new(registry);

    let broadcaster = Broadcaster::new();
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "F".into(),
        path: "/tmp".into(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_session(NewSession {
        id: "s1".into(),
        name: "Cursor Live".into(),
        folder_id: "f1".into(),
        model: None,
        effort: None,
        is_worker: false,
        project_id: None,
        card_id: None,
        conversation_id: None,
        created_at: ts.clone(),
        last_activity: ts,
        ..Default::default()
    })
    .await
    .unwrap();

    let mut completion_rx = manager
        .take_completion_rx()
        .await
        .expect("completion rx available");

    let config = SpawnConfig {
        model: "cursor:auto".into(),
        effort: None,
        working_dir: "/tmp".into(),
        mcp_config_path: None,
        env: Default::default(),
        permission_mode: None,
        timeout_ms: None,
        metadata: serde_json::Value::Null,
        system_prompt_suffix: None,
        system_prompt_override: None,
        extra_allowed_tools: Vec::new(),
        is_worker: false,
        is_pre_hatcher: false,
    };

    manager
        .send_or_queue(
            "s1",
            UserMessage::from_text("Reply with exactly the single word: pong"),
            &db,
            &broadcaster,
            config,
        )
        .await
        .expect("dispatch succeeds");

    // A real model turn round-trips to Cursor's backend; allow plenty of time.
    let completion = tokio::time::timeout(Duration::from_secs(120), completion_rx.recv())
        .await
        .expect("turn completed before timeout")
        .expect("completion channel still open");
    assert_eq!(completion.session_id, "s1");
    assert!(
        completion.completed,
        "cursor:auto turn should report success",
    );

    let events = db.events_tail("s1", 100).await.unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    eprintln!("cursor turn events: {kinds:?}");

    assert_eq!(
        events.first().map(|e| e.kind.as_str()),
        Some("agent-start"),
        "first event should be agent-start: {kinds:?}",
    );
    assert_eq!(
        events.last().map(|e| e.kind.as_str()),
        Some("agent-end"),
        "last event should be agent-end: {kinds:?}",
    );
    assert!(
        kinds.contains(&"agent-text"),
        "expected at least one agent-text event: {kinds:?}",
    );

    // The end frame must report a clean completion.
    let end_data: serde_json::Value = serde_json::from_str(&events.last().unwrap().data).unwrap();
    assert_eq!(
        end_data["status"], "complete",
        "agent-end status should be complete: {end_data}",
    );

    // The assistant's reply should contain "pong".
    let said_pong = events.iter().filter(|e| e.kind == "agent-text").any(|e| {
        serde_json::from_str::<serde_json::Value>(&e.data)
            .ok()
            .and_then(|d| {
                d["text"]
                    .as_str()
                    .map(|s| s.to_lowercase().contains("pong"))
            })
            .unwrap_or(false)
    });
    assert!(said_pong, "expected an assistant reply containing 'pong'");

    // Cursor's chat id must be persisted so the next turn can `--resume`.
    let session = db.get_session("s1").await.unwrap().unwrap();
    assert!(
        session.conversation_id.is_some(),
        "expected a persisted conversation_id for resume",
    );
}

/// The model picker / `/api/models` route / MCP `list_models` tool all read
/// `ProviderRegistry::list_all_models()`. This proves the full live Cursor
/// catalog surfaces there with `cursor:` prefixes — i.e. the UI shows every
/// model, not just the static seed.
#[tokio::test]
#[ignore = "requires an authenticated cursor-agent CLI + network; run with --ignored"]
async fn cursor_models_surface_through_catalog_api() {
    let registry = ProviderRegistry::new();
    let db = Db::in_memory().unwrap();
    registry
        .register(
            Arc::new(CursorProvider::new(cursor_settings_store(db))),
            ProviderInfo {
                id: "cursor".into(),
                display_name: "Cursor".into(),
                models: default_models(),
                effort_levels: vec![],
            },
        )
        .await;

    // Exactly what GET /api/models and the MCP list_models tool consume.
    let catalog = registry.list_all_models().await;
    let cursor_ids: Vec<&str> = catalog
        .iter()
        .map(|(full_id, _)| full_id.as_str())
        .filter(|id| id.starts_with("cursor:"))
        .collect();
    eprintln!(
        "catalog exposes {} cursor models, e.g. {:?}",
        cursor_ids.len(),
        &cursor_ids[..cursor_ids.len().min(8)],
    );

    // The whole live list flows through, prefixed as the UI consumes it.
    assert!(
        cursor_ids.len() > default_models().len(),
        "catalog should expose the full live list, got {}",
        cursor_ids.len(),
    );
    assert!(cursor_ids.contains(&"cursor:auto"));
    // Latest families are all present and correctly prefixed.
    assert!(
        cursor_ids.iter().any(|id| id.contains("opus-4-8")),
        "expected an Opus 4.8 model in {cursor_ids:?}",
    );
    assert!(
        cursor_ids.iter().any(|id| id.contains("gpt-5.5")),
        "expected a GPT-5.5 model in {cursor_ids:?}",
    );
    // Every entry is provider-prefixed exactly once.
    for id in &cursor_ids {
        assert_eq!(id.matches(':').count(), 1, "malformed catalog id {id}");
    }
}

/// Full MCP round-trip through the workspace wiring: the provider writes a
/// secret-free `.cursor/mcp.json` (env-reference header), re-asserts server
/// approval, exports the per-session token as an env var, and the real
/// cursor-agent then calls a tool on a mock peckboard `/mcp` endpoint that
/// mirrors `routes/mcp.rs` conventions (plain JSON POST, 202 notifications).
#[tokio::test]
#[ignore = "requires an authenticated cursor-agent CLI + network; run with --ignored"]
async fn cursor_mcp_tool_roundtrip() {
    use axum::{Json, Router, http::HeaderMap, http::StatusCode, routing::post};

    // Surface the provider's tracing (MCP wiring warns etc.) in --nocapture.
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .try_init();

    const MAGIC: &str = "PECK-MAGIC-8241";
    const TOKEN: &str = "live-test-token-1234";

    // Authorization header of every request, to prove env interpolation.
    let auth_seen: Arc<tokio::sync::Mutex<Vec<String>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let auth_rec = auth_seen.clone();

    let app = Router::new().route(
        "/mcp",
        post(
            move |headers: HeaderMap, Json(body): Json<serde_json::Value>| {
                let auth_rec = auth_rec.clone();
                async move {
                    if let Some(a) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
                        auth_rec.lock().await.push(a.to_string());
                    }
                    // Notifications (no id) expect a bare 202, like routes/mcp.rs.
                    let Some(id) = body.get("id").cloned() else {
                        return (StatusCode::ACCEPTED, Json(serde_json::json!({})));
                    };
                    let result = match body.get("method").and_then(|m| m.as_str()).unwrap_or("") {
                        "initialize" => serde_json::json!({
                            "protocolVersion": body["params"]["protocolVersion"]
                                .as_str()
                                .unwrap_or("2024-11-05"),
                            "serverInfo": { "name": "peckboard", "version": "1.0.0" },
                            "capabilities": { "tools": {} },
                        }),
                        "tools/list" => serde_json::json!({
                            "tools": [{
                                "name": "peck_ping",
                                "description": "Returns the magic word. Call with no arguments.",
                                "inputSchema": { "type": "object", "properties": {} },
                            }]
                        }),
                        "tools/call" => serde_json::json!({
                            "content": [{ "type": "text", "text": MAGIC }]
                        }),
                        _ => serde_json::json!({}),
                    };
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })),
                    )
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // Session workspace + the per-session worker-mcp config the dispatcher
    // would have written (same shape as `write_mcp_config`).
    // cursor-agent silently drops MCP servers in untrusted workspaces, and
    // /tmp is untrusted by default — anchor the workspace under $HOME (the
    // verified-trusted zone; the turn also passes --trust via auto_approve).
    let home = std::env::var("HOME").expect("HOME set");
    let ws = tempfile::Builder::new()
        .prefix(".pb-cursor-mcp-live-")
        .tempdir_in(home)
        .unwrap();
    eprintln!("live ws = {}", ws.path().display());
    let mcp_cfg = ws.path().join("worker-mcp.json");
    std::fs::write(
        &mcp_cfg,
        serde_json::json!({
            "mcpServers": { "peckboard": {
                "type": "http",
                "url": format!("http://127.0.0.1:{port}/mcp"),
                "headers": { "Authorization": format!("Bearer {TOKEN}") },
            }}
        })
        .to_string(),
    )
    .unwrap();

    let registry = Arc::new(ProviderRegistry::new());
    let db = Db::in_memory().unwrap();
    let provider = Arc::new(CursorProvider::new(cursor_settings_store(db.clone())));
    registry
        .register(
            provider,
            ProviderInfo {
                id: "cursor".into(),
                display_name: "Cursor".into(),
                models: default_models(),
                effort_levels: vec![],
            },
        )
        .await;
    let manager = SessionManager::new(registry);

    let broadcaster = Broadcaster::new();
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: "f-mcp".into(),
        name: "F".into(),
        path: ws.path().to_string_lossy().to_string(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_session(NewSession {
        id: "s-mcp".into(),
        name: "Cursor MCP Live".into(),
        folder_id: "f-mcp".into(),
        model: None,
        effort: None,
        is_worker: false,
        project_id: None,
        card_id: None,
        conversation_id: None,
        created_at: ts.clone(),
        last_activity: ts,
        ..Default::default()
    })
    .await
    .unwrap();

    let mut completion_rx = manager
        .take_completion_rx()
        .await
        .expect("completion rx available");

    let config = SpawnConfig {
        model: "cursor:auto".into(),
        effort: None,
        working_dir: ws.path().to_string_lossy().to_string(),
        mcp_config_path: Some(mcp_cfg.to_string_lossy().to_string()),
        env: Default::default(),
        permission_mode: None,
        timeout_ms: None,
        metadata: serde_json::Value::Null,
        system_prompt_suffix: None,
        system_prompt_override: None,
        extra_allowed_tools: Vec::new(),
        is_worker: false,
        is_pre_hatcher: false,
    };

    manager
        .send_or_queue(
            "s-mcp",
            UserMessage::from_text(
                "Call the MCP tool peck_ping from the peckboard server, then reply \
                 with exactly the text it returns.",
            ),
            &db,
            &broadcaster,
            config,
        )
        .await
        .expect("dispatch succeeds");

    let completion = tokio::time::timeout(Duration::from_secs(180), completion_rx.recv())
        .await
        .expect("turn completed before timeout")
        .expect("completion channel still open");
    assert_eq!(completion.session_id, "s-mcp");
    assert!(
        completion.completed,
        "cursor MCP turn should report success"
    );

    // Debug: what the agent actually said, before the assertions below.
    for e in db.events_tail("s-mcp", 200).await.unwrap() {
        if e.kind == "agent-text" || e.kind == "agent-end" {
            eprintln!(
                "[{}] {}",
                e.kind,
                e.data.chars().take(400).collect::<String>()
            );
        }
    }
    let written = std::fs::read_to_string(ws.path().join(".cursor/mcp.json")).unwrap();
    assert!(
        written.contains("${env:PECKBOARD_MCP_TOKEN}"),
        "workspace config should reference the env var: {written}",
    );
    assert!(
        !written.contains(TOKEN),
        "workspace config must never contain the raw token: {written}",
    );

    // cursor-agent reached the endpoint with the interpolated session token.
    let seen = auth_seen.lock().await;
    assert!(
        !seen.is_empty(),
        "cursor-agent never called the MCP endpoint"
    );
    assert!(
        seen.iter().all(|a| a == &format!("Bearer {TOKEN}")),
        "unexpected Authorization values: {seen:?}",
    );
    drop(seen);

    // And the model surfaced the tool's output back into the event log.
    let events = db.events_tail("s-mcp", 200).await.unwrap();
    assert!(
        events.iter().any(|e| e.data.contains(MAGIC)),
        "expected the tool's magic word in the event log",
    );
}

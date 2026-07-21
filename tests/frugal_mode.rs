//! Integration tests for cost-aware model auto-switch ("Frugal Mode"):
//! the system-prompt library CRUD, and the `get_model_guidance` /
//! `switch_session_model` MCP tools. All state is an in-memory DB with the
//! mock provider, whose models carry a real tier ladder
//! (echo=1 < tool-use=2 < happy-path=3), so a "cheaper but capable"
//! candidate genuinely exists without touching a real CLI.

use std::sync::Arc;

use peckboard::auth::rate_limit::RateLimiter;
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewSession};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::mock::register_mock_provider;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::service::mcp_server::{McpTokenRegistry, McpToolRegistry, ToolCallContext};
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;

async fn build_state() -> Arc<AppState> {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    std::mem::forget(tmp);

    let registry = Arc::new(ProviderRegistry::new());
    register_mock_provider(&registry).await;

    let db = Db::in_memory().unwrap();
    let plugins = Arc::new(PluginManager::new(&data_dir, db.clone()));
    let session_manager = SessionManager::new(registry.clone()).with_plugins(plugins.clone());

    Arc::new(AppState {
        env_unlock: Arc::new(peckboard::service::env_vars::EnvUnlockRegistry::new()),
        config: Config {
            port: 0,
            https_port: 0,
            host: "127.0.0.1".into(),
            data_dir: data_dir.clone(),
            mdns: false,
            keep_alive_hours: 0,
            provider_send_timeout_secs: 300,
        },
        db,
        plugins,
        builtin_plugins: Arc::new(BuiltinPluginRegistry::new()),
        jwt_secret: vec![0u8; 32],
        login_limiter: RateLimiter::new(100),
        password_change_limiter: RateLimiter::new(100),
        broadcaster: Broadcaster::new(),
        provider_registry: registry,
        session_manager,
        repeating_task_manager: peckboard::repeating::RepeatingTaskManager::new(),
        run_auditor: peckboard::repeating::RunAuditor::new(),
        mcp_tokens: McpTokenRegistry::new(),
        push_service: PushService::new(&data_dir),
    })
}

/// A worker session with an explicit model and toggle. Returns its id.
async fn seed_worker(
    state: &Arc<AppState>,
    model: &str,
    is_worker: bool,
    autoswitch: Option<bool>,
) -> String {
    let ts = chrono::Utc::now().to_rfc3339();
    let id = uuid::Uuid::new_v4().to_string();
    state
        .db
        .create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp".into(),
            created_at: ts.clone(),
        })
        .await
        .ok();
    state
        .db
        .create_session(NewSession {
            id: id.clone(),
            name: "worker".into(),
            folder_id: "f1".into(),
            model: Some(model.to_string()),
            is_worker,
            created_at: ts.clone(),
            last_activity: ts,
            model_autoswitch: autoswitch,
            ..Default::default()
        })
        .await
        .unwrap();
    id
}

fn ctx(state: &Arc<AppState>, session_id: &str) -> ToolCallContext {
    ToolCallContext {
        session_id: session_id.to_string(),
        project_id: None,
        card_id: None,
        db: Arc::new(state.db.clone()),
        broadcaster: state.broadcaster.clone(),
        provider_registry: Some(state.provider_registry.clone()),
        data_dir: None,
        folder_id: "f1".into(),
    }
}

// ── System-prompt library ─────────────────────────────────────────────

#[tokio::test]
async fn system_prompt_crud_and_upsert() {
    let state = build_state().await;
    let db = &state.db;

    let p = db
        .create_system_prompt("implement", "Do the thing.", None)
        .await
        .unwrap();
    assert_eq!(p.name, "implement");

    // Duplicate name is a UNIQUE violation on create.
    assert!(
        db.create_system_prompt("implement", "again", None)
            .await
            .is_err()
    );

    // Resolve by name (the switch/set-prompt path).
    let got = db.get_system_prompt_by_name("implement").await.unwrap();
    assert_eq!(got.unwrap().body, "Do the thing.");

    // Upsert-by-name refreshes the body in place, keeps a single row.
    let up = db
        .upsert_system_prompt_by_name("implement", "Do it well.", Some("https://x/y"))
        .await
        .unwrap();
    assert_eq!(up.id, p.id);
    assert_eq!(up.body, "Do it well.");
    assert_eq!(db.list_system_prompts().await.unwrap().len(), 1);

    assert!(db.delete_system_prompt(&p.id).await.unwrap());
    assert!(db.list_system_prompts().await.unwrap().is_empty());
}

#[tokio::test]
async fn seed_defaults_backfills_missing_without_clobbering() {
    let state = build_state().await;
    let db = &state.db;
    let n = db.seed_default_system_prompts().await.unwrap();
    assert!(n >= 6, "seeds implement/research/debug/review/docs/fable 5");
    // Re-running with the full library present inserts nothing.
    assert_eq!(db.seed_default_system_prompts().await.unwrap(), 0);

    // A user edit to a builtin survives re-seeding (create-if-missing).
    let impl_prompt = db
        .get_system_prompt_by_name("implement")
        .await
        .unwrap()
        .unwrap();
    db.update_system_prompt(&impl_prompt.id, None, Some("edited"), None)
        .await
        .unwrap();

    // Deleting a builtin and re-seeding backfills only that one.
    let fable = db
        .get_system_prompt_by_name("fable 5")
        .await
        .unwrap()
        .unwrap();
    assert!(db.delete_system_prompt(&fable.id).await.unwrap());
    assert_eq!(db.seed_default_system_prompts().await.unwrap(), 1);

    assert!(
        db.get_system_prompt_by_name("fable 5")
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        db.get_system_prompt_by_name("implement")
            .await
            .unwrap()
            .unwrap()
            .body,
        "edited"
    );
}

// ── get_model_guidance ────────────────────────────────────────────────

#[tokio::test]
async fn guidance_lists_cheaper_same_provider_candidates() {
    let state = build_state().await;
    let sid = seed_worker(&state, "mock:happy-path", true, None).await;
    let reg = McpToolRegistry::new();

    let out = reg
        .handle_tool_call(
            "get_model_guidance",
            serde_json::json!({}),
            &ctx(&state, &sid),
        )
        .await
        .unwrap();

    assert_eq!(out["current_model"], "mock:happy-path");
    assert_eq!(out["current_tier"], 3);
    let candidates = out["candidates"].as_array().unwrap();
    assert!(!candidates.is_empty(), "lower-tier mock models exist");
    // Every candidate is same provider (mock) and strictly lower tier.
    for c in candidates {
        let m = c["model"].as_str().unwrap();
        assert!(m.starts_with("mock:"));
        assert!(c["tier"].as_i64().unwrap() < 3);
    }
}

// ── switch_session_model ──────────────────────────────────────────────

#[tokio::test]
async fn switch_downgrades_and_records_event_and_prompt() {
    let state = build_state().await;
    state
        .db
        .create_system_prompt("implement", "Focus.", None)
        .await
        .unwrap();
    let sid = seed_worker(&state, "mock:happy-path", true, None).await;
    let reg = McpToolRegistry::new();

    let out = reg
        .handle_tool_call(
            "switch_session_model",
            serde_json::json!({
                "model": "mock:echo",
                "rationale": "plan is trivial",
                "system_prompt_name": "implement"
            }),
            &ctx(&state, &sid),
        )
        .await
        .unwrap();
    assert_eq!(out["status"], "ok");
    assert_eq!(out["to"], "mock:echo");

    // Session row now carries the new model AND the library prompt body.
    let s = state.db.get_session(&sid).await.unwrap().unwrap();
    assert_eq!(s.model.as_deref(), Some("mock:echo"));
    assert_eq!(s.system_prompt.as_deref(), Some("Focus."));

    // A model-switch event was recorded with the rationale.
    let events = state.db.list_events_by_session(&sid, None).await.unwrap();
    let sw = events.iter().find(|e| e.kind == "model-switch").unwrap();
    let data: serde_json::Value = serde_json::from_str(&sw.data).unwrap();
    assert_eq!(data["from"], "mock:happy-path");
    assert_eq!(data["to"], "mock:echo");
    assert_eq!(data["rationale"], "plan is trivial");
    assert_eq!(data["system_prompt_name"], "implement");
}

#[tokio::test]
async fn switch_refused_when_toggle_off() {
    let state = build_state().await;
    // A chat session (is_worker=false) defaults OFF.
    let sid = seed_worker(&state, "mock:happy-path", false, None).await;
    let reg = McpToolRegistry::new();
    let err = reg
        .handle_tool_call(
            "switch_session_model",
            serde_json::json!({ "model": "mock:echo", "rationale": "x" }),
            &ctx(&state, &sid),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("disabled"), "got: {err}");
}

#[tokio::test]
async fn switch_refused_cross_provider() {
    let state = build_state().await;
    let sid = seed_worker(&state, "mock:happy-path", true, None).await;
    let reg = McpToolRegistry::new();
    let err = reg
        .handle_tool_call(
            "switch_session_model",
            serde_json::json!({ "model": "claude:claude-haiku-4-5", "rationale": "x" }),
            &ctx(&state, &sid),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("same provider"), "got: {err}");
}

#[tokio::test]
async fn switch_cap_blocks_flip_flopping() {
    let state = build_state().await;
    let sid = seed_worker(&state, "mock:happy-path", true, None).await;
    let reg = McpToolRegistry::new();

    // Three switches are allowed; each toggles between tiers.
    let targets = ["mock:echo", "mock:tool-use", "mock:echo"];
    for t in targets {
        reg.handle_tool_call(
            "switch_session_model",
            serde_json::json!({ "model": t, "rationale": "r" }),
            &ctx(&state, &sid),
        )
        .await
        .unwrap();
    }
    // The fourth is refused by the per-session cap.
    let err = reg
        .handle_tool_call(
            "switch_session_model",
            serde_json::json!({ "model": "mock:tool-use", "rationale": "r" }),
            &ctx(&state, &sid),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("cap"), "got: {err}");
}

#[tokio::test]
async fn resolve_system_prompt_semantics() {
    let state = build_state().await;
    let db = &state.db;
    db.seed_default_system_prompts().await.unwrap();

    // None / empty / whitespace => no prompt. This is the signal the
    // create/edit forms send to clear a selection (empty string over HTTP).
    assert!(db.resolve_system_prompt(None).await.unwrap().is_none());
    assert!(db.resolve_system_prompt(Some("")).await.unwrap().is_none());
    assert!(
        db.resolve_system_prompt(Some("   "))
            .await
            .unwrap()
            .is_none()
    );

    // Unknown non-empty name is an error (surfaced as a 400 by the routes).
    assert!(
        db.resolve_system_prompt(Some("does-not-exist"))
            .await
            .is_err()
    );

    // Known name resolves to (name, body) — both move together onto a row.
    let (name, body) = db
        .resolve_system_prompt(Some("fable 5"))
        .await
        .unwrap()
        .expect("fable 5 resolves");
    assert_eq!(name, "fable 5");
    assert!(!body.is_empty());
}

#[tokio::test]
async fn switch_compact_signals_handover_and_defers_model() {
    let state = build_state().await;
    state
        .db
        .create_system_prompt("review", "Review it.", None)
        .await
        .unwrap();
    // Cheaper model finished; upgrade back UP to the stronger one to review.
    let sid = seed_worker(&state, "mock:echo", true, None).await;
    let reg = McpToolRegistry::new();

    let out = reg
        .handle_tool_call(
            "switch_session_model",
            serde_json::json!({
                "model": "mock:happy-path",
                "rationale": "cheap model finished; upgrade for review",
                "system_prompt_name": "review",
                "compact": true
            }),
            &ctx(&state, &sid),
        )
        .await
        .unwrap();

    assert_eq!(out["status"], "ok");
    assert_eq!(out["compact"], true);
    // The handler signals the route to run a compacting handover rather
    // than writing the model itself.
    assert_eq!(out["_begin_handover"]["from"], "mock:echo");
    assert_eq!(out["_begin_handover"]["to"], "mock:happy-path");

    // Model is NOT flipped here — finalize_handover does that once the
    // compaction doc lands. The focusing review prompt IS applied now, so
    // the stronger model resumes in review mode.
    let s = state.db.get_session(&sid).await.unwrap().unwrap();
    assert_eq!(s.model.as_deref(), Some("mock:echo"));
    assert_eq!(s.system_prompt.as_deref(), Some("Review it."));

    // The switch is still recorded (drives the per-session cap + report),
    // tagged as a compacting switch.
    let events = state.db.list_events_by_session(&sid, None).await.unwrap();
    let sw = events.iter().find(|e| e.kind == "model-switch").unwrap();
    let data: serde_json::Value = serde_json::from_str(&sw.data).unwrap();
    assert_eq!(data["compact"], true);
    assert_eq!(data["to"], "mock:happy-path");
}

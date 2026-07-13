//! End-to-end tests of **plugin-registered AI providers** against the real
//! core host functions. Loads the compiled scripted provider plugin
//! (`peck-plugins/provider-test`), approves it, syncs it into a
//! `ProviderRegistry`, and drives turns through the resulting
//! `PluginProviderAdapter` — exercising `provider.register` / `provider.send`
//! dispatch plus the provider host functions (`peckboard_register_provider`,
//! `peckboard_emit_provider_event`, `peckboard_provider_should_stop`,
//! `peckboard_provider_get_session`, `peckboard_provider_get_mcp_config`).
//!
//! The wasm is built out-of-tree (`peck-plugins/provider-test/build.sh`) and
//! this repo's `cargo test` has no `wasm32` toolchain, so the tests **skip**
//! with a note when the artifact is absent.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewSession};
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::agent::SendMessageContext;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::provider::stream::SpawnConfig;
use peckboard::ws::broadcaster::Broadcaster;
use tokio::sync::mpsc;

const PLUGIN_ID: &str = "provider-test";
const PROVIDER_ID: &str = "wasmtest";

fn plugin_wasm() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "../peck-plugins/provider-test/target/wasm32-unknown-unknown/release/\
         peckboard_provider_test_plugin.wasm",
    );
    p.exists().then_some(p)
}

/// Load + approve the provider-test plugin, bind a fresh registry, and sync
/// so the `wasmtest` provider is registered. Returns everything a turn needs.
async fn setup(data_dir: &Path) -> (Db, Arc<PluginManager>, Arc<ProviderRegistry>) {
    let plugins_dir = data_dir.join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::copy(
        plugin_wasm().unwrap(),
        plugins_dir.join(format!("{PLUGIN_ID}.wasm")),
    )
    .unwrap();

    let db = Db::open(data_dir).unwrap();
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "F".into(),
        path: "/tmp/pt".into(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_session(NewSession {
        id: "s1".into(),
        name: "S".into(),
        folder_id: "f1".into(),
        model: Some(format!("{PROVIDER_ID}:echo-1")),
        created_at: ts.clone(),
        last_activity: ts,
        ..Default::default()
    })
    .await
    .unwrap();

    let plugins = Arc::new(PluginManager::new(data_dir, db.clone()));
    plugins.load_all().await.unwrap();
    let info = plugins
        .decide(PLUGIN_ID, true)
        .await
        .unwrap()
        .expect("provider-test plugin should be loaded");
    assert_eq!(info.status, "approved", "plugin must be active: {info:?}");

    let registry = Arc::new(ProviderRegistry::new());
    plugins.set_provider_registry(&registry);
    plugins.sync_plugin_providers().await;
    (db, plugins, registry)
}

/// Dispatch one turn through the adapter and wait for its completion signal.
async fn run_turn(
    db: &Db,
    plugins: &Arc<PluginManager>,
    registry: &Arc<ProviderRegistry>,
    broadcaster: &Arc<Broadcaster>,
    text: &str,
    conversation_id: Option<String>,
) -> peckboard::provider::agent::ProcessCompletion {
    let provider = registry
        .get_provider(PROVIDER_ID)
        .await
        .expect("wasmtest provider registered");
    let (completion_tx, mut completion_rx) = mpsc::channel(8);
    let ctx = SendMessageContext {
        session_id: "s1".into(),
        message: text.into(),
        db: db.clone(),
        broadcaster: broadcaster.clone(),
        config: SpawnConfig {
            model: format!("{PROVIDER_ID}:echo-1"),
            working_dir: "/tmp/pt".into(),
            mcp_config_path: Some("/tmp/pt-mcp.json".into()),
            ..Default::default()
        },
        conversation_id,
        completion_tx,
        plugins: plugins.clone(),
    };
    provider.send_message(ctx).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), completion_rx.recv())
        .await
        .expect("completion within timeout")
        .expect("completion channel open")
}

fn text_events(events: &[peckboard::db::models::Event]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.kind == "agent-text")
        .map(|e| {
            let v: serde_json::Value = serde_json::from_str(&e.data).unwrap();
            v["text"].as_str().unwrap_or_default().to_string()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_plugin_registers_catalog_and_prices() {
    let Some(_) = plugin_wasm() else {
        eprintln!("SKIP provider_plugin_registers_catalog_and_prices: wasm not built");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (_db, plugins, registry) = setup(dir.path()).await;

    let info = registry
        .get_info(PROVIDER_ID)
        .await
        .expect("provider registered");
    assert_eq!(info.display_name, "WASM Test Provider");
    let model_ids: Vec<&str> = info.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(model_ids, ["echo-1", "echo-mini"]);
    assert_eq!(info.effort_levels.len(), 2);

    // Models surface through the same catalog walk /api/models uses.
    let all = registry.list_all_models().await;
    assert!(all.iter().any(|(id, _)| id == "wasmtest:echo-1"), "{all:?}");

    // Pricing map backs model_price; unpriced models stay unknown.
    let provider = registry.get_provider(PROVIDER_ID).await.unwrap();
    assert_eq!(provider.model_price("echo-1"), Some((1.0, 2.0)));
    assert_eq!(provider.model_price("echo-mini"), None);

    // Deny → sync unregisters the provider.
    plugins.decide(PLUGIN_ID, false).await.unwrap();
    plugins.sync_plugin_providers().await;
    assert!(registry.get_info(PROVIDER_ID).await.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_plugin_cannot_displace_native_provider() {
    let Some(_) = plugin_wasm() else {
        eprintln!("SKIP provider_plugin_cannot_displace_native_provider: wasm not built");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let plugins_dir = dir.path().join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::copy(
        plugin_wasm().unwrap(),
        plugins_dir.join(format!("{PLUGIN_ID}.wasm")),
    )
    .unwrap();
    let db = Db::open(dir.path()).unwrap();
    let plugins = Arc::new(PluginManager::new(dir.path(), db));
    plugins.load_all().await.unwrap();
    plugins.decide(PLUGIN_ID, true).await.unwrap();

    // A "native" provider already owns the id the plugin wants.
    let registry = Arc::new(ProviderRegistry::new());
    registry
        .register(
            Arc::new(peckboard::provider::mock::MockProvider::new()),
            peckboard::provider::registry::ProviderInfo {
                id: PROVIDER_ID.into(),
                display_name: "Native".into(),
                models: vec![],
                effort_levels: vec![],
            },
        )
        .await;
    plugins.set_provider_registry(&registry);
    plugins.sync_plugin_providers().await;

    let info = registry.get_info(PROVIDER_ID).await.unwrap();
    assert_eq!(info.display_name, "Native", "plugin must not displace it");
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_send_streams_events_resumes_and_guards_foreign_sessions() {
    let Some(_) = plugin_wasm() else {
        eprintln!(
            "SKIP provider_send_streams_events_resumes_and_guards_foreign_sessions: wasm not built"
        );
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (db, plugins, registry) = setup(dir.path()).await;
    let broadcaster = Broadcaster::new();
    let mut ws_rx = broadcaster.subscribe_all();

    // Turn 1: context host fns + normal stream.
    let done = run_turn(&db, &plugins, &registry, &broadcaster, "getctx hello", None).await;
    assert!(done.completed, "turn should complete: {:?}", done.error);

    let events = db.events_tail("s1", 32).await.unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert!(kinds.contains(&"agent-start"), "{kinds:?}");
    assert!(kinds.contains(&"agent-text"), "{kinds:?}");
    assert!(kinds.contains(&"agent-usage"), "{kinds:?}");
    assert!(kinds.contains(&"agent-end"), "{kinds:?}");
    let texts = text_events(&events);
    assert!(texts[0].contains("echo: getctx hello"), "{texts:?}");
    // peckboard_provider_get_session served the trusted snapshot…
    assert!(texts[0].contains("/tmp/pt"), "{texts:?}");
    // …and peckboard_provider_get_mcp_config the SpawnConfig's path.
    assert!(texts[0].contains("pt-mcp.json"), "{texts:?}");

    // Usage landed in the usage_events table via the shared emit path.
    let usage = db.usage_events_for_session("s1").await.unwrap();
    assert_eq!(usage.len(), 1, "{usage:?}");
    assert_eq!(usage[0].input_tokens, 10);
    assert_eq!(usage[0].output_tokens, 5);

    // Completed persisted the conversation id on the session row.
    let session = db.get_session("s1").await.unwrap().unwrap();
    assert_eq!(session.conversation_id.as_deref(), Some("conv-fixed"));

    // Events were broadcast over WS while the wasm call was in flight.
    let mut ws_kinds = Vec::new();
    while let Ok(ev) = ws_rx.try_recv() {
        if ev.session_id == "s1" {
            ws_kinds.push(ev.data["kind"].as_str().unwrap_or_default().to_string());
        }
    }
    assert!(ws_kinds.iter().any(|k| k == "agent-text"), "{ws_kinds:?}");
    assert!(ws_kinds.iter().any(|k| k == "agent-end"), "{ws_kinds:?}");

    // Turn 2: resume — the dispatcher hands back the stored conversation id
    // and the plugin sees it in the provider.send payload.
    let done = run_turn(
        &db,
        &plugins,
        &registry,
        &broadcaster,
        "resume-check",
        session.conversation_id.clone(),
    )
    .await;
    assert!(done.completed);
    let events = db.events_tail("s1", 64).await.unwrap();
    let texts = text_events(&events);
    assert!(
        texts.iter().any(|t| t.contains("cid:conv-fixed")),
        "conversation_id must round-trip to the plugin: {texts:?}"
    );

    // Turn 3: emitting into a session this plugin's turn doesn't own is
    // refused by the host, and nothing lands in that session's log.
    let done = run_turn(&db, &plugins, &registry, &broadcaster, "foreign", None).await;
    assert!(done.completed);
    let events = db.events_tail("s1", 64).await.unwrap();
    let texts = text_events(&events);
    assert!(
        texts.iter().any(|t| t.contains("foreign:Err")),
        "foreign emit must be refused: {texts:?}"
    );
    let foreign = db.events_tail("not-my-session", 8).await.unwrap();
    assert!(foreign.is_empty(), "{foreign:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_send_crash_trap_and_interrupt_do_not_wedge_the_session() {
    let Some(_) = plugin_wasm() else {
        eprintln!(
            "SKIP provider_send_crash_trap_and_interrupt_do_not_wedge_the_session: wasm not built"
        );
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (db, plugins, registry) = setup(dir.path()).await;
    let broadcaster = Broadcaster::new();
    let provider = registry.get_provider(PROVIDER_ID).await.unwrap();

    // Scripted Crashed event: completion reports it, run is torn down.
    let done = run_turn(&db, &plugins, &registry, &broadcaster, "crash", None).await;
    assert!(!done.completed);
    assert_eq!(done.error.as_deref(), Some("scripted crash"));
    assert!(!provider.is_running("s1").await);

    // Trap (the wasm call itself errors, no terminal event): the adapter
    // synthesizes Crashed and the session is dispatchable again.
    let done = run_turn(&db, &plugins, &registry, &broadcaster, "trap", None).await;
    assert!(!done.completed);
    assert!(done.error.is_some(), "trap must surface an error");
    assert!(!provider.is_running("s1").await);
    let events = db.events_tail("s1", 8).await.unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.kind, "agent-end");
    assert!(last.data.contains("crashed"), "{}", last.data);

    // Interrupt: the stalled turn polls peckboard_provider_should_stop and
    // winds down once the adapter sets the stop flag.
    let (completion_tx, mut completion_rx) = mpsc::channel(8);
    let ctx = SendMessageContext {
        session_id: "s1".into(),
        message: "stall".into(),
        db: db.clone(),
        broadcaster: broadcaster.clone(),
        config: SpawnConfig {
            model: format!("{PROVIDER_ID}:echo-1"),
            working_dir: "/tmp/pt".into(),
            ..Default::default()
        },
        conversation_id: None,
        completion_tx,
        plugins: plugins.clone(),
    };
    provider.send_message(ctx).await.unwrap();
    // The turn is in flight (the plugin is spinning on the stop poll).
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(provider.is_running("s1").await);
    provider.interrupt("s1").await;
    let done = tokio::time::timeout(Duration::from_secs(30), completion_rx.recv())
        .await
        .expect("interrupted turn completes")
        .unwrap();
    assert!(!done.completed);
    assert_eq!(done.error.as_deref(), Some("interrupted"));
    provider.wait_for_termination("s1").await;
    assert!(!provider.is_running("s1").await);

    // And a normal turn still works afterwards — the session isn't wedged.
    let done = run_turn(&db, &plugins, &registry, &broadcaster, "hello again", None).await;
    assert!(done.completed, "{:?}", done.error);
}

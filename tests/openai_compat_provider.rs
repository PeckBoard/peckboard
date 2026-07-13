//! Integration test for the openai-compat WASM plugin provider.
//!
//! Builds a minimal tokio TCP stub that speaks just enough of the
//! OpenAI /chat/completions API to drive one full turn through the plugin,
//! then asserts the expected event kinds + usage_events row land in the DB.
//!
//! Skips when the plugin WASM isn't built (run `peck-plugins/openai-compat/build.sh` first).

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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

const PLUGIN_ID: &str = "openai-compat";
const PROVIDER_ID: &str = "openai-compat";
const MODEL_ID: &str = "stub-model";

fn plugin_wasm() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../peck-plugins/openai-compat/dist/plugin.wasm");
    p.exists().then_some(p)
}

/// Minimal stub: reads one HTTP request, serves a canned chat/completions JSON.
async fn spawn_stub_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 8192];
            let mut total = Vec::new();
            loop {
                let n =
                    match tokio::time::timeout(Duration::from_secs(5), sock.read(&mut buf)).await {
                        Ok(Ok(n)) if n > 0 => n,
                        _ => break,
                    };
                total.extend_from_slice(&buf[..n]);
                // Stop reading when the full HTTP body has arrived.
                if let Some(hdr_end) = find_subseq(&total, b"\r\n\r\n") {
                    let hdr = String::from_utf8_lossy(&total[..hdr_end]);
                    let cl = hdr.lines().find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    });
                    if let Some(len) = cl {
                        if total.len() >= hdr_end + 4 + len {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            }
            let body = serde_json::json!({
                "choices": [{"message": {"content": "hello from stub", "tool_calls": null}}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
                "model": MODEL_ID
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.write_all(body.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    format!("http://{}", addr)
}

fn find_subseq(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

async fn setup(data_dir: &Path, base_url: &str) -> (Db, Arc<PluginManager>, Arc<ProviderRegistry>) {
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
        path: "/tmp/oc".into(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_session(NewSession {
        id: "s1".into(),
        name: "S".into(),
        folder_id: "f1".into(),
        model: Some(format!("{PROVIDER_ID}:{MODEL_ID}")),
        created_at: ts.clone(),
        last_activity: ts,
        ..Default::default()
    })
    .await
    .unwrap();

    // Seed settings before sync so provider.register reads them.
    db.set_plugin_setting(
        PLUGIN_ID,
        "base_url",
        &serde_json::Value::String(base_url.to_string()),
    )
    .await
    .unwrap();
    db.set_plugin_setting(PLUGIN_ID, "models", &serde_json::json!([MODEL_ID]))
        .await
        .unwrap();
    db.set_plugin_setting(
        PLUGIN_ID,
        "display_name",
        &serde_json::Value::String("OpenAI Stub".to_string()),
    )
    .await
    .unwrap();

    let plugins = Arc::new(PluginManager::new(data_dir, db.clone()));
    plugins.load_all().await.unwrap();
    let info = plugins
        .decide(PLUGIN_ID, true)
        .await
        .unwrap()
        .expect("openai-compat plugin must load");
    assert_eq!(info.status, "approved", "plugin must be active: {info:?}");

    let registry = Arc::new(ProviderRegistry::new());
    plugins.set_provider_registry(&registry);
    plugins.sync_plugin_providers().await;
    (db, plugins, registry)
}

async fn run_turn(
    db: &Db,
    plugins: &Arc<PluginManager>,
    registry: &Arc<ProviderRegistry>,
    broadcaster: &Arc<Broadcaster>,
    message: &str,
    conversation_id: Option<String>,
) -> peckboard::provider::agent::ProcessCompletion {
    let provider = registry
        .get_provider(PROVIDER_ID)
        .await
        .expect("openai-compat provider registered");
    let (completion_tx, mut completion_rx) = mpsc::channel(8);
    let ctx = SendMessageContext {
        session_id: "s1".into(),
        message: message.into(),
        db: db.clone(),
        broadcaster: broadcaster.clone(),
        config: SpawnConfig {
            model: format!("{PROVIDER_ID}:{MODEL_ID}"),
            working_dir: "/tmp/oc".into(),
            ..Default::default()
        },
        conversation_id,
        completion_tx,
        plugins: plugins.clone(),
    };
    provider.send_message(ctx).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), completion_rx.recv())
        .await
        .expect("completion within 30s")
        .expect("completion channel open")
}

#[tokio::test(flavor = "multi_thread")]
async fn openai_compat_registers_and_drives_a_turn() {
    let Some(_) = plugin_wasm() else {
        eprintln!("SKIP openai_compat_registers_and_drives_a_turn: wasm not built");
        return;
    };
    let base_url = spawn_stub_server().await;
    let dir = tempfile::tempdir().unwrap();
    let (db, plugins, registry) = setup(dir.path(), &base_url).await;

    // Provider registered with the configured model.
    let info = registry
        .get_info(PROVIDER_ID)
        .await
        .expect("openai-compat provider registered");
    assert_eq!(info.display_name, "OpenAI Stub");
    assert!(
        info.models.iter().any(|m| m.id == MODEL_ID),
        "model {MODEL_ID} must be registered: {:?}",
        info.models
    );

    let broadcaster = Broadcaster::new();
    let done = run_turn(&db, &plugins, &registry, &broadcaster, "hello", None).await;
    assert!(done.completed, "turn must complete: {:?}", done.error);

    // Correct event sequence in the DB.
    let events = db.events_tail("s1", 32).await.unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert!(
        kinds.contains(&"agent-start"),
        "missing agent-start: {kinds:?}"
    );
    assert!(
        kinds.contains(&"agent-text"),
        "missing agent-text: {kinds:?}"
    );
    assert!(
        kinds.contains(&"agent-usage"),
        "missing agent-usage: {kinds:?}"
    );
    assert!(kinds.contains(&"agent-end"), "missing agent-end: {kinds:?}");

    // Text content matches stub response.
    let text_event = events.iter().find(|e| e.kind == "agent-text").unwrap();
    let text_data: serde_json::Value = serde_json::from_str(&text_event.data).unwrap();
    assert_eq!(text_data["text"], "hello from stub");

    // Usage row in usage_events table.
    let usage = db.usage_events_for_session("s1").await.unwrap();
    assert_eq!(usage.len(), 1, "expected 1 usage row: {usage:?}");
    assert_eq!(usage[0].input_tokens, 10);
    assert_eq!(usage[0].output_tokens, 5);
    assert_eq!(usage[0].total_tokens, 15);
}

#[tokio::test(flavor = "multi_thread")]
async fn openai_compat_resumes_conversation() {
    let Some(_) = plugin_wasm() else {
        eprintln!("SKIP openai_compat_resumes_conversation: wasm not built");
        return;
    };
    let base_url = spawn_stub_server().await;
    let dir = tempfile::tempdir().unwrap();
    let (db, plugins, registry) = setup(dir.path(), &base_url).await;
    let broadcaster = Broadcaster::new();

    // Turn 1: no conversation_id yet.
    let done = run_turn(&db, &plugins, &registry, &broadcaster, "turn 1", None).await;
    assert!(done.completed);

    // Completed event carries conversation_id; session row persists it.
    let session = db.get_session("s1").await.unwrap().unwrap();
    let conv_id = session.conversation_id.expect("conversation_id persisted");

    // Turn 2: resume with stored conversation_id.
    let done2 = run_turn(
        &db,
        &plugins,
        &registry,
        &broadcaster,
        "turn 2",
        Some(conv_id.clone()),
    )
    .await;
    assert!(
        done2.completed,
        "resume turn must complete: {:?}",
        done2.error
    );

    // Exactly 2 usage rows now.
    let usage = db.usage_events_for_session("s1").await.unwrap();
    assert_eq!(usage.len(), 2, "expected 2 usage rows: {usage:?}");
}

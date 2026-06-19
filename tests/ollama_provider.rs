//! End-to-end test for the Ollama provider against a stub HTTP server.
//!
//! Spins up a tiny TCP listener that speaks just enough of Ollama's
//! `/api/chat` streaming protocol to drive the provider through one
//! `Started → Text → Completed` cycle. The point is not to test
//! Ollama itself — it's to prove that:
//!
//! * the provider reads its base URL from the plugin settings store,
//! * outbound additional headers configured in settings actually land
//!   in the request,
//! * NDJSON chunks become `ProviderEvent::Text`s in the order received,
//!   and
//! * the `done: true` marker triggers a `ProviderEvent::Completed`
//!   plus a `ProcessCompletion { completed: true }` to the orchestrator.

use std::sync::Arc;
use std::time::Duration;

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewSession};
use peckboard::plugin::settings::{FieldKind, PluginSettingsStore, SettingField, SettingsSchema};
use peckboard::provider::agent::{AgentProvider, SendMessageContext};
use peckboard::provider::ollama::OllamaProvider;
use peckboard::provider::stream::SpawnConfig;
use peckboard::ws::broadcaster::Broadcaster;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc};

/// Schema used to seed the in-memory settings store for the test. Has
/// to mirror the real Ollama plugin's schema for the `base_url` field
/// or the provider rejects the configuration.
fn test_schema() -> SettingsSchema {
    SettingsSchema::new(vec![
        SettingField {
            key: "base_url".into(),
            title: "Base URL".into(),
            description: None,
            required: true,
            kind: FieldKind::Url {
                default: Some("http://localhost:11434".into()),
                placeholder: None,
            },
        },
        SettingField {
            key: "additional_headers".into(),
            title: "Headers".into(),
            description: None,
            required: false,
            kind: FieldKind::KeyValueList {
                secret_values: true,
                key_placeholder: None,
                value_placeholder: None,
            },
        },
    ])
}

/// Stub server. Accepts a single connection, reads the HTTP request,
/// captures the headers + path the provider sent (so the test can
/// assert against them later), then streams the canned NDJSON body
/// back as a chunked response.
async fn spawn_stub_ollama() -> (String, Arc<Mutex<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_ret = captured.clone();
    tokio::spawn(async move {
        let (mut sock, _peer) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let mut total = Vec::new();
        // Read until we see the end of the HTTP request body. The
        // provider sends a Content-Length + complete body in one go, so
        // this loop generally only reads once.
        loop {
            let n = match tokio::time::timeout(Duration::from_secs(5), sock.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => n,
                _ => break,
            };
            total.extend_from_slice(&buf[..n]);
            // Quick heuristic: if we've seen the headers terminator AND
            // the body length matches Content-Length, we're done.
            if let Some(headers_end) = find_subseq(&total, b"\r\n\r\n") {
                let headers_text = String::from_utf8_lossy(&total[..headers_end]);
                if let Some(cl) = headers_text
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length: "))
                    .or_else(|| {
                        headers_text
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                    })
                    && let Ok(len) = cl.trim().parse::<usize>()
                {
                    let body_start = headers_end + 4;
                    if total.len() >= body_start + len {
                        break;
                    }
                }
            }
        }
        let text = String::from_utf8_lossy(&total).to_string();
        *captured.lock().await = text;

        // Stream two text chunks then a done marker. The provider
        // joins the content fragments into a single assistant turn.
        let body = b"{\"message\":{\"content\":\"hel\"},\"done\":false}\n\
                     {\"message\":{\"content\":\"lo\"},\"done\":false}\n\
                     {\"done\":true}\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        sock.write_all(response.as_bytes()).await.unwrap();
        sock.write_all(body).await.unwrap();
        let _ = sock.shutdown().await;
    });
    (format!("http://{}", addr), captured_ret)
}

fn find_subseq(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[tokio::test]
async fn ollama_provider_streams_text_and_completes() {
    let (base_url, captured) = spawn_stub_ollama().await;

    let db = Db::in_memory().unwrap();
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
        name: "S".into(),
        folder_id: "f1".into(),
        model: Some("ollama:llama3.1".into()),
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

    // Seed settings: point the provider at the stub server and attach a
    // custom Authorization header so we can verify it lands in the
    // outbound request.
    let schema = test_schema();
    db.set_plugin_setting("ollama", "base_url", &serde_json::Value::String(base_url))
        .await
        .unwrap();
    db.set_plugin_setting(
        "ollama",
        "additional_headers",
        &serde_json::json!([
            {"key": "Authorization", "value": "Bearer test_token"}
        ]),
    )
    .await
    .unwrap();

    let store = PluginSettingsStore::new("ollama", schema, db.clone());
    let provider = OllamaProvider::new(store);

    let broadcaster = Broadcaster::new();
    let (completion_tx, mut completion_rx) = mpsc::channel(8);
    let plugins = Arc::new(peckboard::plugin::manager::PluginManager::empty());

    let ctx = SendMessageContext {
        session_id: "s1".into(),
        message: "hello there".into(),
        db: db.clone(),
        broadcaster: broadcaster.clone(),
        config: SpawnConfig {
            model: "ollama:llama3.1".into(),
            ..Default::default()
        },
        conversation_id: None,
        completion_tx,
        plugins,
    };

    provider.send_message(ctx).await.unwrap();

    let completion = tokio::time::timeout(Duration::from_secs(5), completion_rx.recv())
        .await
        .expect("completion delivered within timeout")
        .expect("channel still open");
    assert!(completion.completed, "stub should drive a clean completion");
    assert_eq!(completion.session_id, "s1");

    // Inspect what the provider actually sent to the stub.
    let req = captured.lock().await.clone();
    let lower = req.to_lowercase();
    assert!(
        lower.contains("/api/chat"),
        "request should target /api/chat (got: {req})"
    );
    assert!(
        lower.contains("authorization: bearer test_token"),
        "additional Authorization header from settings must be sent (got: {req})"
    );
    // The user message should be in the body verbatim.
    assert!(
        req.contains("hello there"),
        "user message must appear in body"
    );

    // Events written to the session: Started, Text("hel"), Text("lo"), Completed.
    let events = db.events_tail("s1", 16).await.unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert!(kinds.contains(&"agent-start"));
    assert!(kinds.contains(&"agent-text"));
    assert!(kinds.contains(&"agent-end"));
    let text_events: Vec<String> = events
        .iter()
        .filter(|e| e.kind == "agent-text")
        .map(|e| {
            let v: serde_json::Value = serde_json::from_str(&e.data).unwrap();
            v["text"].as_str().unwrap_or_default().to_string()
        })
        .collect();
    // Order is preserved by the seq column, so chunk fragments arrive in
    // the order the stub emitted them.
    assert_eq!(text_events, vec!["hel", "lo"]);
}

/// Schema with the `additional_models` field so the store can round-trip
/// the setting the dynamic-models path reads.
fn models_schema() -> SettingsSchema {
    SettingsSchema::new(vec![
        SettingField {
            key: "base_url".into(),
            title: "Base URL".into(),
            description: None,
            required: true,
            kind: FieldKind::Url {
                default: Some("http://localhost:11434".into()),
                placeholder: None,
            },
        },
        SettingField {
            key: "additional_models".into(),
            title: "Additional Models".into(),
            description: None,
            required: false,
            kind: FieldKind::StringList {
                item_placeholder: None,
            },
        },
    ])
}

/// The user-registered `additional_models` setting surfaces through the
/// provider registry's catalog path as `ollama:<name>` models, live —
/// exactly what `/api/models` and the model picker consume. Tag variants
/// (`llama3.1:8b`) must round-trip even though they carry a colon.
#[tokio::test]
async fn additional_models_setting_registers_models_by_name() {
    use peckboard::provider::registry::{ProviderInfo, ProviderRegistry};

    let db = Db::in_memory().unwrap();
    db.set_plugin_setting(
        "ollama",
        "additional_models",
        &serde_json::json!(["llama3.1:8b", "mistral-small", "llama3.1"]),
    )
    .await
    .unwrap();

    let store = PluginSettingsStore::new("ollama", models_schema(), db.clone());
    let provider = Arc::new(OllamaProvider::new(store));

    let registry = ProviderRegistry::new();
    registry
        .register(
            provider,
            ProviderInfo {
                id: "ollama".into(),
                display_name: "Ollama".into(),
                // Static seed registered at init — the dynamic override
                // must replace this, not append to a stale copy.
                models: peckboard::provider::ollama::default_models(),
            },
        )
        .await;

    let all: Vec<String> = registry
        .list_all_models()
        .await
        .into_iter()
        .map(|(id, _)| id)
        .collect();

    // Built-in seeds still present.
    assert!(all.contains(&"ollama:llama3.1".to_string()));
    assert!(all.contains(&"ollama:qwen2.5-coder".to_string()));
    // User extras registered by name, tag colon preserved.
    assert!(all.contains(&"ollama:llama3.1:8b".to_string()));
    assert!(all.contains(&"ollama:mistral-small".to_string()));
    // "llama3.1" was both a seed and an extra → not duplicated.
    assert_eq!(
        all.iter().filter(|id| *id == "ollama:llama3.1").count(),
        1,
        "an extra duplicating a built-in id must not appear twice"
    );

    // The per-provider catalog view reflects the same effective list.
    let providers = registry.list_providers_with_models().await;
    let ollama = providers.iter().find(|p| p.id == "ollama").unwrap();
    assert!(ollama.models.iter().any(|m| m.id == "llama3.1:8b"));
    let extra = ollama
        .models
        .iter()
        .find(|m| m.id == "mistral-small")
        .unwrap();
    assert_eq!(extra.display_name, "mistral-small (Ollama)");
}

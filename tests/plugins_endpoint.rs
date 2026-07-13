//! HTTP-level test for `GET /api/plugins`.
//!
//! Locks in the wire shape the Settings page consumes — id, display name,
//! permissions array, status enum — and asserts that the two built-in
//! plugins registered by `plugin::builtins::register_all` show up with
//! the permissions they declared.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewUser};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::builtins::register_all as register_builtin_plugins;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::routes::plugins::router;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;
use serde_json::Value;
use tower::ServiceExt;

async fn build_state() -> (Arc<AppState>, String) {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        port: 0,
        https_port: 0,
        host: "127.0.0.1".into(),
        data_dir: tmp.path().to_path_buf(),
        mdns: false,
        keep_alive_hours: 0,
        provider_send_timeout_secs: 300,
    };

    let db = Db::in_memory().unwrap();
    let plugins = Arc::new(PluginManager::new(&config.data_dir, db.clone()));
    let jwt_secret = generate_jwt_secret();
    let provider_registry = Arc::new(ProviderRegistry::new());
    let builtin_plugins = Arc::new(BuiltinPluginRegistry::new());
    register_builtin_plugins(&builtin_plugins, provider_registry.clone(), db.clone()).await;
    let session_manager = SessionManager::new(provider_registry.clone());
    let push_service = PushService::new(&config.data_dir);

    db.create_user(NewUser {
        id: "u1".into(),
        username: "admin".into(),
        email: None,
        password_hash: "h".into(),
        role: "admin".into(),
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
    })
    .await
    .unwrap();
    let (token, _exp) = create_token(&jwt_secret, "u1", "admin", "as1").unwrap();
    db.create_auth_session(NewAuthSession {
        id: "as1".into(),
        user_id: "u1".into(),
        token_hash: hash_token(&token),
        created_at: 1_000_000,
        expires_at: 1_000_000 + 7 * 24 * 60 * 60,
        user_agent: None,
        ip_address: None,
    })
    .await
    .unwrap();

    let state = Arc::new(AppState {
        config,
        db,
        plugins,
        builtin_plugins,
        jwt_secret,
        login_limiter: RateLimiter::new(60),
        password_change_limiter: RateLimiter::<String>::new(5),
        broadcaster: Broadcaster::new(),
        provider_registry,
        session_manager,
        repeating_task_manager: peckboard::repeating::RepeatingTaskManager::new(),
        run_auditor: peckboard::repeating::RunAuditor::new(),
        mcp_tokens: McpTokenRegistry::new(),
        push_service,
    });
    std::mem::forget(tmp);
    (state, token)
}

#[tokio::test]
async fn list_plugins_returns_builtin_catalog() {
    let (state, token) = build_state().await;
    let app = router(state.clone()).with_state(state.clone());

    let req = Request::builder()
        .uri("/api/plugins")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    let plugins = json["plugins"].as_array().expect("plugins array");
    assert_eq!(
        plugins.len(),
        5,
        "expected built-in claude-code + mock + ollama + cursor + grok; got {plugins:?}",
    );

    // The catalog also carries the plugin-contributed UI panels (from
    // loaded WASM plugins). None are loaded here, so the field is present
    // and empty — the UI relies on it always being an array.
    let panels = json["ui_panels"]
        .as_array()
        .expect("ui_panels array present in catalog");
    assert!(panels.is_empty(), "no WASM plugins loaded; got {panels:?}");

    let claude = plugins
        .iter()
        .find(|p| p["id"] == "claude-code")
        .expect("claude-code plugin present");
    assert_eq!(claude["display_name"], "Claude Code");
    assert_eq!(claude["built_in"], true);
    assert_eq!(claude["enabled"], true);
    assert_eq!(claude["status"]["kind"], "active");

    let claude_perms: Vec<&str> = claude["permissions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    // Claude needs subprocess + filesystem + network; the test pins the
    // requested set so an accidental permission drop is caught.
    for required in [
        "register_provider",
        "spawn_process",
        "filesystem_read",
        "filesystem_write",
        "network_access",
    ] {
        assert!(
            claude_perms.contains(&required),
            "claude-code missing requested permission {required}: {claude_perms:?}",
        );
    }

    let mock = plugins
        .iter()
        .find(|p| p["id"] == "mock")
        .expect("mock plugin present");
    assert_eq!(mock["display_name"], "Mock Provider");
    let mock_perms: Vec<&str> = mock["permissions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    // Mock never touches process / network / fs, so its catalog entry
    // should only carry the one permission it actually needs.
    assert_eq!(mock_perms, vec!["register_provider"]);

    let cursor = plugins
        .iter()
        .find(|p| p["id"] == "cursor")
        .expect("cursor plugin present");
    assert_eq!(cursor["display_name"], "Cursor");
    assert_eq!(cursor["built_in"], true);
    let cursor_perms: Vec<&str> = cursor["permissions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    // The cursor-agent CLI spawns a subprocess, touches the working dir,
    // and talks to Cursor's backend — pin the requested set.
    for required in [
        "register_provider",
        "spawn_process",
        "filesystem_read",
        "filesystem_write",
        "network_access",
    ] {
        assert!(
            cursor_perms.contains(&required),
            "cursor missing requested permission {required}: {cursor_perms:?}",
        );
    }

    // Each permission entry must carry a human label + description for
    // the UI; an empty string would mean the UI renders a blank row.
    for p in claude["permissions"].as_array().unwrap() {
        assert!(!p["label"].as_str().unwrap().is_empty());
        assert!(!p["description"].as_str().unwrap().is_empty());
    }
}

#[tokio::test]
async fn list_plugins_requires_auth() {
    let (state, _token) = build_state().await;
    let app = router(state.clone()).with_state(state.clone());

    let req = Request::builder()
        .uri("/api/plugins")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

/// The Ollama plugin's settings schema is the first non-trivial example
/// of the typed schema flowing through `/api/plugins`. Pin the shape so
/// the UI's renderer doesn't drift away from the backend.
#[tokio::test]
async fn list_plugins_includes_ollama_settings_schema() {
    let (state, token) = build_state().await;
    let app = router(state.clone()).with_state(state.clone());

    let req = Request::builder()
        .uri("/api/plugins")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    let plugins = json["plugins"].as_array().unwrap();
    let ollama = plugins
        .iter()
        .find(|p| p["id"] == "ollama")
        .expect("ollama plugin registered");
    assert_eq!(ollama["display_name"], "Ollama");

    let fields = ollama["settings_schema"]["fields"]
        .as_array()
        .expect("settings schema fields");
    let keys: Vec<&str> = fields.iter().map(|f| f["key"].as_str().unwrap()).collect();
    assert!(keys.contains(&"base_url"));
    assert!(keys.contains(&"default_model"));
    assert!(keys.contains(&"additional_headers"));

    // The headers field must declare secret_values so the UI password-
    // masks it and the API never echoes the value back.
    let headers = fields
        .iter()
        .find(|f| f["key"] == "additional_headers")
        .unwrap();
    assert_eq!(headers["type"], "key_value_list");
    assert_eq!(headers["secret_values"], true);
}

#[tokio::test]
async fn put_settings_round_trips_and_masks_secret_values() {
    let (state, token) = build_state().await;
    let app = router(state.clone()).with_state(state.clone());

    // Save a base URL change AND an additional Authorization header.
    let put_req = Request::builder()
        .uri("/api/plugins/ollama/settings")
        .method("PUT")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "updates": {
                    "base_url": "https://ollama.example.com",
                    "additional_headers": [
                        {"key": "Authorization", "value": "Bearer SECRET_TOKEN"},
                        {"key": "X-Tenant", "value": "team-a"}
                    ]
                }
            })
            .to_string(),
        ))
        .unwrap();
    let put_res = app.clone().oneshot(put_req).await.unwrap();
    assert_eq!(put_res.status(), StatusCode::OK);
    let put_body = axum::body::to_bytes(put_res.into_body(), usize::MAX)
        .await
        .unwrap();
    let put_json: Value = serde_json::from_slice(&put_body).unwrap();
    let put_settings = put_json["settings"].as_array().unwrap();
    let base_url = put_settings
        .iter()
        .find(|s| s["key"] == "base_url")
        .unwrap();
    assert_eq!(base_url["value"], "https://ollama.example.com");
    let headers = put_settings
        .iter()
        .find(|s| s["key"] == "additional_headers")
        .unwrap();
    // The wire payload must mask the secret values — keys are visible
    // but values are gone. This is the only thing standing between a
    // browser session and a leaked Authorization header.
    assert_eq!(headers["masked"], true);
    let entries = headers["value"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["key"], "Authorization");
    assert!(entries[0]["value"].is_null());

    // GET returns the same masked shape on a fresh request.
    let get_req = Request::builder()
        .uri("/api/plugins/ollama/settings")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let get_res = app.oneshot(get_req).await.unwrap();
    assert_eq!(get_res.status(), StatusCode::OK);
    let get_body = axum::body::to_bytes(get_res.into_body(), usize::MAX)
        .await
        .unwrap();
    let get_json: Value = serde_json::from_slice(&get_body).unwrap();
    let get_headers = get_json["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["key"] == "additional_headers")
        .unwrap();
    let entries = get_headers["value"].as_array().unwrap();
    assert!(
        entries[0]["value"].is_null(),
        "secret value must stay masked on GET"
    );
    assert_eq!(entries[0]["key"], "Authorization");

    // Bonus: the raw DB row holds the real value — proves the masking
    // is purely a wire-format concern and the provider can still read
    // the secret at request time.
    let raw = state.db.list_plugin_settings("ollama").await.unwrap();
    let raw_headers = raw.get("additional_headers").unwrap();
    let raw_arr = raw_headers.as_array().unwrap();
    assert_eq!(raw_arr[0]["value"], "Bearer SECRET_TOKEN");
}

/// A WASM plugin's manifest-declared settings ride the exact same routes as
/// a built-in plugin's: schema from the manifest, values validated + stored
/// in `plugin_settings`, secrets masked on the wire. Skips when the
/// nginx-manager wasm isn't built (same policy as the e2e bridge test).
#[tokio::test]
async fn wasm_plugin_settings_round_trip_via_routes() {
    let wasm = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "../peck-plugins/nginx-manager/target/wasm32-unknown-unknown/release/\
         peckboard_nginx_manager_plugin.wasm",
    );
    if !wasm.exists() {
        eprintln!("SKIP wasm_plugin_settings_round_trip_via_routes: plugin wasm not built");
        return;
    }

    let (state, token) = build_state().await;
    let plugins_dir = state.config.data_dir.join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::copy(&wasm, plugins_dir.join("nginx-manager.wasm")).unwrap();
    state.plugins.load_all().await.unwrap();

    let app = router(state.clone()).with_state(state.clone());

    // GET: the manifest-declared schema (url + secret string), no values yet.
    let req = Request::builder()
        .uri("/api/plugins/nginx-manager/settings")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    let fields = json["schema"]["fields"].as_array().unwrap();
    assert_eq!(fields.len(), 2, "schema: {json}");
    assert_eq!(fields[0]["key"], "base_url");
    assert_eq!(fields[0]["type"], "url");
    assert_eq!(fields[1]["key"], "api_key");
    assert_eq!(fields[1]["secret"], true);

    // PUT url + token; the secret comes back masked but is stored raw.
    let req = Request::builder()
        .uri("/api/plugins/nginx-manager/settings")
        .method("PUT")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "updates": {
                    "base_url": "http://192.168.1.10:81",
                    "api_key": "npm_super_secret_key"
                }
            })
            .to_string(),
        ))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    let settings = json["settings"].as_array().unwrap();
    let base_url = settings.iter().find(|s| s["key"] == "base_url").unwrap();
    assert_eq!(base_url["value"], "http://192.168.1.10:81");
    let api_key = settings.iter().find(|s| s["key"] == "api_key").unwrap();
    assert_eq!(api_key["masked"], true);
    assert!(
        api_key["value"].is_null(),
        "secret must not echo: {api_key}"
    );
    assert_eq!(api_key["has_value"], true);

    // A bad URL is rejected by the shared validator.
    let req = Request::builder()
        .uri("/api/plugins/nginx-manager/settings")
        .method("PUT")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({ "updates": { "base_url": "file:///etc/passwd" } }).to_string(),
        ))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // The stored row holds the raw key — exactly what the plugin's
    // `peckboard_get_plugin_setting` host read returns at tool-call time.
    let raw = state
        .db
        .list_plugin_settings("nginx-manager")
        .await
        .unwrap();
    assert_eq!(raw.get("api_key").unwrap(), "npm_super_secret_key");
}
#[tokio::test]
async fn put_settings_rejects_invalid_url() {
    let (state, token) = build_state().await;
    let app = router(state.clone()).with_state(state.clone());

    let req = Request::builder()
        .uri("/api/plugins/ollama/settings")
        .method("PUT")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "updates": { "base_url": "file:///etc/passwd" }
            })
            .to_string(),
        ))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["field"], "base_url");
}

#[tokio::test]
async fn put_settings_rejects_header_with_crlf() {
    let (state, token) = build_state().await;
    let app = router(state.clone()).with_state(state.clone());

    // CRLF in a header value is the classic response-splitting vector;
    // the validator must drop it before the value ever hits reqwest.
    let req = Request::builder()
        .uri("/api/plugins/ollama/settings")
        .method("PUT")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "updates": {
                    "additional_headers": [
                        {"key": "X-Smuggle", "value": "ok\r\nInjected: yes"}
                    ]
                }
            })
            .to_string(),
        ))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn put_settings_rejects_unknown_field() {
    let (state, token) = build_state().await;
    let app = router(state.clone()).with_state(state.clone());

    let req = Request::builder()
        .uri("/api/plugins/ollama/settings")
        .method("PUT")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({ "updates": { "not_a_field": "x" } }).to_string(),
        ))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn put_settings_requires_auth() {
    let (state, _token) = build_state().await;
    let app = router(state.clone()).with_state(state.clone());

    let req = Request::builder()
        .uri("/api/plugins/ollama/settings")
        .method("PUT")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"updates":{}}"#))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

// ── Plugin upgrade / compatibility ────────────────────────────────────

/// A stub registry server: replies to any request with `body` and loops so a
/// fresh connection per fetch is fine. Returns its base `registry.json` URL.
async fn spawn_stub_registry(body: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let mut buf = [0u8; 2048];
            // Drain the request line/headers (a GET has no body).
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    format!("http://{addr}/registry.json")
}

/// Registry index with one entry that needs a far-future Peckboard
/// (`min_peckboard: 99.0.0` → incompatible) and one with no floor (compatible).
const STUB_INDEX: &str = r#"{
  "schema_version": 1,
  "plugins": [
    {"id":"futuristic","name":"Futuristic","description":"needs a newer host",
     "author":"a","version":"9.9.9","hooks":["mcp.tool.invoke"],
     "url":"https://example.invalid/futuristic.wasm","sha256":"00","min_peckboard":"99.0.0"},
    {"id":"compatible","name":"Compatible","description":"works anywhere",
     "author":"a","version":"1.0.0","hooks":["mcp.tool.invoke"],
     "url":"https://example.invalid/compatible.wasm","sha256":"00"}
  ]
}"#;

#[tokio::test]
async fn registry_endpoint_reports_compatibility_and_version() {
    let (state, token) = build_state().await;
    let url = spawn_stub_registry(STUB_INDEX).await;
    state.db.add_plugin_repository(&url, "stub").await.unwrap();
    let app = router(state.clone()).with_state(state.clone());

    let req = Request::builder()
        .uri("/api/plugins/registry")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();

    // The running Peckboard version is surfaced so the UI can show "needs …".
    assert!(
        json["peckboard_version"]
            .as_str()
            .is_some_and(|v| !v.is_empty()),
        "peckboard_version present: {json}"
    );

    let plugins = json["plugins"].as_array().expect("plugins array");
    let by_id = |id: &str| plugins.iter().find(|p| p["id"] == id).cloned().unwrap();

    let fut = by_id("futuristic");
    assert_eq!(fut["compatible"], false, "99.0.0 floor → incompatible");
    assert_eq!(fut["installed"], false);
    assert_eq!(
        fut["upgrade_available"], false,
        "not installed → no upgrade"
    );
    assert_eq!(fut["min_peckboard"], "99.0.0");

    let compat = by_id("compatible");
    assert_eq!(compat["compatible"], true, "no floor → compatible");
    assert!(compat["min_peckboard"].is_null());
}

#[tokio::test]
async fn install_refuses_incompatible_but_passes_compatible_gate() {
    let (state, token) = build_state().await;
    let url = spawn_stub_registry(STUB_INDEX).await;
    state.db.add_plugin_repository(&url, "stub").await.unwrap();

    // Incompatible (min_peckboard 99.0.0) → refused at the compatibility gate,
    // before any download, with 409 Conflict.
    let app = router(state.clone()).with_state(state.clone());
    let req = Request::builder()
        .uri("/api/plugins/registry/install")
        .method("POST")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"id":"futuristic"}"#))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap()
            .contains("requires Peckboard"),
        "clear incompatibility error: {json}"
    );

    // Compatible (no floor) → passes the gate and proceeds to download, which
    // fails against the bogus example.invalid URL (502). Proves the gate did
    // NOT block a compatible plugin.
    let app = router(state.clone()).with_state(state.clone());
    let req = Request::builder()
        .uri("/api/plugins/registry/install")
        .method("POST")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"id":"compatible"}"#))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(
        res.status(),
        StatusCode::BAD_GATEWAY,
        "compatible plugin passes the gate and fails only at download"
    );
}

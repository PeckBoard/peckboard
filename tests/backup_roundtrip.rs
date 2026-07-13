//! Integration tests for backup/restore: round-trip, retention pruning,
//! and admin-gate on the HTTP endpoint.

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
use peckboard::routes::backup::router as backup_router;
use peckboard::service::backup::{create_snapshot, prune_old_backups, restore_from};
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;
use tower::ServiceExt;

// ── helpers ──────────────────────────────────────────────────────────────────

async fn build_state_with_dir(dir: &std::path::Path, role: &str) -> (Arc<AppState>, String) {
    let config = Config {
        port: 0,
        https_port: 0,
        host: "127.0.0.1".into(),
        data_dir: dir.to_path_buf(),
        mdns: false,
        keep_alive_hours: 0,
        provider_send_timeout_secs: 300,
    };

    let db = Db::open(dir).unwrap();
    let plugins = Arc::new(PluginManager::new(&config.data_dir, db.clone()));
    let jwt_secret = generate_jwt_secret();
    let provider_registry = Arc::new(ProviderRegistry::new());
    let builtin_plugins = Arc::new(BuiltinPluginRegistry::new());
    register_builtin_plugins(&builtin_plugins, provider_registry.clone(), db.clone()).await;
    let session_manager = SessionManager::new(provider_registry.clone());
    let push_service = PushService::new(&config.data_dir);

    db.create_user(NewUser {
        id: "u1".into(),
        username: "testuser".into(),
        email: None,
        password_hash: "h".into(),
        role: role.into(),
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
    })
    .await
    .unwrap();

    let (token, _) = create_token(&jwt_secret, "u1", role, "s1").unwrap();
    db.create_auth_session(NewAuthSession {
        id: "s1".into(),
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
    (state, token)
}

// ── round-trip ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn backup_and_restore_roundtrip() {
    let src = tempfile::tempdir().unwrap();
    let src_dir = src.path();

    // Open a real DB and seed a user so there is something to back up.
    let db = Db::open(src_dir).unwrap();
    db.create_user(NewUser {
        id: "u1".into(),
        username: "alice".into(),
        email: None,
        password_hash: "h".into(),
        role: "admin".into(),
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
    })
    .await
    .unwrap();

    // Write some sidecar files
    std::fs::write(src_dir.join("config.json"), r#"{"foo":"bar"}"#).unwrap();
    std::fs::write(src_dir.join("vapid_keys.json"), r#"{"key":"val"}"#).unwrap();
    std::fs::create_dir_all(src_dir.join("reports")).unwrap();
    std::fs::write(src_dir.join("reports").join("note.md"), "hello").unwrap();

    // Create snapshot
    let bytes = create_snapshot(&db, src_dir).await.unwrap();
    assert!(!bytes.is_empty(), "snapshot must not be empty");
    // gzip magic
    assert_eq!(&bytes[..2], &[0x1f, 0x8b], "must be a gzip file");

    // Write archive to a temp file
    let archive = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(archive.path(), &bytes).unwrap();

    // Restore into a fresh directory
    let dest = tempfile::tempdir().unwrap();
    restore_from(archive.path(), dest.path(), false).unwrap();

    // DB was restored
    assert!(dest.path().join("peckboard.db").exists());
    // Sidecar files were restored
    assert_eq!(
        std::fs::read_to_string(dest.path().join("config.json")).unwrap(),
        r#"{"foo":"bar"}"#
    );
    assert_eq!(
        std::fs::read_to_string(dest.path().join("vapid_keys.json")).unwrap(),
        r#"{"key":"val"}"#
    );
    assert_eq!(
        std::fs::read_to_string(dest.path().join("reports").join("note.md")).unwrap(),
        "hello"
    );

    // Row counts match
    let restored_db = Db::open(dest.path()).unwrap();
    let count = restored_db.count_users().await.unwrap();
    assert_eq!(count, 1);
}

// ── force flag ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn restore_refuses_without_force_when_db_exists() {
    let src = tempfile::tempdir().unwrap();
    let db = Db::open(src.path()).unwrap();
    let bytes = create_snapshot(&db, src.path()).await.unwrap();

    let archive = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(archive.path(), &bytes).unwrap();

    let dest = tempfile::tempdir().unwrap();
    // Simulate existing DB
    std::fs::write(dest.path().join("peckboard.db"), b"").unwrap();

    let err = restore_from(archive.path(), dest.path(), false);
    assert!(err.is_err());
    assert!(
        err.unwrap_err().to_string().contains("--force"),
        "error should mention --force"
    );

    // With force it succeeds
    restore_from(archive.path(), dest.path(), true).unwrap();
    assert!(dest.path().join("peckboard.db").exists());
}

// ── retention pruning ─────────────────────────────────────────────────────────
#[test]
fn retention_pruning_keeps_newest_n() {
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path();
    let keep = 3usize;

    // Create keep+2 fake backup files
    for i in 0..(keep + 2) {
        let path = dir_path.join(format!("peckboard-backup-{i}.tar.gz"));
        std::fs::write(&path, b"fake").unwrap();
    }

    prune_old_backups(dir_path, keep).unwrap();

    let remaining: Vec<_> = std::fs::read_dir(dir_path)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("peckboard-backup-")
        })
        .collect();

    assert_eq!(
        remaining.len(),
        keep,
        "expected {keep} files to remain after pruning"
    );
}

// ── 403 for non-admin ─────────────────────────────────────────────────────────

#[tokio::test]
async fn backup_endpoint_rejects_non_admin() {
    let tmp = tempfile::tempdir().unwrap();
    let (state, user_token) = build_state_with_dir(tmp.path(), "user").await;

    let app = backup_router(state.clone()).with_state(state);

    // Non-admin user gets 403
    let req = Request::builder()
        .method("GET")
        .uri("/api/admin/backup")
        .header(header::AUTHORIZATION, format!("Bearer {user_token}"))
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn backup_endpoint_allows_admin() {
    let tmp = tempfile::tempdir().unwrap();
    let (state, admin_token) = build_state_with_dir(tmp.path(), "admin").await;

    let app = backup_router(state.clone()).with_state(state);

    let req = Request::builder()
        .method("GET")
        .uri("/api/admin/backup")
        .header(header::AUTHORIZATION, format!("Bearer {admin_token}"))
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/gzip")
    );
}

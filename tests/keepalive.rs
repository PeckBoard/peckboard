//! Integration test for the provider login keep-alive.
//!
//! `keepalive::run_once` should, for each registered auth provider, spin up a
//! throwaway session, drive one turn through the provider, then delete the
//! session (and its events) — leaving no residue behind. The mock provider is
//! registered under the `cursor` id so the run is deterministic and completes
//! on its own.

use std::sync::Arc;

use peckboard::db::Db;
use peckboard::db::models::NewKimiAccount;
use peckboard::keepalive;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::mock::MockProvider;
use peckboard::provider::registry::{ProviderInfo, ProviderRegistry};
use peckboard::provider::stream::ModelInfo;
use peckboard::ws::broadcaster::Broadcaster;

#[tokio::test]
async fn keep_alive_pings_then_cleans_up_after_itself() {
    let registry = Arc::new(ProviderRegistry::new());
    // Register the mock provider under the `cursor` id so the keep-alive's
    // `cursor` target dispatches to it. Any model id resolves to the mock's
    // "unknown scenario" path, which still emits Started → Text → Completed.
    registry
        .register(
            Arc::new(MockProvider::new()),
            ProviderInfo {
                id: "cursor".into(),
                display_name: "Cursor (mock)".into(),
                models: vec![ModelInfo {
                    id: "keepalive-probe".into(),
                    display_name: "probe".into(),
                    capabilities: vec![],
                    tier: 0,
                }],
                effort_levels: vec![],
                capabilities: Default::default(),
            },
        )
        .await;

    let manager = SessionManager::new(registry.clone());
    let db = Db::in_memory().unwrap();
    let broadcaster = Broadcaster::new();
    let data_dir = std::env::temp_dir().join(format!("pb-keepalive-{}", uuid::Uuid::new_v4()));

    // No sessions and no folders to start.
    assert!(db.list_sessions().await.unwrap().is_empty());
    assert!(db.list_folders().await.unwrap().is_empty());

    keepalive::run_once(&db, &registry, &manager, &broadcaster, &data_dir).await;

    // Each login records its own last-run, surfaced per account/provider in
    // Settings via /api/config. Here the mock is registered under `cursor`,
    // so its default login is stamped.
    let runs = keepalive::last_runs();
    assert!(
        runs.iter()
            .any(|r| r.provider == "cursor" && r.account_id.is_none()),
        "run_once should record a per-login last-run for the cursor default"
    );

    // The throwaway session was created AND torn down: nothing left behind.
    let sessions = db.list_sessions().await.unwrap();
    assert!(
        sessions.is_empty(),
        "keep-alive should delete its session; found {}",
        sessions.len()
    );

    // The dedicated (hidden) keep-alive folder was created and persists so the
    // next cycle reuses it.
    let folders = db.list_folders().await.unwrap();
    assert_eq!(folders.len(), 1, "expected the keep-alive folder to exist");
    assert_eq!(folders[0].id, keepalive::KEEPALIVE_FOLDER_ID);

    // A second cycle reuses the folder rather than erroring on a duplicate id.
    keepalive::run_once(&db, &registry, &manager, &broadcaster, &data_dir).await;
    assert!(db.list_sessions().await.unwrap().is_empty());
    assert_eq!(db.list_folders().await.unwrap().len(), 1);
}

#[tokio::test]
async fn keep_alive_is_noop_when_no_auth_providers_registered() {
    let registry = Arc::new(ProviderRegistry::new());
    // Only the mock provider under its own id — none of claude/grok/cursor.
    registry
        .register(
            Arc::new(MockProvider::new()),
            ProviderInfo {
                id: "mock".into(),
                display_name: "Mock".into(),
                models: vec![ModelInfo {
                    id: "happy-path".into(),
                    display_name: "happy".into(),
                    capabilities: vec![],
                    tier: 0,
                }],
                effort_levels: vec![],
                capabilities: Default::default(),
            },
        )
        .await;

    let manager = SessionManager::new(registry.clone());
    let db = Db::in_memory().unwrap();
    let broadcaster = Broadcaster::new();
    let data_dir = std::env::temp_dir().join(format!("pb-keepalive-noop-{}", uuid::Uuid::new_v4()));

    keepalive::run_once(&db, &registry, &manager, &broadcaster, &data_dir).await;

    // No auth login to refresh ⇒ no sessions created.
    assert!(db.list_sessions().await.unwrap().is_empty());
}

#[tokio::test]
async fn keep_alive_pings_kimi_default_and_each_stored_account() {
    let registry = Arc::new(ProviderRegistry::new());
    // Mock registered under the `kimi` id so the kimi targets dispatch to it.
    registry
        .register(
            Arc::new(MockProvider::new()),
            ProviderInfo {
                id: "kimi".into(),
                display_name: "Kimi (mock)".into(),
                models: vec![ModelInfo {
                    id: "keepalive-probe".into(),
                    display_name: "probe".into(),
                    capabilities: vec![],
                    tier: 0,
                }],
                effort_levels: vec![],
                capabilities: Default::default(),
            },
        )
        .await;

    let manager = SessionManager::new(registry.clone());
    let db = Db::in_memory().unwrap();
    let broadcaster = Broadcaster::new();
    let data_dir = std::env::temp_dir().join(format!("pb-keepalive-kimi-{}", uuid::Uuid::new_v4()));

    let now = 1_700_000_000_i64;
    db.create_kimi_account(NewKimiAccount {
        id: "kimi-acct-1".into(),
        name: "Work".into(),
        kind: "device".into(),
        credential: "device".into(),
        config_dir: None,
        budget_window_hours: None,
        budget_limit_usd: None,
        budget_limit_tokens: None,
        warn_threshold: 0.8,
        critical_threshold: 0.95,
        created_at: now,
        updated_at: now,
    })
    .await
    .unwrap();

    keepalive::run_once(&db, &registry, &manager, &broadcaster, &data_dir).await;

    // Kimi is multi-account: the host default login AND the stored account
    // each get their own ping/last-run.
    let runs = keepalive::last_runs();
    assert!(
        runs.iter()
            .any(|r| r.provider == "kimi" && r.account_id.is_none()),
        "expected a last-run for the kimi host default login"
    );
    assert!(
        runs.iter()
            .any(|r| r.provider == "kimi" && r.account_id.as_deref() == Some("kimi-acct-1")),
        "expected a last-run for the stored kimi account"
    );

    // Throwaway sessions torn down as for any other provider.
    assert!(db.list_sessions().await.unwrap().is_empty());
}

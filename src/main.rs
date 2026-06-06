use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::generate_jwt_secret;
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::claude::register_claude_provider;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::routes::api_router;
use peckboard::security::{origin_check, repair_dangling_sessions, security_headers};
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;

use axum::middleware;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::signal;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = Config::load();
    let addr = format!("{}:{}", config.host, config.port);

    let db = Db::open(&config.data_dir)?;
    tracing::info!("Database opened at {}", config.data_dir.display());

    // Startup state repair: fix dangling agent-starts from previous crash
    repair_dangling_sessions(&db).await?;

    // Purge expired auth sessions
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let purged = db.delete_expired_auth_sessions(now).await?;
    if purged > 0 {
        tracing::info!("Purged {purged} expired auth session(s)");
    }

    let plugins = PluginManager::new(&config.data_dir);
    plugins.load_all().await?;

    let jwt_secret = generate_jwt_secret();
    let login_limiter = RateLimiter::new(5);

    let broadcaster = Broadcaster::new();
    let provider_registry = ProviderRegistry::new();
    register_claude_provider(&provider_registry).await;

    let state = Arc::new(AppState {
        config,
        db,
        plugins,
        jwt_secret,
        login_limiter,
        broadcaster,
        provider_registry,
    });

    let app = api_router(state.clone())
        .layer(middleware::from_fn(security_headers))
        .layer(middleware::from_fn(origin_check))
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    tracing::info!("Peckboard listening on http://{addr}");
    let listener = TcpListener::bind(&addr).await?;

    // Graceful shutdown on SIGINT/SIGTERM
    let shutdown_state = state.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("Shutdown signal received, shutting down gracefully...");
            shutdown_state.plugins.shutdown().await;
            tracing::info!("Shutdown complete");
        })
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

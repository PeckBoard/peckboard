use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::routes::api_router;
use peckboard::state::AppState;

use std::sync::Arc;
use tokio::net::TcpListener;
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

    let state = Arc::new(AppState { config, db });

    let app = api_router()
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    tracing::info!("Peckboard listening on http://{addr}");
    let listener = TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

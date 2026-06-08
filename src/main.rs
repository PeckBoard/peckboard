use clap::Parser;
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::reset::reset_user_password;
use peckboard::auth::token::load_or_create_jwt_secret;
use peckboard::config::{CliArgs, Config};
use peckboard::db::Db;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::claude::register_claude_provider;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::mock::register_mock_provider;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::routes::api_router;
use peckboard::security::{origin_check, repair_dangling_sessions, security_headers};
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::mdns;
use peckboard::service::push::PushService;
use peckboard::service::tls;
use peckboard::service::wake::WakeDetector;
use peckboard::state::AppState;
use peckboard::worker::watchdog;
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

    let args = CliArgs::parse();

    // Short-circuit CLI maintenance flows before any server startup.
    if args.reset_password {
        let username = args.user.clone();
        let config = Config::from_args(args);
        let db = Db::open(&config.data_dir)?;
        let outcome = reset_user_password(&db, username.as_deref()).await?;
        // stderr for the human note, stdout for just the credentials so
        // it's easy to pipe `peckboard --reset-password | tail -1`.
        eprintln!(
            "Reset password for '{}' and revoked {} auth session(s).",
            outcome.username, outcome.sessions_revoked,
        );
        println!("{}:{}", outcome.username, outcome.new_password);
        return Ok(());
    }

    let config = Config::from_args(args);
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

    let plugins = Arc::new(PluginManager::new(&config.data_dir));
    plugins.load_all().await?;

    let jwt_secret = load_or_create_jwt_secret(&config.data_dir)?;
    // 60/min is plenty for a single-tenant LAN server; the previous 5
    // was so aggressive that even a normal user with a few tabs open
    // (each authenticating its own WS) could trip it. Rate-limiting
    // still uses the (currently hardcoded) IP, so this is a per-host
    // ceiling, not a per-account one.
    let login_limiter = RateLimiter::new(60);

    let broadcaster = Broadcaster::new();
    let provider_registry = Arc::new(ProviderRegistry::new());
    register_claude_provider(&provider_registry).await;
    register_mock_provider(&provider_registry).await;
    let session_manager =
        SessionManager::new(provider_registry.clone()).with_plugins(plugins.clone());

    let mcp_tokens = McpTokenRegistry::new();
    let push_service = PushService::new(&config.data_dir);

    let state = Arc::new(AppState {
        config,
        db,
        plugins,
        jwt_secret,
        login_limiter,
        broadcaster,
        provider_registry,
        session_manager,
        mcp_tokens,
        push_service,
    });

    // Resume any in-flight worker sessions after startup repair
    peckboard::worker::orchestrator::check_and_spawn_workers(&state).await;
    tracing::info!("Worker orchestrator startup check complete");

    // Run orchestrator on a 5-second interval to pick up new cards quickly
    {
        let orch_state = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            interval.tick().await; // skip immediate first tick (already ran above)
            loop {
                interval.tick().await;
                peckboard::worker::orchestrator::check_and_spawn_workers(&orch_state).await;
            }
        });
        tracing::info!("Worker orchestrator loop started (5s interval)");
    }

    let app = api_router(state.clone())
        .layer(axum::extract::DefaultBodyLimit::max(20 * 1024 * 1024))
        .layer(middleware::from_fn(security_headers))
        .layer(middleware::from_fn(origin_check))
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    tracing::info!("Peckboard listening on http://{addr}");
    let listener = TcpListener::bind(&addr).await?;

    // Start mDNS advertisement
    let mdns_name = mdns::generate_mdns_name();
    let mdns_handle = match mdns::start_mdns(&mdns_name, state.config.port) {
        Ok(handle) => {
            tracing::info!("mDNS name: {mdns_name}");
            Some(handle)
        }
        Err(e) => {
            tracing::warn!("Failed to start mDNS: {e}");
            None
        }
    };

    // Start wake-from-sleep detector
    let _wake_detector = WakeDetector::start();
    tracing::info!("Wake-from-sleep detector started");

    // Start worker watchdog (orphan cleanup every 60s)
    {
        let watchdog_db = state.db.clone();
        let watchdog_sm = SessionManager::new(state.provider_registry.clone());
        let watchdog_bc = state.broadcaster.clone();
        tokio::spawn(watchdog::start_watchdog(
            watchdog_db,
            watchdog_sm,
            watchdog_bc,
        ));
        tracing::info!("Worker watchdog started");
    }

    // Start worker completion listener -- receives notifications when a
    // streaming process finishes and runs the worker-done handler +
    // orchestration outside the tokio::spawn boundary (avoiding Send issues
    // with AppState's PluginManager).
    //
    // Every completion (success or crash) also drains any persistent
    // queued message for the session so a "send while busy" reliably
    // delivers, even if the in-flight run was interrupted or crashed.
    {
        if let Some(mut rx) = state.session_manager.take_completion_rx().await {
            let orchestrator_state = state.clone();
            tokio::spawn(async move {
                while let Some(completion) = rx.recv().await {
                    let sid = completion.session_id.clone();

                    // 1. Worker-specific bookkeeping. Hold the per-session
                    //    lock for the entire handler so the watchdog's
                    //    try_lock_session check skips this session while
                    //    plugins, token revoke, and DB updates run — even
                    //    if the handler takes longer than the watchdog's
                    //    grace window.
                    {
                        let _guard = orchestrator_state.session_manager.lock_session(&sid).await;
                        match orchestrator_state.db.get_session(&sid).await {
                            Ok(Some(session)) if session.is_worker => {
                                if completion.completed {
                                    tracing::info!(session_id = %sid, "Worker completed, running handle_worker_done");
                                    peckboard::worker::orchestrator::handle_worker_done(
                                        &orchestrator_state,
                                        &sid,
                                    )
                                    .await;
                                } else {
                                    // Worker crashed/interrupted — clear
                                    // worker_session_id so the orchestrator can
                                    // re-spawn or the watchdog can detect the
                                    // dead worker.
                                    tracing::warn!(session_id = %sid, "Worker crashed or interrupted");
                                    if let Some(card_id) = &session.card_id {
                                        let _ = orchestrator_state
                                            .db
                                            .update_card(
                                                card_id,
                                                peckboard::db::models::UpdateCard {
                                                    worker_session_id: Some(None),
                                                    last_worker_session_id: Some(Some(sid.clone())),
                                                    ..Default::default()
                                                },
                                            )
                                            .await;
                                    }
                                }
                            }
                            _ => {}
                        }
                    } // release lock before drain_queue_for_session, which
                    // re-acquires it inside drain_queued (tokio Mutex is
                    // not reentrant).

                    // 2. Drain any queued message — runs for every session
                    // (worker or interactive) and every completion outcome.
                    // drain_queue_for_session takes the per-session lock
                    // itself; we don't need to hold it here.
                    if let Err(e) = peckboard::worker::orchestrator::drain_queue_for_session(
                        &orchestrator_state,
                        &sid,
                    )
                    .await
                    {
                        tracing::warn!(
                            session_id = %sid,
                            "Queue drain failed: {e}"
                        );
                    }

                    // 3. Fill any freed worker slots.
                    peckboard::worker::orchestrator::check_and_spawn_workers(&orchestrator_state)
                        .await;
                }
            });
            tracing::info!("Worker completion listener started");
        }
    }

    // Start HTTPS listener if TLS certs can be loaded
    let https_addr = format!("{}:{}", state.config.host, state.config.https_port);
    let tls_handle = match tls::ensure_certs(&state.config.data_dir) {
        Ok(tls_config) => {
            // Install the default crypto provider for rustls (idempotent)
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            match tls::load_tls_config(&tls_config) {
                Ok(tls_acceptor) => {
                    let https_app = app.clone();
                    let https_listener = TcpListener::bind(&https_addr).await?;
                    tracing::info!("Peckboard listening on https://{https_addr}");
                    Some(tokio::spawn(serve_https(
                        https_listener,
                        tls_acceptor,
                        https_app,
                    )))
                }
                Err(e) => {
                    tracing::warn!("Failed to load TLS config, HTTPS disabled: {e}");
                    None
                }
            }
        }
        Err(e) => {
            tracing::warn!("Failed to ensure TLS certs, HTTPS disabled: {e}");
            None
        }
    };

    // Graceful shutdown on SIGINT/SIGTERM
    let shutdown_state = state.clone();
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        tracing::info!("Shutdown signal received, shutting down gracefully...");
        shutdown_state.session_manager.shutdown().await;
        shutdown_state.plugins.shutdown().await;
        tracing::info!("Shutdown complete");
    })
    .await?;

    // Stop mDNS advertisement
    if let Some(mdns) = mdns_handle {
        if let Err(e) = mdns.stop() {
            tracing::warn!("Failed to stop mDNS: {e}");
        } else {
            tracing::info!("mDNS advertisement stopped");
        }
    }

    // Abort the HTTPS task when HTTP server shuts down
    if let Some(handle) = tls_handle {
        handle.abort();
    }

    Ok(())
}

/// Serve the axum app over HTTPS by accepting TLS connections and feeding them
/// into `axum::serve`.  Each accepted TCP stream is upgraded to TLS via the
/// `TlsAcceptor` and then handled by the router.
async fn serve_https(
    listener: TcpListener,
    tls_acceptor: tokio_rustls::TlsAcceptor,
    app: axum::Router,
) {
    use tower::Service;

    let mut make_service = app.into_make_service_with_connect_info::<std::net::SocketAddr>();

    loop {
        let (tcp_stream, remote_addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::debug!("HTTPS accept error: {e}");
                continue;
            }
        };

        let acceptor = tls_acceptor.clone();
        let service = match make_service.call(remote_addr).await {
            Ok(svc) => svc,
            Err(e) => {
                // Infallible in practice, but handle gracefully
                tracing::debug!("HTTPS make_service error: {e}");
                continue;
            }
        };

        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(tcp_stream).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!("TLS handshake failed from {remote_addr}: {e}");
                    return;
                }
            };

            let io = hyper_util::rt::TokioIo::new(tls_stream);
            let hyper_service = hyper_util::service::TowerToHyperService::new(service);

            if let Err(e) =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection(io, hyper_service)
                    .await
            {
                tracing::debug!("HTTPS connection error from {remote_addr}: {e}");
            }
        });
    }
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

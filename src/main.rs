use clap::Parser;
use peckboard::auth::bootstrap::{BootstrapOutcome, ensure_admin_user};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::reset::reset_user_password;
use peckboard::auth::token::load_or_create_jwt_secret;
use peckboard::config::{CliArgs, Config};
use peckboard::db::Db;
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::builtins::register_all as register_builtin_plugins;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::repeating::{RepeatingTaskManager, RunAuditor};
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

    // First-run bootstrap: create the sole admin user if the DB is empty.
    // Peckboard does not expose self-service registration — operators
    // get one auto-generated admin and can mint additional users from
    // there. The outcome is held until the very end of startup so the
    // credentials land below the noisy tracing logs and are the last
    // thing the operator sees.
    let bootstrap_outcome = ensure_admin_user(&db).await?;

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

    let plugins = Arc::new(PluginManager::new(&config.data_dir, db.clone()));
    plugins.load_all().await?;

    let jwt_secret = load_or_create_jwt_secret(&config.data_dir)?;
    // 60/min is plenty for a single-tenant LAN server; the previous 5
    // was so aggressive that even a normal user with a few tabs open
    // (each authenticating its own WS) could trip it. Rate-limiting
    // still uses the (currently hardcoded) IP, so this is a per-host
    // ceiling, not a per-account one.
    let login_limiter = RateLimiter::new(60);
    // 5 password-change attempts per minute per user. Stops a stolen
    // token from ratcheting the password to lock out the legitimate
    // user; 5 is plenty of headroom for honest mis-clicks.
    let password_change_limiter = RateLimiter::<String>::new(5);

    let broadcaster = Broadcaster::new();

    let provider_registry = Arc::new(ProviderRegistry::new());
    let builtin_plugins = Arc::new(BuiltinPluginRegistry::new());
    // Each built-in plugin registers its capabilities (today: an
    // AgentProvider) through the catalog, replacing the old direct
    // `register_*_provider` calls. The catalog records the granted
    // permissions for `/api/plugins` and the Settings UI.
    register_builtin_plugins(&builtin_plugins, provider_registry.clone(), db.clone()).await;
    let session_manager =
        SessionManager::new(provider_registry.clone()).with_plugins(plugins.clone());
    let repeating_task_manager = RepeatingTaskManager::new();
    let run_auditor = RunAuditor::new();

    let mcp_tokens = McpTokenRegistry::new();
    let push_service = PushService::new(&config.data_dir);

    let state = Arc::new(AppState {
        config,
        db,
        plugins,
        builtin_plugins,
        jwt_secret,
        login_limiter,
        password_change_limiter,
        broadcaster,
        provider_registry,
        session_manager,
        repeating_task_manager,
        run_auditor,
        mcp_tokens,
        push_service,
    });

    // Now that `AppState` exists, bind the plugin agent-dispatch bridge. A
    // `Weak<AppState>` inside `AppLiveHost` breaks the otherwise-cyclic
    // `state → plugins → live → state` ownership; plugins loaded earlier share
    // the same (until now empty) slot, so they pick this up immediately.
    state
        .plugins
        .set_live_host(Arc::new(peckboard::service::mcp_server::AppLiveHost::new(
            &state,
            tokio::runtime::Handle::current(),
        )));

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

    // Repeating tasks scheduler: tick every 30 seconds. Cheap because
    // the due-task query is indexed on next_run_at. We don't tick more
    // often than the smallest practical schedule interval (1 minute),
    // so 30s gives us at most ~30s of slack between "due" and "fired".
    {
        let sched_state = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            // If a tick takes longer than the interval (e.g. a slow
            // dispatch with many due tasks), skip the missed ticks
            // rather than bursting catch-up runs — the next tick
            // re-queries due tasks anyway, so a burst would just
            // re-process the same set on top of each other.
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // Skip the immediate first tick — every tick after start sleeps
            // first so we don't dispatch the moment the server boots
            // (which would race the rest of startup).
            interval.tick().await;
            loop {
                interval.tick().await;
                let ctx = peckboard::repeating::RunContext {
                    db: &sched_state.db,
                    broadcaster: &sched_state.broadcaster,
                    session_manager: &sched_state.session_manager,
                    mcp_tokens: &sched_state.mcp_tokens,
                    data_dir: &sched_state.config.data_dir,
                    http_port: sched_state.config.port,
                    auditor: &sched_state.run_auditor,
                };
                sched_state.repeating_task_manager.run_due_tasks(ctx).await;
            }
        });
        tracing::info!("Repeating-task scheduler started (30s interval)");
    }

    // Repeating-task watchdog: independent observer that audits both the
    // auditor's own dispatch log and the persisted session rows every
    // 60s. Catches any scheduler-initiated runs that fired closer
    // together than the schedule allows; on a hit, it disables the
    // task (kill switch) and broadcasts a `repeating-task-watchdog`
    // event so the UI can surface the failure. See [`RunAuditor`].
    {
        let auditor = state.run_auditor.clone();
        let db_clone = state.db.clone();
        let bc_clone = state.broadcaster.clone();
        auditor.spawn_audit_loop(db_clone, bc_clone, std::time::Duration::from_secs(60));
        tracing::info!("Repeating-task watchdog started (60s interval)");
    }

    // Idle lock-map sweepers for SessionManager + RepeatingTaskManager.
    // Both managers insert a per-session/per-task `Arc<Mutex<()>>` on
    // first access and previously never removed it; for a user who
    // never closes tabs that map grows monotonically over months. The
    // sweep is `O(N)` over the map at a 5-minute cadence — invisible
    // until the count climbs into the thousands, and even then it
    // serialises against hot paths only briefly because the outer
    // mutex hold is bounded by the retain pass.
    state.session_manager.spawn_lock_sweeper();
    state.repeating_task_manager.spawn_lock_sweeper();
    tracing::info!("Lock-map sweepers started (5min interval)");

    // Provider login keep-alive: periodically ping each auth login with a
    // throwaway "hi" so tokens don't go stale. No-op when the interval is 0.
    peckboard::keepalive::spawn(state.clone(), state.config.keep_alive_hours);

    let app = api_router(state.clone())
        .layer(axum::extract::DefaultBodyLimit::max(20 * 1024 * 1024))
        .layer(middleware::from_fn(security_headers))
        .layer(middleware::from_fn(origin_check))
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    tracing::info!("Peckboard listening on http://{addr}");
    let listener = TcpListener::bind(&addr).await?;

    // mDNS advertisement is opt-in. Default-off avoids broadcasting
    // service presence on the LAN, which is an unnecessary discovery
    // / fingerprinting surface for a single-tenant LAN server. Enable
    // explicitly with `--mdns` (or `PECKBOARD_MDNS=1`) when discovery
    // is actually desired.
    let mdns_handle = if state.config.mdns {
        let mdns_name = mdns::generate_mdns_name();
        match mdns::start_mdns(&mdns_name, state.config.port) {
            Ok(handle) => {
                tracing::info!("mDNS name: {mdns_name}");
                Some(handle)
            }
            Err(e) => {
                tracing::warn!("Failed to start mDNS: {e}");
                None
            }
        }
    } else {
        tracing::info!("mDNS disabled (enable with --mdns)");
        None
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

                    // 0. Handover finalize. If this completion is the
                    //    outgoing model's doc-generation turn (session has a
                    //    parked `handover_to_model`), capture the doc, flip
                    //    the model, and stash the doc for the incoming model.
                    //    Runs before worker bookkeeping and short-circuits the
                    //    rest: a doc-gen turn isn't a normal worker/queue
                    //    completion and must not respawn anything.
                    {
                        let is_handover = matches!(
                            orchestrator_state.db.get_session(&sid).await,
                            Ok(Some(s)) if s.handover_to_model.is_some()
                        );
                        if is_handover {
                            {
                                let _guard =
                                    orchestrator_state.session_manager.lock_session(&sid).await;
                                if let Err(e) = peckboard::handover::finalize_handover(
                                    &orchestrator_state,
                                    &sid,
                                )
                                .await
                                {
                                    tracing::error!(
                                        session_id = %sid,
                                        "Handover finalize failed: {e}"
                                    );
                                }
                            } // drop the lock — the drain re-acquires it
                            // A message may have queued behind the doc turn;
                            // deliver it now. Its dispatch injects the freshly
                            // stashed doc via `take_pending_injection`.
                            if let Err(e) =
                                peckboard::worker::orchestrator::drain_queue_for_session(
                                    &orchestrator_state,
                                    &sid,
                                )
                                .await
                            {
                                tracing::warn!(
                                    session_id = %sid,
                                    "Post-handover queue drain failed: {e}"
                                );
                            }
                            continue;
                        }
                    }

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
                                    // Worker crashed/interrupted — clear the
                                    // card's `worker_session_id` ONLY if it
                                    // still points at THIS session. The
                                    // orchestrator can have spawned a
                                    // replacement between an outgoing cancel
                                    // and this listener firing (5s tick); an
                                    // unconditional clear would free the
                                    // replacement's slot and produce two
                                    // concurrent workers for the same card.
                                    tracing::warn!(session_id = %sid, "Worker crashed or interrupted");
                                    if let Some(card_id) = &session.card_id {
                                        let _ = orchestrator_state
                                            .db
                                            .clear_card_worker_if_matches(card_id, &sid)
                                            .await;

                                        // Auto-pause defense: if this card has
                                        // been crashing in a tight loop (e.g.
                                        // rate-limit, bad credentials, broken
                                        // sandbox), stop the project so we don't
                                        // burn cycles. Stderr from this run goes
                                        // into the pause reason so the user has
                                        // a starting point.
                                        let last_stderr =
                                            last_crash_stderr(&orchestrator_state.db, &sid).await;
                                        peckboard::worker::orchestrator::maybe_auto_pause_after_crash(
                                            &orchestrator_state,
                                            card_id,
                                            last_stderr.as_deref(),
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

                    // 1.5 Auto-compaction: a session whose context occupancy
                    // crossed the threshold gets a same-model compaction turn
                    // dispatched right here — the model writes a continuation
                    // doc and the conversation restarts fresh with it
                    // injected (see crate::handover). Applies to chats and
                    // workers alike; the eligibility guards (idle, nothing
                    // queued, worker card still resuming this session) live
                    // in maybe_auto_compact. A dispatched compaction implies
                    // an empty queue (guard), so falling through to the
                    // drain below is harmless.
                    if completion.completed
                        && let Err(e) =
                            peckboard::handover::maybe_auto_compact(&orchestrator_state, &sid).await
                    {
                        tracing::warn!(
                            session_id = %sid,
                            "Auto-compaction check failed: {e}"
                        );
                    }

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

    // Print the first-run admin credentials *last* so they sit below
    // the startup tracing noise and are the operator's final view
    // before the server goes quiet waiting on connections.
    if let Some(outcome) = bootstrap_outcome.as_ref() {
        print_bootstrap_banner(outcome);
    }

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

/// Print the first-run admin credentials in a hard-to-miss banner.
/// Goes to stdout so it shows up in normal terminal capture and so the
/// final `username:password` line stays pipe-friendly
/// (`peckboard | tail -1`).
fn print_bootstrap_banner(outcome: &BootstrapOutcome) {
    const BAR: &str = "════════════════════════════════════════════════════════════════════";
    println!();
    println!("{BAR}");
    println!("  PECKBOARD FIRST-RUN ADMIN ACCOUNT");
    println!("{BAR}");
    println!();
    println!("    username:  {}", outcome.username);
    println!("    password:  {}", outcome.new_password);
    println!();
    println!("  Save this — it will not be shown again.");
    println!("  Use `peckboard --reset-password` to mint a new one if it's lost.");
    println!();
    println!("{BAR}");
    // Machine-readable form (same as --reset-password) for tooling that
    // parses `peckboard | tail -1`.
    println!("{}:{}", outcome.username, outcome.new_password);
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

/// Read the stderr text from the session's most recent crash `agent-end`
/// event. The completion listener uses this to enrich the auto-pause
/// reason — knowing the worker crashed isn't useful on its own; the user
/// needs the underlying CLI error to act on it.
async fn last_crash_stderr(db: &peckboard::db::Db, session_id: &str) -> Option<String> {
    let events = db.events_tail(session_id, 16).await.ok()?;
    for event in events.iter().rev() {
        if event.kind != "agent-end" {
            continue;
        }
        let data: serde_json::Value = serde_json::from_str(&event.data).ok()?;
        if data.get("status").and_then(|s| s.as_str()) != Some("crashed") {
            continue;
        }
        return data
            .get("stderr")
            .and_then(|s| s.as_str())
            .map(str::to_string);
    }
    None
}

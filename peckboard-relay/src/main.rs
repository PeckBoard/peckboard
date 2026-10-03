//! `peckboard-relay` binary. Administration is local only: CLI flags at
//! start, `SIGUSR1` toggles debug logging, `SIGTERM`/`SIGINT` shut down
//! gracefully. Nothing administrative is reachable over the network.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use peckboard_relay::server::{Relay, RelayConfig};
use peckboard_relay::tls;
use tokio::net::{TcpListener, UdpSocket};
use tokio_rustls::TlsAcceptor;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

#[derive(Parser, Debug)]
#[command(version, about = "Peckboard handshake-only rendezvous relay")]
struct Args {
    /// TCP listen address for TLS signaling (+ ACME TLS-ALPN-01).
    #[arg(long, default_value = "[::]:443")]
    listen: SocketAddr,
    /// UDP listen address for authenticated STUN.
    #[arg(long, default_value = "[::]:3478")]
    stun_listen: SocketAddr,
    /// UDP port advertised to peers (defaults to the --stun-listen port).
    #[arg(long)]
    stun_public_port: Option<u16>,
    /// Certificate name.
    #[arg(long, default_value = "relay.peckboard.com")]
    domain: String,
    /// ACME account contact e-mail (optional).
    #[arg(long, env = "PECKRELAY_ACME_CONTACT")]
    acme_contact: Option<String>,
    /// Use the Let's Encrypt staging directory.
    #[arg(long)]
    acme_staging: bool,
    /// Where the ACME account/cert cache lives (0700).
    #[arg(long, env = "STATE_DIRECTORY", default_value = "/var/lib/peckrelay")]
    state_dir: PathBuf,
    /// Throwaway self-signed cert instead of ACME (tests / local runs). The
    /// cert DER is written to <state-dir>/dev-cert.der for clients to pin.
    #[arg(long)]
    dev_self_signed: bool,
    #[arg(long, default_value_t = 4096)]
    max_connections: usize,
    #[arg(long, default_value_t = 16)]
    max_connections_per_ip: usize,
    #[arg(long, default_value_t = 100_000)]
    max_ids: usize,
    /// Log full client IPs (default: salted 4-byte hash).
    #[arg(long)]
    log_full_ips: bool,
    /// Initial log filter (also RUST_LOG).
    #[arg(long, env = "RUST_LOG", default_value = "info")]
    log: String,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let (filter, reload) = tracing_subscriber::reload::Layer::new(
        EnvFilter::try_new(&args.log).unwrap_or_else(|_| EnvFilter::new("info")),
    );
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout())),
        )
        .init();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(args, reload))
}

async fn run(
    args: Args,
    reload: tracing_subscriber::reload::Handle<EnvFilter, tracing_subscriber::Registry>,
) -> anyhow::Result<()> {
    let server_cfg = if args.dev_self_signed {
        let (cfg, der) = tls::self_signed(&[args.domain.clone(), "localhost".into()])?;
        std::fs::create_dir_all(&args.state_dir).ok();
        let path = args.state_dir.join("dev-cert.der");
        std::fs::write(&path, der.as_ref()).with_context(|| format!("write {}", path.display()))?;
        info!("dev self-signed cert written to {}", path.display());
        cfg
    } else {
        tls::acme(
            &args.domain,
            args.acme_contact.as_deref(),
            &args.state_dir,
            !args.acme_staging,
        )?
    };

    let tcp = TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("bind {}", args.listen))?;
    let udp = UdpSocket::bind(args.stun_listen)
        .await
        .with_context(|| format!("bind {}", args.stun_listen))?;
    let stun_port = args.stun_public_port.unwrap_or(args.stun_listen.port());

    let cfg = RelayConfig {
        max_connections: args.max_connections,
        max_connections_per_ip: args.max_connections_per_ip,
        max_ids: args.max_ids,
        log_full_ips: args.log_full_ips,
        ..RelayConfig::default()
    };
    let relay = Relay::new(cfg, stun_port);
    info!(tcp = %args.listen, udp = %args.stun_listen, "relay listening");

    let r1 = relay.clone();
    let acceptor = TlsAcceptor::from(server_cfg);
    let tls_task = tokio::spawn(async move { r1.serve_tls(tcp, acceptor).await });
    let r2 = relay.clone();
    let stun_task = tokio::spawn(async move { r2.serve_stun(udp).await });

    wait_for_shutdown(reload, args.log).await;
    info!("shutting down");
    relay.shutdown(Duration::from_secs(5)).await;
    let _ = tls_task.await;
    let _ = stun_task.await;
    Ok(())
}

#[cfg(unix)]
async fn wait_for_shutdown(
    reload: tracing_subscriber::reload::Handle<EnvFilter, tracing_subscriber::Registry>,
    base: String,
) {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    let mut usr1 = signal(SignalKind::user_defined1()).expect("SIGUSR1 handler");
    let mut verbose = false;
    loop {
        tokio::select! {
            _ = term.recv() => return,
            _ = int.recv() => return,
            _ = usr1.recv() => {
                verbose = !verbose;
                let f = if verbose { "debug".to_string() } else { base.clone() };
                let _ = reload.modify(|cur| *cur = EnvFilter::try_new(&f).unwrap_or_else(|_| EnvFilter::new("info")));
                info!(verbose, "log level toggled");
            }
        }
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown(
    _reload: tracing_subscriber::reload::Handle<EnvFilter, tracing_subscriber::Registry>,
    _base: String,
) {
    let _ = tokio::signal::ctrl_c().await;
}

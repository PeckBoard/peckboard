//! `peckboard-connect <pairing-link>` — reach your Peckboard box directly.
//!
//! Rendezvous through the relay, hole-punch a direct UDP path, run QUIC over
//! it (mutually authenticated with keys from the pairing secret), and expose
//! the box on a local TCP port. Reconnects with backoff when the path drops.
//! Nothing is written to disk unless `--save` is given.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::parser::ValueSource;
use clap::{CommandFactory, FromArgMatches, Parser};
use peckboard_relay::client::ClientConfig;
use peckboard_relay::proto::Role;
use peckboard_relay::tunnel::{
    PairingLink, TunnelError, TunnelEvent, connect_device, establish, relay_config,
};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

/// Tried first so the local URL (and the browser state tied to its origin)
/// stays the same across runs; falls back to any free port.
const PREFERRED_PORT: u16 = 3399;
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A connection that lasted this long resets the backoff.
const STABLE_AFTER: Duration = Duration::from_secs(30);
const HARD_NAT: &str = "couldn't reach your Peckboard directly from this network — forward one UDP port on the box's router or try another network";
const ARGV_WARNING: &str = "warning: a pairing link given as an argument is visible in the process list; prefer `-` (stdin), $PECKBOARD_LINK or --save";

#[derive(Parser)]
#[command(
    name = "peckboard-connect",
    version,
    about = "Reach your Peckboard over a direct, end-to-end encrypted tunnel",
    after_help = "Pass the link without exposing it in the process list:\n  \
                  peckboard-connect - < link.txt          (stdin)\n  \
                  PECKBOARD_LINK='peckboard://pair/...' peckboard-connect\n  \
                  peckboard-connect --save -             (remember it; later runs need no link)"
)]
struct Args {
    /// `-` to read the pairing link from stdin (preferred), or the link
    /// itself (`peckboard://pair/...` — visible in the process list).
    /// Omit to use $PECKBOARD_LINK or the link stored by `--save`.
    #[arg(env = "PECKBOARD_LINK", hide_env_values = true)]
    link: Option<String>,
    /// Local address to expose Peckboard on (default: 127.0.0.1:3399, or a
    /// free port if that's taken).
    #[arg(long)]
    listen: Option<SocketAddr>,
    /// Rendezvous relay `host[:port]` (overrides the link's).
    #[arg(long)]
    relay: Option<String>,
    /// Store the link (file mode 0600) so later runs need no argument.
    #[arg(long)]
    save: bool,
    /// Pin this DER certificate for the relay instead of public CAs (dev).
    #[arg(long, hide = true)]
    relay_cert: Option<PathBuf>,
}

fn saved_link_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.map(|b| b.join("peckboard").join("connect-link"))
}

fn save_link(link: &str) -> anyhow::Result<PathBuf> {
    let path = saved_link_path().context("no config directory to save the link in")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&path)?;
    #[cfg(unix)]
    {
        // `mode` only applies on creation; tighten a pre-existing file too.
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    std::io::Write::write_all(&mut f, format!("{link}\n").as_bytes())?;
    Ok(path)
}

async fn read_link(arg: Option<String>) -> anyhow::Result<String> {
    match arg.as_deref() {
        Some("-") => {
            let mut s = String::new();
            tokio::io::stdin().read_to_string(&mut s).await?;
            Ok(s.trim().to_string())
        }
        Some(l) => Ok(l.to_string()),
        None => {
            let path = saved_link_path().context("no pairing link given")?;
            let s = std::fs::read_to_string(&path).with_context(|| {
                format!(
                    "no pairing link given and none saved at {} (pass one, with --save to remember it)",
                    path.display()
                )
            })?;
            Ok(s.trim().to_string())
        }
    }
}

/// True when the secret itself arrived as a command-line argument (not `-`,
/// not `$PECKBOARD_LINK`, not the saved file) and so sits in the process list.
fn link_on_argv(source: Option<ValueSource>, link: Option<&str>) -> bool {
    source == Some(ValueSource::CommandLine) && link != Some("-")
}
async fn bind(listen: Option<SocketAddr>) -> anyhow::Result<TcpListener> {
    if let Some(a) = listen {
        return TcpListener::bind(a)
            .await
            .with_context(|| format!("listen on {a}"));
    }
    let preferred = SocketAddr::from((Ipv4Addr::LOCALHOST, PREFERRED_PORT));
    match TcpListener::bind(preferred).await {
        Ok(l) => Ok(l),
        Err(_) => Ok(TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?),
    }
}

async fn client_config(relay: &str, cert: Option<&PathBuf>) -> anyhow::Result<ClientConfig> {
    let cfg = relay_config(relay).await?;
    match cert {
        None => Ok(cfg),
        Some(p) => {
            let der = std::fs::read(p).with_context(|| format!("read {}", p.display()))?;
            ClientConfig::pinned(
                cfg.relay,
                &cfg.server_name,
                rustls_pki_types::CertificateDer::from(der),
            )
        }
    }
}

fn url(local: SocketAddr) -> String {
    let host = if local.ip().is_unspecified() {
        "127.0.0.1".to_string()
    } else if local.is_ipv6() {
        format!("[{}]", local.ip())
    } else {
        local.ip().to_string()
    };
    format!("http://{host}:{}", local.port())
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let matches = Args::command().get_matches();
    let args = Args::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    if link_on_argv(matches.value_source("link"), args.link.as_deref()) {
        eprintln!("{ARGV_WARNING}");
    }
    let raw = read_link(args.link).await?;
    let mut link = PairingLink::parse(&raw)?;
    if args.save {
        let p = save_link(&link.to_uri())?;
        eprintln!("Saved pairing link to {}", p.display());
    }
    if let Some(r) = args.relay {
        link.relay = r;
    }
    let listener = bind(args.listen).await?;
    let local = url(listener.local_addr()?);

    tokio::select! {
        r = connect_loop(&link, args.relay_cert.as_ref(), &listener, &local) => r,
        _ = tokio::signal::ctrl_c() => {
            eprintln!("Bye.");
            Ok(())
        }
    }
}

async fn connect_loop(
    link: &PairingLink,
    relay_cert: Option<&PathBuf>,
    listener: &TcpListener,
    local: &str,
) -> anyhow::Result<()> {
    let connected_at: Arc<Mutex<Option<Instant>>> = Arc::default();
    let mut ever_connected = false;
    let mut backoff = Duration::from_secs(1);
    eprintln!("Connecting to your Peckboard via {}…", link.relay);
    loop {
        *connected_at.lock().unwrap() = None;
        let (at, url, first) = (connected_at.clone(), local.to_string(), !ever_connected);
        let on_event = move |ev: TunnelEvent| match ev {
            TunnelEvent::Connected { peer, rtt_ms } => {
                *at.lock().unwrap() = Some(Instant::now());
                if first {
                    println!("Peckboard available at {url}");
                } else {
                    eprintln!("Reconnected — Peckboard available at {url}");
                }
                eprintln!("  direct path to {peer}, rtt {rtt_ms} ms");
            }
            TunnelEvent::Disconnected { reason } => eprintln!("Connection lost: {reason}"),
            TunnelEvent::Error(e) => eprintln!("Tunnel error: {e}"),
        };
        let attempt = async {
            let cfg = client_config(&link.relay, relay_cert).await?;
            let path = establish(&cfg, &link.secret, Role::Device).await?;
            connect_device(path, &link.secret, listener, on_event).await
        };
        let result = attempt.await;
        let lasted = connected_at.lock().unwrap().map(|t| t.elapsed());
        ever_connected |= lasted.is_some();
        if let Err(e) = result {
            match e.downcast_ref::<TunnelError>() {
                Some(TunnelError::PunchFailed { .. }) => {
                    eprintln!("{HARD_NAT}");
                    if !ever_connected {
                        bail!("no direct path to your Peckboard");
                    }
                }
                Some(TunnelError::PeerOffline) => eprintln!(
                    "Your Peckboard isn't reachable through the relay right now (offline, or this link was revoked)."
                ),
                None => eprintln!("Connection failed: {e:#}"),
            }
        }
        if lasted.is_some_and(|d| d >= STABLE_AFTER) {
            backoff = Duration::from_secs(1);
        }
        eprintln!("Retrying in {}s…", backoff.as_secs());
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_formats() {
        assert_eq!(
            url("127.0.0.1:3399".parse().unwrap()),
            "http://127.0.0.1:3399"
        );
        assert_eq!(url("0.0.0.0:80".parse().unwrap()), "http://127.0.0.1:80");
        assert_eq!(url("[::1]:8080".parse().unwrap()), "http://[::1]:8080");
    }

    #[test]
    fn warns_only_for_a_link_on_argv() {
        let on_argv = |argv: &[&str]| {
            let m = Args::command().try_get_matches_from(argv).unwrap();
            let link = m.get_one::<String>("link").map(String::as_str);
            link_on_argv(m.value_source("link"), link)
        };
        assert!(on_argv(&["pc", "peckboard://pair/AAAA"]));
        assert!(!on_argv(&["pc", "-"]));
        assert!(!on_argv(&["pc", "--save"]));
        assert!(!link_on_argv(
            Some(ValueSource::EnvVariable),
            Some("peckboard://pair/AAAA")
        ));
    }
}

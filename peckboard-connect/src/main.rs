//! `peckboard-connect <pairing-link>` — reach your Peckboard box directly.
//!
//! Rendezvous through the relay, hole-punch a direct UDP path, run QUIC over
//! it (mutually authenticated with keys from the pairing secret), and expose
//! the box on a local TCP port. Reconnects with backoff when the path drops.
//! Nothing is written to disk unless `--save` is given.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, anyhow};
use clap::parser::ValueSource;
use clap::{CommandFactory, FromArgMatches, Parser};
use peckboard_relay::client::ClientConfig;
use peckboard_relay::tunnel::{
    CancellationToken, CookieGate, DeviceEvent, DeviceOptions, ListenAddr, PairingLink,
    TunnelError, bind_listener, relay_config, run_device,
};
use tokio::io::AsyncReadExt;

/// Tried first so the local URL (and the browser state tied to its origin)
/// stays the same across runs; falls back to any free port.
const PREFERRED_PORT: u16 = 3399;
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
    /// Only let the browser that opens the printed link use the local port
    /// (cookie gate): other local programs are refused.
    #[arg(long)]
    gate: bool,
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
    let listener = bind_listener(match args.listen {
        Some(a) => ListenAddr::Exact(a),
        None => ListenAddr::Prefer(SocketAddr::from((Ipv4Addr::LOCALHOST, PREFERRED_PORT))),
    })
    .await
    .with_context(|| {
        format!(
            "listen on {}",
            args.listen.map_or("localhost".into(), |a| a.to_string())
        )
    })?;
    let mut local = url(listener.local_addr()?);

    let mut opts = DeviceOptions::new(link);
    opts.give_up_on_punch_failure = true;
    if let Some(p) = &args.relay_cert {
        opts.relay = Some(client_config(&opts.link.relay, Some(p)).await?);
    }
    if args.gate {
        let gate = CookieGate::new();
        local.push_str(&gate.boot_path());
        opts = opts.with_gate(&gate);
    }

    eprintln!("Connecting to your Peckboard via {}…", opts.link.relay);
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            stop.cancel();
        }
    });
    run_device(opts, listener, cancel, report(local))
        .await
        .map_err(|e| match e.downcast_ref::<TunnelError>() {
            Some(TunnelError::PunchFailed { .. }) => anyhow!("no direct path to your Peckboard"),
            _ => e,
        })?;
    eprintln!("Bye.");
    Ok(())
}

/// Print [`run_device`] progress the way this CLI always has.
fn report(url: String) -> impl Fn(DeviceEvent) + Send + Sync + 'static {
    let ever_connected = AtomicBool::new(false);
    move |ev| match ev {
        DeviceEvent::Connecting => {}
        DeviceEvent::Connected { peer, rtt_ms } => {
            if ever_connected.swap(true, Ordering::Relaxed) {
                eprintln!("Reconnected — Peckboard available at {url}");
            } else {
                println!("Peckboard available at {url}");
            }
            eprintln!("  direct path to {peer}, rtt {rtt_ms} ms");
        }
        DeviceEvent::Disconnected { reason } => eprintln!("Connection lost: {reason}"),
        DeviceEvent::PunchFailed { .. } => eprintln!("{HARD_NAT}"),
        DeviceEvent::PeerOffline => eprintln!(
            "Your Peckboard isn't reachable through the relay right now (offline, or this link was revoked)."
        ),
        DeviceEvent::Failed(e) => eprintln!("Connection failed: {e}"),
        DeviceEvent::Retrying { after } => eprintln!("Retrying in {}s…", after.as_secs()),
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

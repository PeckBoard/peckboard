//! `peckboard-connect <pairing-link>` — reach your Peckboard box directly.
//!
//! Rendezvous through the relay, hole-punch a direct UDP path, run QUIC over
//! it (mutually authenticated, no CA), and expose the box on a local TCP
//! port. Reconnects with backoff when the path drops.
//!
//! A v2 pairing link works once: the first run enrolls this device's own
//! key with the box and always saves the resulting credential (file mode
//! 0600, default `<config>/peckboard/connect-credential`, or
//! `--credential <path>`); later runs need no link. A legacy (v1) link
//! writes nothing unless `--save` or `--credential` is given; then it is
//! upgraded to a credential too.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, anyhow, bail};
use clap::parser::ValueSource;
use clap::{CommandFactory, FromArgMatches, Parser};
use ed25519_dalek::SigningKey;
use peckboard_relay::client::ClientConfig;
use peckboard_relay::identity::{decode_key, encode_key};
use peckboard_relay::tunnel::{
    CancellationToken, CookieGate, DeviceCredential, DeviceEvent, DeviceOptions,
    EnrolledCredential, ListenAddr, PairingLink, PathKind, TunnelError, bind_listener,
    relay_config, run_device,
};
use rand::RngCore;
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
                  peckboard-connect --save -             (remember it; later runs need no link)\n  \
                  peckboard-connect --credential work.cred -   (one credential file per box)"
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
    /// Make this box the default for argument-less runs (file mode 0600).
    #[arg(long)]
    save: bool,
    /// Credential file to use and update instead of the default one (one
    /// per box when you have several).
    #[arg(long, value_name = "PATH")]
    credential: Option<PathBuf>,
    /// Write nothing to disk (legacy links only: a v2 link must save this
    /// device's key, since the link works once).
    #[arg(long, conflicts_with_all = ["save", "credential"])]
    no_save: bool,
    /// Pin this DER certificate for the relay instead of public CAs (dev).
    #[arg(long, hide = true)]
    relay_cert: Option<PathBuf>,
}

fn config_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.map(|b| b.join("peckboard"))
}

/// Where older builds' `--save` put the link (read only now).
fn saved_link_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("connect-link"))
}

fn default_credential_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("connect-credential"))
}

/// Write `data` to `path` atomically (temp file + rename), mode 0600.
fn write_private(path: &Path, data: &str) -> anyhow::Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(d) = dir {
        std::fs::create_dir_all(d)?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.tmp{}", rand::random::<u32>()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let res = (|| {
        let mut f = opts.open(&tmp)?;
        std::io::Write::write_all(&mut f, data.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res.with_context(|| format!("write {}", path.display()))
}

/// What a credential file (or the input) holds.
enum Saved {
    /// `peckboard-cred:2:…`: enrolled.
    Enrolled(EnrolledCredential),
    /// A link, plus this device's key once one was made for it (written
    /// before enrolling, so a retry after a crash re-uses it).
    Link {
        link: PairingLink,
        key: Option<SigningKey>,
    },
}

const KEY_LINE: &str = "device-key=";

fn parse_saved(s: &str) -> anyhow::Result<Saved> {
    let s = s.trim();
    if s.starts_with(EnrolledCredential::PREFIX) {
        return Ok(Saved::Enrolled(EnrolledCredential::parse(s)?));
    }
    let mut lines = s.lines();
    let link = PairingLink::parse(lines.next().unwrap_or(""))?;
    let key = lines
        .filter_map(|l| l.trim().strip_prefix(KEY_LINE))
        .find_map(decode_key)
        .map(|seed| SigningKey::from_bytes(&seed));
    Ok(Saved::Link { link, key })
}

/// The file form of [`Saved::Link`].
fn render_link(link: &PairingLink, key: &SigningKey) -> String {
    format!(
        "{}\n{KEY_LINE}{}\n",
        link.to_uri(),
        encode_key(&key.to_bytes())
    )
}

fn new_device_key() -> SigningKey {
    let mut seed = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut seed);
    SigningKey::from_bytes(&seed)
}

/// The link / credential text: `-` (stdin), the argument, or the saved
/// credential file (falling back to an older build's saved link). Returns
/// it with whether it was read from disk.
async fn read_input(arg: Option<String>, file: Option<&Path>) -> anyhow::Result<(String, bool)> {
    match arg.as_deref() {
        Some("-") => {
            let mut s = String::new();
            tokio::io::stdin().read_to_string(&mut s).await?;
            Ok((s.trim().to_string(), false))
        }
        Some(l) => Ok((l.to_string(), false)),
        None => {
            if let Some(p) = file
                && let Ok(s) = std::fs::read_to_string(p)
            {
                return Ok((s, true));
            }
            let path = saved_link_path().context("no pairing link given")?;
            let s = std::fs::read_to_string(&path).with_context(|| {
                format!(
                    "no pairing link given and none saved at {} (pass one, with --save to remember it)",
                    file.unwrap_or(&path).display()
                )
            })?;
            // An older build's `--save`: treat like a saved credential
            // (persisted, upgraded).
            Ok((s.trim().to_string(), true))
        }
    }
}

/// How this run persists, from the flags.
struct Persist {
    /// Credential file to write, if any.
    path: Option<PathBuf>,
    /// The user asked for it (`--save` / `--credential`): replacing another
    /// box's saved credential is fine.
    explicit: bool,
}

/// Turn the input into the credential to run with, saving the device key
/// first where one is needed. `from_file`: the input was read from
/// `persist.path` (re-use its key, no need to rewrite it).
fn prepare(saved: Saved, persist: &Persist, from_file: bool) -> anyhow::Result<DeviceCredential> {
    let (link, key) = match saved {
        Saved::Enrolled(c) => {
            if !from_file
                && persist.explicit
                && let Some(p) = &persist.path
            {
                write_private(p, &format!("{}\n", c.encode()))?;
            }
            return Ok(DeviceCredential::Enrolled(c));
        }
        Saved::Link { link, key } => (link, key),
    };
    let Some(path) = &persist.path else {
        if link.is_v2() {
            bail!(
                "this pairing link works once, so peckboard-connect must save this device's key; drop --no-save"
            );
        }
        return Ok(DeviceCredential::Legacy {
            link,
            upgrade_key: None,
        });
    };
    if !link.is_v2() && !persist.explicit && !from_file {
        // A one-off legacy run writes nothing (as before).
        return Ok(DeviceCredential::Legacy {
            link,
            upgrade_key: None,
        });
    }
    // Re-use the key saved for this very link (crash mid-enrollment).
    let on_disk = std::fs::read_to_string(path).ok();
    let reused = match on_disk.as_deref().map(parse_saved) {
        Some(Ok(Saved::Link {
            link: l,
            key: Some(k),
        })) if l.secret.as_bytes() == link.secret.as_bytes() => Some(k),
        _ => None,
    };
    if reused.is_none() && on_disk.is_some() && !persist.explicit {
        bail!(
            "{} already holds another box's credential; pass --credential <path> for this one, or --save to replace it",
            path.display()
        );
    }
    let key = reused.or(key).unwrap_or_else(new_device_key);
    write_private(path, &render_link(&link, &key))?;
    Ok(DeviceCredential::from_link(link, key))
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
    let persist = Persist {
        path: if args.no_save {
            None
        } else {
            args.credential.clone().or_else(default_credential_path)
        },
        explicit: args.save || args.credential.is_some(),
    };
    let (raw, from_file) = read_input(args.link, persist.path.as_deref()).await?;
    let mut saved = parse_saved(&raw)?;
    if let (Some(r), Saved::Link { link, .. }) = (&args.relay, &mut saved) {
        link.relay = r.clone();
    }
    if let Saved::Link { link, .. } = &saved
        && let Some(fp) = link.box_fingerprint()
    {
        eprintln!("Box fingerprint: {fp} — check it matches the one shown next to the QR code.");
    }
    let cred = prepare(saved, &persist, from_file)?;
    if let (Some(p), DeviceCredential::Link { .. }) = (&persist.path, &cred) {
        eprintln!("Saved this device's key to {}", p.display());
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

    let relay_host = args
        .relay
        .clone()
        .unwrap_or_else(|| cred.relay_host().to_string());
    let mut opts = DeviceOptions::new(cred);
    opts.give_up_on_punch_failure = true;
    opts.device_name = "peckboard-connect".into();
    if args.relay_cert.is_some() || args.relay.is_some() {
        opts.relay = Some(client_config(&relay_host, args.relay_cert.as_ref()).await?);
    }
    if let Some(p) = persist.path.clone() {
        opts.on_enrolled = Some(Arc::new(move |c: &EnrolledCredential| {
            write_private(&p, &format!("{}\n", c.encode()))?;
            eprintln!(
                "Saved this device's credential to {} (the pairing link itself is now used up)",
                p.display()
            );
            Ok(())
        }));
    }
    if args.gate {
        let gate = CookieGate::new();
        local.push_str(&gate.boot_path());
        opts = opts.with_gate(&gate);
    }

    eprintln!("Connecting to your Peckboard via {relay_host}…");
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
            Some(TunnelError::EnrollRefused { reason }) => anyhow!("{}", reason.message()),
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
        DeviceEvent::Connected { peer, rtt_ms, path } => {
            if ever_connected.swap(true, Ordering::Relaxed) {
                eprintln!("Reconnected — Peckboard available at {url}");
            } else {
                println!("Peckboard available at {url}");
            }
            match path {
                PathKind::Direct => eprintln!("  direct path to {peer}, rtt {rtt_ms} ms"),
                PathKind::Relayed => eprintln!(
                    "  relayed through the rendezvous server (no direct path; still end-to-end encrypted), rtt {rtt_ms} ms"
                ),
            }
        }
        DeviceEvent::PathChanged { path } => eprintln!("  now using the {path} path"),
        DeviceEvent::Disconnected { reason } => eprintln!("Connection lost: {reason}"),
        DeviceEvent::PunchFailed { .. } => eprintln!("{HARD_NAT}"),
        DeviceEvent::PeerOffline => eprintln!(
            "Your Peckboard isn't reachable through the relay right now (offline, or this link was revoked)."
        ),
        DeviceEvent::Failed(e) => eprintln!("Connection failed: {e}"),
        DeviceEvent::Retrying { after } => eprintln!("Retrying in {}s…", after.as_secs()),
        DeviceEvent::Enrolled {
            box_fingerprint,
            legacy_upgrade,
        } => {
            if legacy_upgrade {
                eprintln!("Secured this pairing with a device key (box {box_fingerprint}).");
            } else {
                eprintln!("Paired with your Peckboard (box {box_fingerprint}).");
            }
        }
        DeviceEvent::EnrollRefused { reason } => eprintln!("{}", reason.message()),
        DeviceEvent::Activated => {}
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

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pc-test-{}", rand::random::<u64>()));
        d.join(name)
    }

    fn v2_link(n: u8) -> PairingLink {
        let b = peckboard_relay::identity::BoxIdentity::from_seed([n; 32]).public_key();
        PairingLink::new_v2(
            peckboard_relay::keys::PairingSecret::from_bytes([n; 32]),
            "relay.example",
            b,
            4_000_000_000,
        )
    }

    #[test]
    fn v2_link_saves_key_before_enrolling_and_reuses_it() {
        let path = tmp("connect-credential");
        let persist = Persist {
            path: Some(path.clone()),
            explicit: false,
        };
        let link = || Saved::Link {
            link: v2_link(1),
            key: None,
        };
        let DeviceCredential::Link { device_key: a, .. } =
            prepare(link(), &persist, false).unwrap()
        else {
            panic!("expected a link credential");
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // A rerun with the same link (crash before the grant was saved)
        // enrolls the same key.
        let DeviceCredential::Link { device_key: b, .. } =
            prepare(link(), &persist, false).unwrap()
        else {
            panic!("expected a link credential");
        };
        assert_eq!(a.to_bytes(), b.to_bytes());
        // Another box's link must not silently replace it.
        let other = Saved::Link {
            link: v2_link(2),
            key: None,
        };
        assert!(prepare(other, &persist, false).is_err());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn v2_link_with_no_save_is_refused() {
        let none = Persist {
            path: None,
            explicit: false,
        };
        let saved = Saved::Link {
            link: v2_link(1),
            key: None,
        };
        let e = prepare(saved, &none, false).unwrap_err().to_string();
        assert!(e.contains("--no-save"), "{e}");
        assert!(
            Args::command()
                .try_get_matches_from(["pc", "--no-save", "--save", "-"])
                .is_err()
        );
    }

    #[test]
    fn legacy_link_writes_nothing_unless_saved_then_upgrades() {
        let path = tmp("connect-credential");
        let legacy = || Saved::Link {
            link: PairingLink::new(
                peckboard_relay::keys::PairingSecret::from_bytes([3; 32]),
                "relay.example",
            ),
            key: None,
        };
        let implicit = Persist {
            path: Some(path.clone()),
            explicit: false,
        };
        let c = prepare(legacy(), &implicit, false).unwrap();
        assert!(matches!(
            c,
            DeviceCredential::Legacy {
                upgrade_key: None,
                ..
            }
        ));
        assert!(!path.exists());
        let explicit = Persist {
            path: Some(path.clone()),
            explicit: true,
        };
        let DeviceCredential::Legacy {
            upgrade_key: Some(k),
            link,
        } = prepare(legacy(), &explicit, false).unwrap()
        else {
            panic!("expected an upgradable legacy credential");
        };
        // The upgrade's `on_enrolled` rewrites the saved file as a
        // credential, which a later argument-less run loads.
        let cred = EnrolledCredential::new(
            peckboard_relay::keys::RendezvousSecret::from_bytes([4; 32]),
            k,
            peckboard_relay::identity::BoxIdentity::from_seed([5; 32]).public_key(),
            &link.relay,
        );
        write_private(&path, &format!("{}\n", cred.encode())).unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(matches!(parse_saved(&on_disk), Ok(Saved::Enrolled(_))));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}

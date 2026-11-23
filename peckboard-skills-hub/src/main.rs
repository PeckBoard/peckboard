//! `peckboard-skills-hub` binary: the server (default) and the local admin
//! CLI (`peckboard-skills-hub admin …`).

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use peckboard_skills_hub::{Config, Hub, admin, api, git::Repos, repos_dir, store, tls};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(version, about = "PeckBoard public skills hub")]
struct Args {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Public host name: the certificate name and the host of returned URLs.
    #[arg(
        long,
        env = "PECKBOARD_SKILLS_DOMAIN",
        default_value = "skills.peckboard.com"
    )]
    domain: String,
    /// Database, repos, backups, ACME cache and the admin key (0700).
    #[arg(
        long,
        env = "PECKBOARD_SKILLS_DATA_DIR",
        default_value = "/var/lib/peckboard-skills-hub",
        global = true
    )]
    data_dir: PathBuf,
    /// ACME account contact e-mail (optional).
    #[arg(long, env = "PECKBOARD_SKILLS_ACME_EMAIL")]
    acme_email: Option<String>,
    /// Use the Let's Encrypt staging directory.
    #[arg(long, env = "PECKBOARD_SKILLS_ACME_STAGING")]
    acme_staging: bool,
    /// Plain HTTP, no TLS/ACME (local testing, or behind a TLS proxy).
    #[arg(long, env = "PECKBOARD_SKILLS_HTTP_ONLY")]
    http_only: bool,
    /// Listen address (default [::]:443, or 127.0.0.1:8080 with --http-only).
    #[arg(long, env = "PECKBOARD_SKILLS_LISTEN")]
    listen: Option<SocketAddr>,
    /// Base of returned URLs (default https://<domain>).
    #[arg(long, env = "PECKBOARD_SKILLS_PUBLIC_URL")]
    public_url: Option<String>,
    /// Proof-of-work bits a never-seen key solves before its first request.
    #[arg(long, env = "PECKBOARD_SKILLS_NEW_KEY_POW_BITS", default_value_t = peckboard_skills_hub::DEFAULT_NEW_KEY_POW_BITS)]
    new_key_pow_bits: u32,
    /// Successful publishes per key per UTC day.
    #[arg(long, env = "PECKBOARD_SKILLS_PUBLISHES_PER_DAY", default_value_t = peckboard_skills_hub::DEFAULT_PUBLISHES_PER_DAY)]
    publishes_per_day: u32,
    /// Log filter (also RUST_LOG).
    #[arg(long, env = "RUST_LOG", default_value = "info")]
    log: String,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Administer the hub's data dir (run on the server).
    Admin {
        #[command(subcommand)]
        op: AdminOp,
    },
}

#[derive(Subcommand, Debug)]
enum AdminOp {
    /// Ban a publisher key (base64): its publishes are refused.
    Ban { key: String },
    /// Lift a ban.
    Unban { key: String },
    /// Remove a skill (all versions) or one version from the index and stop
    /// serving it.
    Takedown {
        name: String,
        version: Option<String>,
    },
    /// Import a PeckBoard/skills repo checkout (skills/<name>/<ver>/SKILL.md
    /// + groups/*.json), owned by the admin key.
    Import { dir: PathBuf },
    /// List skills, versions, groups and banned keys.
    List,
    /// Hand an original skill to another publisher key (base64), e.g. a
    /// skill imported under the admin key back to its author's install.
    Transfer { name: String, key: String },
}

fn main() -> anyhow::Result<()> {
    let mut args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(&args.log).unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr)
        .init();
    if let Some(Cmd::Admin { op }) = args.cmd.take() {
        return admin_cmd(&args, op);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(args))
}

fn admin_cmd(args: &Args, op: AdminOp) -> anyhow::Result<()> {
    let mut store = store::Store::open(&args.data_dir)?;
    let repos = Repos::open(repos_dir(&args.data_dir))?;
    // Same as server start: pre-git versions go into repos first, so an
    // import never commits ahead of them.
    store::migrate_to_git(&mut store, &repos)?;
    match op {
        AdminOp::Ban { key } => {
            admin::ban(&store, &key, true)?;
            println!("banned");
        }
        AdminOp::Unban { key } => {
            admin::ban(&store, &key, false)?;
            println!("unbanned");
        }
        AdminOp::Transfer { name, key } => {
            admin::transfer(&store, &name, &key)?;
            println!("transferred {name}");
        }
        AdminOp::Takedown { name, version } => {
            let n = admin::takedown(&store, &name, version.as_deref())?;
            println!("{n} version(s) taken down");
        }
        AdminOp::Import { dir } => {
            let owner = admin::admin_key(&args.data_dir)?;
            let rep = admin::import(&mut store, &repos, &dir, &owner)?;
            for w in &rep.warnings {
                eprintln!("warning: {w}");
            }
            for s in &rep.skipped {
                eprintln!("skipped: {s}");
            }
            println!(
                "imported {} version(s), {} group(s) as {owner}",
                rep.imported.len(),
                rep.groups
            );
        }
        AdminOp::List => {
            print!("{}", admin::list(&store)?);
        }
    }
    Ok(())
}

async fn run(args: Args) -> anyhow::Result<()> {
    let mut cfg = Config::new(&args.domain, args.data_dir.clone());
    if let Some(u) = &args.public_url {
        cfg.public_url = u.trim_end_matches('/').to_string();
    } else if args.http_only {
        let listen = args.listen.unwrap_or(([127, 0, 0, 1], 8080).into());
        cfg.public_url = format!("http://{listen}");
    }
    cfg.new_key_pow_bits = args.new_key_pow_bits;
    cfg.publishes_per_day = args.publishes_per_day;
    let hub = Hub::open(cfg)?;
    hub.start_background();
    let app = api::router(hub.clone());

    let server = if args.http_only {
        let listen = args.listen.unwrap_or(([127, 0, 0, 1], 8080).into());
        let tcp = TcpListener::bind(listen)
            .await
            .with_context(|| format!("bind {listen}"))?;
        info!(%listen, "serving plain HTTP");
        tokio::spawn(async move { axum::serve(tcp, app).await.map_err(anyhow::Error::from) })
    } else {
        let listen = args.listen.unwrap_or("[::]:443".parse()?);
        let tls_cfg = tls::acme(
            &args.domain,
            args.acme_email.as_deref(),
            &args.data_dir,
            !args.acme_staging,
        )?;
        let tcp = TcpListener::bind(listen)
            .await
            .with_context(|| format!("bind {listen}"))?;
        info!(%listen, domain = %args.domain, "serving HTTPS");
        tokio::spawn(async move {
            tls::serve(tcp, tls_cfg, app).await;
            Ok(())
        })
    };

    tokio::select! {
        r = server => r??,
        _ = shutdown_signal() => info!("shutting down"),
    }
    Ok(())
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

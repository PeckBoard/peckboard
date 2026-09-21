//! `peckboard-agent` — the remote-control daemon that runs on a user's
//! machine and dials home to Peckboard over an outbound WebSocket.
//!
//! Two subcommands:
//!   * `enroll --server <url> --token <t>` — persist local config.
//!   * `run` — connect and serve capabilities.

mod audit;
mod client;
mod config;
mod executor;
mod input;
mod input_exec;
mod screenshot;
mod server_mgmt;
mod terminal;

use anyhow::Context;
use clap::{Parser, Subcommand};

use crate::config::Config;

#[derive(Parser)]
#[command(
    name = "peckboard-agent",
    version,
    about = "Peckboard remote-control daemon"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Save server URL + enrollment token to the local config, then exit.
    Enroll {
        /// Base server URL, e.g. `https://your-host:3345`.
        #[arg(long)]
        server: String,
        /// One-time enrollment token issued by the Agents panel.
        #[arg(long)]
        token: String,
    },
    /// Connect to the enrolled server and serve capabilities.
    Run,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // rustls needs a process-wide crypto provider before any TLS dial.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "peckboard_agent=info".into()),
        )
        .init();

    match Cli::parse().command {
        Command::Enroll { server, token } => {
            // Preserve any existing capability flags / kill-switch; only
            // (re)write the server + token.
            let mut cfg = Config::load().unwrap_or_default();
            cfg.server_url = server;
            cfg.token = token;
            cfg.save().context("saving config")?;
            let path = Config::config_path()?;
            println!("Enrolled. Config saved to {}", path.display());
            println!(
                "Capabilities enabled: {}",
                cfg.enabled_capabilities().join(", ")
            );
        }
        Command::Run => {
            let cfg = Config::load()
                .context("no local config found — run `peckboard-agent enroll` first")?;
            // Point the shared audit log at <config_dir>/audit.jsonl before
            // we serve any capability, so every action is recorded on disk.
            audit::init(Config::audit_path()?);
            client::run(cfg).await?;
        }
    }
    Ok(())
}

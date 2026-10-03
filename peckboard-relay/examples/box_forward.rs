//! Minimal box: pair through the relay and forward every tunnel stream to
//! a local TCP port — lets you test `peckboard-connect` without Peckboard.
//!
//!   cargo run --manifest-path peckboard-relay/Cargo.toml --features tunnel \
//!     --example box_forward -- 8000 [--link peckboard://pair/...] [--relay host[:port]]
//!
//! Without `--link` a fresh pairing secret is generated and its link printed.

use std::net::SocketAddr;

use anyhow::Context;
use clap::Parser;
use peckboard_relay::keys::PairingSecret;
use peckboard_relay::proto::Role;
use peckboard_relay::tunnel::{
    DEFAULT_RELAY, PairingLink, TunnelError, establish, relay_config, serve_box,
};

#[derive(Parser)]
struct Args {
    /// Local port (on 127.0.0.1) every tunnel stream is forwarded to.
    port: u16,
    /// Reuse an existing pairing link instead of generating one.
    #[arg(long)]
    link: Option<String>,
    /// Relay host[:port] (overrides the link's).
    #[arg(long)]
    relay: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut link = match &args.link {
        Some(l) => PairingLink::parse(l)?,
        None => PairingLink::new(PairingSecret::generate(), DEFAULT_RELAY),
    };
    if let Some(r) = args.relay {
        link.relay = r;
    }
    println!("{}", link.to_uri());
    let target = SocketAddr::from(([127, 0, 0, 1], args.port));
    let cfg = relay_config(&link.relay).await.context("relay")?;
    loop {
        eprintln!("waiting for device…");
        let path = match establish(&cfg, &link.secret, Role::Box).await {
            Ok(p) => p,
            Err(e) => {
                if let Some(TunnelError::PunchFailed { .. }) = e.downcast_ref() {
                    eprintln!("punch failed: {e}");
                } else {
                    eprintln!("establish: {e:#}");
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
        };
        eprintln!("punched to {}", path.peer);
        if let Err(e) = serve_box(path, &link.secret, target, |ev| eprintln!("{ev:?}")).await {
            eprintln!("serve: {e:#}");
        }
    }
}

//! Production [`TunnelBackend`]: the `peckboard-relay` tunnel API
//! (`establish_with` as the box, then `serve_box`).

use std::net::SocketAddr;
use std::sync::Arc;

use peckboard_relay::keys::PairingSecret;
use peckboard_relay::proto::Role;
use peckboard_relay::tunnel::{self, Advertise, EstablishOptions, PunchedPath, TunnelEvent};

use super::secret::DeviceSecret;
use super::tunnel::{
    BoxIdentity, DirectOptions, IdentityStatus, OnRegistered, PunchedTunnel, Registered,
    TunnelBackend, TunnelEvents, TunnelUpdate,
};

/// Cap on one registration-status request (TLS connect + GET).
const STATUS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

pub struct RelayBackend;

fn relay_secret(s: &DeviceSecret) -> PairingSecret {
    PairingSecret::from_bytes(*s.as_bytes())
}

/// The fixed port on the configured public host, or on the STUN-observed
/// IP when there is none (or it doesn't resolve right now).
async fn advertise(direct: &DirectOptions) -> Vec<Advertise> {
    let Some(port) = direct.bind_port else {
        return Vec::new();
    };
    let Some(host) = &direct.public_host else {
        return vec![Advertise::Port(port)];
    };
    match tokio::net::lookup_host((host.as_str(), port)).await {
        Ok(addrs) => {
            let addrs: Vec<SocketAddr> = addrs.collect();
            match addrs.iter().find(|a| a.is_ipv4()).or(addrs.first()) {
                Some(a) => vec![Advertise::Addr(*a)],
                None => vec![Advertise::Port(port)],
            }
        }
        Err(e) => {
            tracing::warn!("remote access: resolving public address {host} failed: {e}");
            vec![Advertise::Port(port)]
        }
    }
}

#[async_trait::async_trait]
impl TunnelBackend for RelayBackend {
    async fn establish(
        &self,
        relay_host: &str,
        secret: &DeviceSecret,
        direct: &DirectOptions,
        identity: Option<&BoxIdentity>,
        on_registered: OnRegistered,
    ) -> anyhow::Result<Box<dyn PunchedTunnel>> {
        let cfg = tunnel::relay_config(relay_host).await?;
        let opts = EstablishOptions {
            bind_port: direct.bind_port,
            advertise: advertise(direct).await,
            public_ip_hint: direct.public_ip_hint,
            on_registered: Some(Arc::new(move |r: &tunnel::Registration| {
                on_registered(Registered {
                    local_port: r.local_port,
                    public: r.public,
                    candidates: r.candidates.clone(),
                    identity: r.identity,
                })
            })),
            identity: identity.cloned(),
            ..EstablishOptions::default()
        };
        let path = tunnel::establish_with(&cfg, &relay_secret(secret), Role::Box, &opts).await?;
        Ok(Box::new(RelayPunched(path)))
    }

    async fn registration_status(&self, relay_host: &str, key: &[u8; 32]) -> anyhow::Result<bool> {
        tokio::time::timeout(STATUS_TIMEOUT, tunnel::registration_status(relay_host, key))
            .await
            .map_err(|_| anyhow::anyhow!("relay {relay_host}: registration status timed out"))?
    }
}

struct RelayPunched(PunchedPath);

#[async_trait::async_trait]
impl PunchedTunnel for RelayPunched {
    fn peer(&self) -> SocketAddr {
        self.0.peer
    }

    fn path(&self) -> &'static str {
        self.0.kind().as_str()
    }

    fn relay_identity(&self) -> Option<IdentityStatus> {
        self.0.relay_identity
    }

    async fn serve(
        self: Box<Self>,
        secret: &DeviceSecret,
        target: SocketAddr,
        events: TunnelEvents,
    ) -> anyhow::Result<()> {
        tunnel::serve_box(self.0, &relay_secret(secret), target, move |ev| {
            events(match ev {
                TunnelEvent::Connected { rtt_ms, path, .. } => TunnelUpdate::Connected {
                    rtt_ms,
                    path: path.as_str(),
                },
                TunnelEvent::PathChanged { path } => TunnelUpdate::PathChanged {
                    path: path.as_str(),
                },
                TunnelEvent::Disconnected { reason } => TunnelUpdate::Disconnected { reason },
                TunnelEvent::Error(e) => TunnelUpdate::Error(e),
            })
        })
        .await
    }
}

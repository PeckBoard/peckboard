//! Production [`TunnelBackend`]: the `peckboard-relay` tunnel API
//! (`establish` as the box, then `serve_box`).

use std::net::SocketAddr;

use peckboard_relay::keys::PairingSecret;
use peckboard_relay::proto::Role;
use peckboard_relay::tunnel::{self, PunchedPath, TunnelEvent};

use super::secret::DeviceSecret;
use super::tunnel::{PunchedTunnel, TunnelBackend, TunnelEvents, TunnelUpdate};

pub struct RelayBackend;

fn relay_secret(s: &DeviceSecret) -> PairingSecret {
    PairingSecret::from_bytes(*s.as_bytes())
}

#[async_trait::async_trait]
impl TunnelBackend for RelayBackend {
    async fn establish(
        &self,
        relay_host: &str,
        secret: &DeviceSecret,
    ) -> anyhow::Result<Box<dyn PunchedTunnel>> {
        let cfg = tunnel::relay_config(relay_host).await?;
        let path = tunnel::establish(&cfg, &relay_secret(secret), Role::Box).await?;
        Ok(Box::new(RelayPunched(path)))
    }
}

struct RelayPunched(PunchedPath);

#[async_trait::async_trait]
impl PunchedTunnel for RelayPunched {
    fn peer(&self) -> SocketAddr {
        self.0.peer
    }

    async fn serve(
        self: Box<Self>,
        secret: &DeviceSecret,
        target: SocketAddr,
        events: TunnelEvents,
    ) -> anyhow::Result<()> {
        tunnel::serve_box(self.0, &relay_secret(secret), target, move |ev| {
            events(match ev {
                TunnelEvent::Connected { rtt_ms, .. } => TunnelUpdate::Connected { rtt_ms },
                TunnelEvent::Disconnected { reason } => TunnelUpdate::Disconnected { reason },
                TunnelEvent::Error(e) => TunnelUpdate::Error(e),
            })
        })
        .await
    }
}

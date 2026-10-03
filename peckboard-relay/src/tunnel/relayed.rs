//! Relay fallback path: QUIC over the relay's v2 data channel.
//!
//! When no punch round gets through (typically both peers behind
//! symmetric / carrier-grade NATs), [`establish_with`](super::establish_with)
//! keeps the relay session open and hands back a [`RelayedPath`]. Its
//! socket ([`MagicSock`], a quinn [`AsyncUdpSocket`]) carries every QUIC
//! datagram as a relay `Data` frame over the already-authenticated TLS
//! session, and the relay forwards it verbatim to the other peer. The
//! tunnel itself is unchanged: same QUIC, same pairing-pinned TLS 1.3 keys,
//! so the relay only ever sees QUIC ciphertext.
//!
//! Upgrade: while relayed, the box periodically asks the relay for another
//! punch round (both sides then punch from the socket `establish` used,
//! kept for this). On success the socket starts sending to the punched
//! address directly — QUIC never notices, its peer address stays the same
//! placeholder — and goes back to the relay if direct packets stop arriving.

use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use tokio::io::ReadBuf;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use super::{CRED_MARGIN, MAINTAIN_EVERY, PUNCH_TIMEOUT, PathKind};
use crate::client::{Event, RelayClient, RelayData};
use crate::proto::Role;

/// The direct path is in use while a packet arrived on it this recently
/// (the tunnel pings every 5 s, so this tolerates one lost ping).
const DIRECT_STALE: Duration = Duration::from_secs(12);
/// After a punch, also send directly for this long before any direct packet
/// has arrived — the peer may have attached too and be listening.
const PROBATION: Duration = Duration::from_secs(10);
/// Upgrade attempts back off up to this.
const MAX_UPGRADE_EVERY: Duration = Duration::from_secs(600);

/// A relayed path, ready for [`serve_box`](super::serve_box) /
/// [`connect_device`](super::connect_device). Owns the relay session (it
/// carries the data) and the punch socket (kept for upgrades); both close
/// when the tunnel's endpoint is dropped.
pub struct RelayedPath {
    relay: RelayClient,
    data: RelayData,
    udp: UdpSocket,
    upgrade_every: Option<Duration>,
    path_tx: watch::Sender<PathKind>,
}

impl fmt::Debug for RelayedPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RelayedPath")
            .field("local", &self.udp.local_addr().ok())
            .field("upgrade_every", &self.upgrade_every)
            .finish_non_exhaustive()
    }
}

impl RelayedPath {
    pub(super) fn new(
        relay: RelayClient,
        data: RelayData,
        udp: UdpSocket,
        upgrade_every: Option<Duration>,
    ) -> Self {
        Self {
            relay,
            data,
            udp,
            upgrade_every,
            path_tx: watch::Sender::new(PathKind::Relayed),
        }
    }

    /// Local address of the punch socket kept for upgrades.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.udp.local_addr()
    }

    /// Current path (`Relayed` until an upgrade succeeds) and its changes.
    pub fn path_watch(&self) -> watch::Receiver<PathKind> {
        self.path_tx.subscribe()
    }

    /// The QUIC socket; `peer` is the address QUIC addresses the other side
    /// by (any stable value — here its STUN-observed endpoint). Spawns the
    /// task that keeps the relay session alive and attempts upgrades; it
    /// stops when the socket is dropped.
    pub(super) fn into_socket(self, peer: SocketAddr) -> Arc<dyn AsyncUdpSocket> {
        let RelayedPath {
            relay,
            data,
            udp,
            upgrade_every,
            path_tx,
        } = self;
        let role = relay.role();
        let udp = Arc::new(udp);
        let stop = CancellationToken::new();
        let sock = Arc::new(MagicSock {
            sentinel: peer,
            relay_tx: data.tx,
            relay_rx: Mutex::new(data.rx),
            udp: udp.clone(),
            direct: Mutex::new(Direct::default()),
            path_tx,
            stop: stop.clone(),
        });
        tokio::spawn(maintain(
            relay,
            udp,
            Arc::downgrade(&sock),
            stop,
            (role == Role::Box).then_some(upgrade_every).flatten(),
        ));
        sock
    }
}

#[derive(Default)]
struct Direct {
    /// Punched peer address, once an upgrade succeeded.
    peer: Option<SocketAddr>,
    attached_at: Option<Instant>,
    last_rx: Option<Instant>,
    /// Last `poll_recv` waker: re-polled on attach so the direct socket is
    /// read without waiting for a relayed packet.
    waker: Option<Waker>,
}

/// quinn socket over the relay data channel, plus (after an upgrade) a
/// direct UDP path to the same peer. QUIC always sees one peer address
/// (`sentinel`); which path a datagram takes is decided here.
pub(super) struct MagicSock {
    sentinel: SocketAddr,
    relay_tx: mpsc::Sender<Vec<u8>>,
    relay_rx: Mutex<mpsc::Receiver<Vec<u8>>>,
    udp: Arc<UdpSocket>,
    direct: Mutex<Direct>,
    path_tx: watch::Sender<PathKind>,
    stop: CancellationToken,
}

impl fmt::Debug for MagicSock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MagicSock")
            .field("sentinel", &self.sentinel)
            .finish_non_exhaustive()
    }
}

impl Drop for MagicSock {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl MagicSock {
    /// Start using `peer` (a punched, two-way-proven address) directly.
    fn attach(&self, peer: SocketAddr) {
        let waker = {
            let mut d = self.direct.lock().unwrap();
            d.peer = Some(peer);
            d.attached_at = Some(Instant::now());
            d.last_rx = None;
            d.waker.take()
        };
        tracing::debug!(%peer, "relayed tunnel: direct path punched");
        if let Some(w) = waker {
            w.wake();
        }
    }

    fn attached(&self) -> bool {
        self.direct.lock().unwrap().peer.is_some()
    }

    fn set_path(&self, kind: PathKind) {
        self.path_tx.send_if_modified(|k| {
            let changed = *k != kind;
            *k = kind;
            changed
        });
    }

    /// Where the next datagram goes: (direct address, also via relay).
    fn route(&self) -> (Option<SocketAddr>, bool) {
        let now = Instant::now();
        let d = self.direct.lock().unwrap();
        let Some(peer) = d.peer else {
            return (None, true);
        };
        let fresh = d
            .last_rx
            .is_some_and(|t| now.saturating_duration_since(t) < DIRECT_STALE);
        let probing = d
            .attached_at
            .is_some_and(|t| now.saturating_duration_since(t) < PROBATION);
        drop(d);
        self.set_path(if fresh {
            PathKind::Direct
        } else {
            PathKind::Relayed
        });
        match (fresh, probing) {
            (true, _) => (Some(peer), false),
            (false, true) => (Some(peer), true),
            (false, false) => (None, true),
        }
    }
}

fn fill_meta(meta: &mut RecvMeta, addr: SocketAddr, len: usize) {
    meta.addr = addr;
    meta.len = len;
    meta.stride = len;
    meta.ecn = None;
    meta.dst_ip = None;
}

impl AsyncUdpSocket for MagicSock {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(AlwaysWritable)
    }

    /// Never blocks: a datagram that doesn't fit a queue is dropped, like
    /// UDP under load, and QUIC's congestion control reacts to the loss.
    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        let (direct, relay) = self.route();
        if let Some(peer) = direct {
            let _ = self.udp.try_send_to(transmit.contents, peer);
        }
        if relay {
            let _ = self.relay_tx.try_send(transmit.contents.to_vec());
        }
        Ok(())
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let mut n = 0;
        let peer = {
            let mut d = self.direct.lock().unwrap();
            d.waker = Some(cx.waker().clone());
            d.peer
        };
        if let Some(peer) = peer {
            while n < bufs.len().min(meta.len()) {
                let mut rb = ReadBuf::new(&mut bufs[n]);
                match self.udp.poll_recv_from(cx, &mut rb) {
                    Poll::Ready(Ok(from)) => {
                        // Only the punched peer; stray STUN / probes are not QUIC.
                        if from != peer {
                            continue;
                        }
                        let len = rb.filled().len();
                        fill_meta(&mut meta[n], self.sentinel, len);
                        n += 1;
                        self.direct.lock().unwrap().last_rx = Some(Instant::now());
                        self.set_path(PathKind::Direct);
                    }
                    // E.g. ICMP-induced errors: the relay path still works.
                    Poll::Ready(Err(_)) | Poll::Pending => break,
                }
            }
        }
        let mut rx = self.relay_rx.lock().unwrap();
        while n < bufs.len().min(meta.len()) {
            match rx.poll_recv(cx) {
                Poll::Ready(Some(p)) => {
                    let buf = &mut bufs[n];
                    let len = p.len().min(buf.len());
                    buf[..len].copy_from_slice(&p[..len]);
                    fill_meta(&mut meta[n], self.sentinel, len);
                    n += 1;
                }
                // Relay gone: only the direct path (if any) is left.
                Poll::Ready(None) | Poll::Pending => break,
            }
        }
        if n > 0 {
            Poll::Ready(Ok(n))
        } else {
            Poll::Pending
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.udp.local_addr()
    }

    fn may_fragment(&self) -> bool {
        false
    }
}

#[derive(Debug)]
struct AlwaysWritable;

impl UdpPoller for AlwaysWritable {
    fn poll_writable(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Keeps the relay session alive (pings; the relay drops sessions idle for
/// 90 s, and data may all go direct after an upgrade), keeps the punch
/// socket's NAT mapping and recorded endpoint fresh, and runs upgrade punch
/// rounds: the box asks for one every `upgrade_every` (doubling); both
/// sides punch on every `PunchNow` until one succeeds.
async fn maintain(
    mut relay: RelayClient,
    udp: Arc<UdpSocket>,
    sock: Weak<MagicSock>,
    stop: CancellationToken,
    upgrade_every: Option<Duration>,
) {
    let attached = || sock.upgrade().is_none_or(|s| s.attached());
    let mut tick =
        tokio::time::interval_at(tokio::time::Instant::now() + MAINTAIN_EVERY, MAINTAIN_EVERY);
    let mut every = upgrade_every.unwrap_or(MAX_UPGRADE_EVERY);
    let upgrade = tokio::time::sleep(every);
    tokio::pin!(upgrade);
    let mut refreshing = false;
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tick.tick() => {
                if relay.ping().await.is_err() {
                    return;
                }
                if attached() {
                    continue;
                }
                let cred = relay.stun_credential();
                if cred.expires.saturating_duration_since(Instant::now()) < CRED_MARGIN {
                    let _ = relay.refresh_stun().await;
                    refreshing = true;
                } else {
                    let _ = relay.stun_binding(&udp).await;
                }
            }
            _ = &mut upgrade, if upgrade_every.is_some() && !attached() => {
                tracing::debug!("relayed tunnel: requesting an upgrade punch");
                let _ = relay.request_punch().await;
                every = (every * 2).min(MAX_UPGRADE_EVERY);
                upgrade.as_mut().reset(tokio::time::Instant::now() + every);
            }
            ev = relay.next_event() => match ev {
                None => return, // relay gone; a direct path (if any) lives on
                Some(Event::CredentialRefreshed) if refreshing => {
                    refreshing = false;
                    if !attached() {
                        let _ = relay.stun_binding(&udp).await;
                    }
                }
                Some(Event::Punch(p)) if !attached() => {
                    let punched = tokio::select! {
                        r = relay.punch(&udp, &p, PUNCH_TIMEOUT) => r,
                        _ = stop.cancelled() => return,
                    };
                    match (punched, sock.upgrade()) {
                        (Ok(peer), Some(s)) => s.attach(peer),
                        (Ok(_), None) => return,
                        (Err(_), _) => tracing::debug!("relayed tunnel: upgrade punch failed"),
                    }
                }
                Some(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sock(udp: UdpSocket) -> (MagicSock, mpsc::Receiver<Vec<u8>>) {
        let (tx, out) = mpsc::channel(8);
        let (_in_tx, rx) = mpsc::channel(8);
        let s = MagicSock {
            sentinel: "203.0.113.1:1".parse().unwrap(),
            relay_tx: tx,
            relay_rx: Mutex::new(rx),
            udp: Arc::new(udp),
            direct: Mutex::new(Direct::default()),
            path_tx: watch::Sender::new(PathKind::Relayed),
            stop: CancellationToken::new(),
        };
        (s, out)
    }

    /// Relay only until a punch; then direct + relay on probation, direct
    /// only once the peer is heard directly, relay again when it goes quiet.
    #[tokio::test(start_paused = true)]
    async fn routes_follow_the_direct_path() {
        let (s, _out) = sock(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let peer: SocketAddr = "127.0.0.1:9".parse().unwrap();
        assert_eq!(s.route(), (None, true));
        s.attach(peer);
        assert_eq!(s.route(), (Some(peer), true));
        assert_eq!(*s.path_tx.borrow(), PathKind::Relayed);
        s.direct.lock().unwrap().last_rx = Some(Instant::now());
        assert_eq!(s.route(), (Some(peer), false));
        assert_eq!(*s.path_tx.borrow(), PathKind::Direct);
        s.direct.lock().unwrap().last_rx = Some(Instant::now() - DIRECT_STALE);
        s.direct.lock().unwrap().attached_at = Some(Instant::now() - PROBATION);
        assert_eq!(s.route(), (None, true));
        assert_eq!(*s.path_tx.borrow(), PathKind::Relayed);
    }
}

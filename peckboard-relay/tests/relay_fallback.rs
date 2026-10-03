//! Relay fallback: when hole punching is impossible (both peers behind
//! symmetric NATs), the tunnel runs over the relay's v2 data channel —
//! end-to-end encrypted QUIC the relay only forwards.
//!
//! NAT model: each peer reaches the relay's STUN port through its own shim
//! on another loopback address (`ClientConfig::stun_host`). The shim sends
//! on from a fresh socket and only lets the relay's replies back in, so the
//! STUN-observed endpoint each peer advertises drops the other peer's
//! probes — an endpoint-dependent (symmetric) mapping with
//! address-dependent filtering. LAN candidates are off (on loopback they'd
//! always punch). Linux-only: binds 127.0.0.2/3.

#![cfg(target_os = "linux")]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use peckboard_relay::client::{ClientConfig, Event, RelayClient};
use peckboard_relay::keys::PairingSecret;
use peckboard_relay::proto::Role;
use peckboard_relay::server::{Relay, RelayConfig};
use peckboard_relay::tls;
use peckboard_relay::tunnel::{
    Advertise, EstablishOptions, PathKind, PunchedPath, RelayFallback, TunnelError, TunnelEvent,
    connect_device, establish_with, serve_box,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;

const NAME: &str = "relay.test";
const T: Duration = Duration::from_secs(30);
const MARKER: &str = "PLAINTEXT-MARKER-7f3a9c";

struct Harness {
    relay: Relay,
    cfg: ClientConfig,
    stun: SocketAddr,
}

/// In-process relay. `v1_only`: a relay that predates the data channel.
async fn start(rc: RelayConfig, v1_only: bool) -> Harness {
    let rc = RelayConfig {
        auth_delay: Duration::from_millis(20),
        punch_lead: Duration::from_millis(50),
        ..rc
    };
    let (server_cfg, cert) = tls::self_signed(&[NAME.to_string()]).unwrap();
    let server_cfg = if v1_only {
        tls::v1_only(&server_cfg)
    } else {
        server_cfg
    };
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let stun = udp.local_addr().unwrap();
    let relay = Relay::new(rc, stun.port());
    let r = relay.clone();
    tokio::spawn(async move { r.serve_tls(tcp, TlsAcceptor::from(server_cfg)).await });
    let r = relay.clone();
    tokio::spawn(async move { r.serve_stun(udp).await });
    Harness {
        relay,
        cfg: ClientConfig::pinned(addr, NAME, cert).unwrap(),
        stun,
    }
}

/// A symmetric NAT in front of one peer's STUN traffic; returns the client
/// config that uses it (see the module docs).
async fn behind_nat(h: &Harness, ip: Ipv4Addr) -> ClientConfig {
    let front = Arc::new(UdpSocket::bind((ip, h.stun.port())).await.unwrap());
    let back = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let client: Arc<Mutex<Option<SocketAddr>>> = Arc::default();
    let (f, b, c, relay) = (front.clone(), back.clone(), client.clone(), h.stun);
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = f.recv_from(&mut buf).await {
            *c.lock().unwrap() = Some(from);
            let _ = b.send_to(&buf[..n], relay).await;
        }
    });
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = back.recv_from(&mut buf).await {
            // Unsolicited (anyone but the relay): dropped.
            let to = *client.lock().unwrap();
            if let (true, Some(to)) = (from == relay, to) {
                let _ = front.send_to(&buf[..n], to).await;
            }
        }
    });
    ClientConfig {
        stun_host: Some(IpAddr::V4(ip)),
        ..h.cfg.clone()
    }
}

fn opts(upgrade_every: Option<Duration>) -> EstablishOptions {
    EstablishOptions {
        fallback: RelayFallback {
            after_failures: 1,
            upgrade_every,
            ..RelayFallback::default()
        },
        no_lan_candidate: true,
        ..EstablishOptions::default()
    }
}

// ---- local HTTP + WebSocket echo ------------------------------------------

async fn echo_server() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            tokio::spawn(echo_conn(s));
        }
    });
    addr
}

async fn read_head(s: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut b = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if s.read(&mut b).await.unwrap() == 0 {
            break;
        }
        buf.push(b[0]);
    }
    String::from_utf8(buf).unwrap()
}

/// `echo <request line>` for HTTP; for an `Upgrade` request a 101 and then
/// echoes every small masked text frame back unmasked.
async fn echo_conn(mut s: TcpStream) {
    let head = read_head(&mut s).await;
    if !head.contains("Upgrade: websocket") {
        let body = format!("echo {}", head.lines().next().unwrap_or(""));
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = s.write_all(resp.as_bytes()).await;
        return;
    }
    let _ = s
        .write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n")
        .await;
    loop {
        let mut hdr = [0u8; 2];
        if s.read_exact(&mut hdr).await.is_err() {
            return;
        }
        let len = (hdr[1] & 0x7f) as usize;
        let mut mask = [0u8; 4];
        let mut p = vec![0u8; len];
        if s.read_exact(&mut mask).await.is_err() || s.read_exact(&mut p).await.is_err() {
            return;
        }
        for (i, b) in p.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
        let mut out = vec![0x81, len as u8];
        out.extend_from_slice(&p);
        let _ = s.write_all(&out).await;
    }
}

async fn http_get(port: u16, path: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    tokio::time::timeout(T, s.read_to_string(&mut out))
        .await
        .unwrap()
        .unwrap();
    out
}

async fn websocket_roundtrip(port: u16, msg: &str) {
    let mut ws = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    ws.write_all(
        b"GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
    )
    .await
    .unwrap();
    let head = tokio::time::timeout(T, read_head(&mut ws)).await.unwrap();
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    for _ in 0..2 {
        let mask = [9u8, 8, 7, 6];
        let mut f = vec![0x81, 0x80 | msg.len() as u8];
        f.extend_from_slice(&mask);
        f.extend(msg.bytes().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        ws.write_all(&f).await.unwrap();
        let mut back = vec![0u8; 2 + msg.len()];
        tokio::time::timeout(T, ws.read_exact(&mut back))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&back[2..], msg.as_bytes());
    }
}

// ---- tunnel helpers --------------------------------------------------------

type Events = mpsc::UnboundedReceiver<TunnelEvent>;

fn events() -> (impl Fn(TunnelEvent) + Send + Sync + 'static, Events) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        move |ev| {
            let _ = tx.send(ev);
        },
        rx,
    )
}

async fn wait_event(rx: &mut Events, want: impl Fn(&TunnelEvent) -> bool) -> TunnelEvent {
    tokio::time::timeout(T, async {
        loop {
            let ev = rx.recv().await.expect("tunnel ended");
            if want(&ev) {
                return ev;
            }
        }
    })
    .await
    .expect("tunnel event did not arrive")
}

struct Pair {
    port: u16,
    box_ev: Events,
    dev_ev: Events,
}

/// Rendezvous both sides with `bopts`/`dopts`, serve the box's echo target
/// and expose the device's listener; asserts both start on `want`.
async fn tunnel(
    bcfg: ClientConfig,
    dcfg: ClientConfig,
    bopts: EstablishOptions,
    dopts: EstablishOptions,
    want: PathKind,
) -> Pair {
    let target = echo_server().await;
    let s = PairingSecret::generate();
    let (bs, ds) = (s.clone(), s);
    let boxed = tokio::spawn(async move {
        establish_with(&bcfg, &bs, Role::Box, &bopts)
            .await
            .map(|p| (p, bs))
    });
    let dev = tokio::time::timeout(T, establish_with(&dcfg, &ds, Role::Device, &dopts))
        .await
        .expect("device establish timed out")
        .expect("device establish failed");
    let (bpath, bs) = tokio::time::timeout(T, boxed)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(bpath.kind(), want, "box path");
    assert_eq!(dev.kind(), want, "device path");
    serve(bpath, bs, dev, ds, target).await
}

async fn serve(
    bpath: PunchedPath,
    bs: PairingSecret,
    dpath: PunchedPath,
    ds: PairingSecret,
    target: SocketAddr,
) -> Pair {
    let (on_box, mut box_ev) = events();
    tokio::spawn(async move { serve_box(bpath, &bs, target, on_box).await });
    let listen = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listen.local_addr().unwrap().port();
    let (on_dev, mut dev_ev) = events();
    tokio::spawn(async move { connect_device(dpath, &ds, &listen, on_dev).await });
    for rx in [&mut dev_ev, &mut box_ev] {
        wait_event(rx, |e| matches!(e, TunnelEvent::Connected { .. })).await;
    }
    Pair {
        port,
        box_ev,
        dev_ev,
    }
}

// ---- tests -----------------------------------------------------------------

/// Both peers behind symmetric NATs: the punch fails, both switch to the
/// relay, and the tunnel carries HTTP and a WebSocket end to end.
#[tokio::test]
async fn symmetric_nats_fall_back_to_relayed_tunnel() {
    let h = start(RelayConfig::default(), false).await;
    let bcfg = behind_nat(&h, Ipv4Addr::new(127, 0, 0, 2)).await;
    let dcfg = behind_nat(&h, Ipv4Addr::new(127, 0, 0, 3)).await;
    let mut p = tunnel(bcfg, dcfg, opts(None), opts(None), PathKind::Relayed).await;

    let r = http_get(p.port, "/hello").await;
    assert!(r.ends_with("echo GET /hello HTTP/1.1"), "{r}");
    let (a, b) = tokio::join!(http_get(p.port, "/a"), http_get(p.port, "/b"));
    assert!(a.ends_with("GET /a HTTP/1.1") && b.ends_with("GET /b HTTP/1.1"));
    websocket_roundtrip(p.port, "over the relay").await;
    assert!(
        h.relay.relay_stats().packets > 0,
        "{:?}",
        h.relay.relay_stats()
    );
    // Nothing reported a path change: it stayed relayed.
    assert!(p.box_ev.try_recv().is_err() && p.dev_ev.try_recv().is_err());
}

/// Every datagram the relay forwards is a QUIC packet (fixed bit set, long
/// headers carry QUIC v1) and never contains the plaintext that went
/// through the tunnel.
#[tokio::test]
async fn relay_only_sees_ciphertext() {
    let h = start(RelayConfig::default(), false).await;
    let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let s = seen.clone();
    h.relay
        .set_data_tap(move |p| s.lock().unwrap().push(p.to_vec()));
    let bcfg = behind_nat(&h, Ipv4Addr::new(127, 0, 0, 2)).await;
    let dcfg = behind_nat(&h, Ipv4Addr::new(127, 0, 0, 3)).await;
    let p = tunnel(bcfg, dcfg, opts(None), opts(None), PathKind::Relayed).await;

    let r = http_get(p.port, &format!("/{MARKER}")).await;
    assert!(r.contains(MARKER), "{r}");
    websocket_roundtrip(p.port, MARKER).await;

    let seen = seen.lock().unwrap();
    assert!(seen.len() > 10, "only {} packets relayed", seen.len());
    let mut long = 0;
    for pkt in seen.iter() {
        assert!(
            !pkt.windows(MARKER.len()).any(|w| w == MARKER.as_bytes()),
            "plaintext crossed the relay"
        );
        // (No fixed-bit check: quinn greases it, RFC 9287.)
        if pkt[0] & 0x80 != 0 {
            assert_eq!(&pkt[1..5], &[0, 0, 0, 1], "long header without QUIC v1");
            long += 1;
        }
    }
    // The handshake (long headers) went through the relay too: the TLS 1.3
    // inside it is pinned to pairing keys the relay doesn't have.
    assert!(long > 0);
}

/// The per-id byte bucket caps what one pairing can push through the
/// relay; the excess is dropped and counted.
#[tokio::test]
async fn relay_rate_limit_per_id() {
    let h = start(
        RelayConfig {
            relay_rate_per_id: 20_000.0,
            relay_burst_per_id: 40_000.0,
            ..RelayConfig::default()
        },
        false,
    )
    .await;
    let s = PairingSecret::generate();
    let mut boxc = RelayClient::connect(&h.cfg, &s, Role::Box).await.unwrap();
    let mut dev = RelayClient::connect(&h.cfg, &s, Role::Device)
        .await
        .unwrap();
    assert_eq!((boxc.protocol_version(), dev.protocol_version()), (2, 2));
    for c in [&mut boxc, &mut dev] {
        tokio::time::timeout(T, async {
            while !matches!(c.next_event().await, Some(Event::PeerOnline)) {}
        })
        .await
        .unwrap();
    }
    let bdata = boxc.take_data().unwrap();
    let mut ddata = dev.take_data().unwrap();
    let t0 = Instant::now();
    for i in 0..300u32 {
        let mut pkt = vec![0x40; 1000];
        pkt[..4].copy_from_slice(&i.to_be_bytes());
        while bdata.tx.try_send(pkt.clone()).is_err() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
    let mut got = 0usize;
    while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(500), ddata.rx.recv()).await
    {
        got += 1;
    }
    let elapsed = t0.elapsed().as_secs_f64();
    let allowed = (40_000.0 + 20_000.0 * elapsed) / 1000.0;
    assert!(got > 0, "nothing relayed");
    assert!(
        (got as f64) <= allowed + 1.0,
        "{got} packets relayed, at most {allowed:.0} allowed in {elapsed:.2} s"
    );
    let st = h.relay.relay_stats();
    assert!(st.dropped_limit > 0, "{st:?}");
    assert_eq!(st.packets as usize, got);
    assert_eq!(st.active_pairs, 1);
}

/// Compatibility: a v1-only (old) device. Directly reachable it still
/// tunnels; behind a symmetric NAT nobody relays — both sides fail the
/// punch exactly as before.
#[tokio::test]
async fn old_client_keeps_direct_only_behaviour() {
    let h = start(RelayConfig::default(), false).await;
    let old = ClientConfig {
        v1_only: true,
        ..h.cfg.clone()
    };
    let probe = RelayClient::connect(&old, &PairingSecret::generate(), Role::Device)
        .await
        .unwrap();
    assert_eq!(probe.protocol_version(), 1);
    drop(probe);

    // Direct (LAN candidate on): works as ever.
    let p = tunnel(
        h.cfg.clone(),
        old.clone(),
        EstablishOptions::default(),
        EstablishOptions::default(),
        PathKind::Direct,
    )
    .await;
    assert!(
        http_get(p.port, "/old")
            .await
            .ends_with("echo GET /old HTTP/1.1")
    );

    // Symmetric NATs: the new box never relays to an old device.
    let bcfg = behind_nat(&h, Ipv4Addr::new(127, 0, 0, 2)).await;
    let dcfg = ClientConfig {
        v1_only: true,
        ..behind_nat(&h, Ipv4Addr::new(127, 0, 0, 3)).await
    };
    let s = PairingSecret::generate();
    let bs = s.clone();
    let boxed =
        tokio::spawn(async move { establish_with(&bcfg, &bs, Role::Box, &opts(None)).await });
    let dev = tokio::time::timeout(
        Duration::from_secs(90),
        establish_with(&dcfg, &s, Role::Device, &opts(None)),
    )
    .await
    .unwrap();
    assert!(
        matches!(
            dev.as_ref()
                .err()
                .and_then(|e| e.downcast_ref::<TunnelError>()),
            Some(TunnelError::PunchFailed { .. })
        ),
        "device: {:?}",
        dev.map(|p| p.kind())
    );
    let b = tokio::time::timeout(Duration::from_secs(90), boxed)
        .await
        .unwrap()
        .unwrap();
    assert!(b.is_err(), "box: {:?}", b.map(|p| p.kind()));
    assert_eq!(h.relay.relay_stats().packets, 0);
}

/// Compatibility: new peers against a relay that predates the data channel
/// negotiate v1 and fail a hopeless punch as before (no relaying).
#[tokio::test]
async fn new_clients_on_old_relay_fail_punch_as_before() {
    let h = start(RelayConfig::default(), true).await;
    let c = RelayClient::connect(&h.cfg, &PairingSecret::generate(), Role::Box)
        .await
        .unwrap();
    assert_eq!(c.protocol_version(), 1);
    drop(c);
    let bcfg = behind_nat(&h, Ipv4Addr::new(127, 0, 0, 2)).await;
    let dcfg = behind_nat(&h, Ipv4Addr::new(127, 0, 0, 3)).await;
    let s = PairingSecret::generate();
    let bs = s.clone();
    let boxed =
        tokio::spawn(async move { establish_with(&bcfg, &bs, Role::Box, &opts(None)).await });
    let dev = tokio::time::timeout(
        Duration::from_secs(90),
        establish_with(&dcfg, &s, Role::Device, &opts(None)),
    )
    .await
    .unwrap();
    assert!(
        matches!(
            dev.as_ref()
                .err()
                .and_then(|e| e.downcast_ref::<TunnelError>()),
            Some(TunnelError::PunchFailed { .. })
        ),
        "device: {:?}",
        dev.map(|p| p.kind())
    );
    let b = tokio::time::timeout(Duration::from_secs(90), boxed)
        .await
        .unwrap()
        .unwrap();
    assert!(b.is_err(), "box: {:?}", b.map(|p| p.kind()));
}

/// UDP port-forward to the box that can be switched on later: from the
/// box's socket to the last other sender, from anyone else to the box.
async fn switchable_forward(box_addr: SocketAddr) -> (SocketAddr, Arc<AtomicBool>) {
    let f = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = f.local_addr().unwrap();
    let open = Arc::new(AtomicBool::new(false));
    let o = open.clone();
    tokio::spawn(async move {
        let mut other: Option<SocketAddr> = None;
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = f.recv_from(&mut buf).await {
            if !o.load(Ordering::SeqCst) {
                continue;
            }
            if from == box_addr {
                if let Some(to) = other {
                    let _ = f.send_to(&buf[..n], to).await;
                }
            } else {
                other = Some(from);
                let _ = f.send_to(&buf[..n], box_addr).await;
            }
        }
    });
    (addr, open)
}

/// Relayed → direct: the box advertises a port-forward that is closed at
/// first (so the tunnel starts relayed); once it opens, the box's upgrade
/// round punches through it and both sides move to the direct path while
/// the tunnel stays up.
#[tokio::test]
async fn relayed_tunnel_upgrades_to_direct() {
    let h = start(RelayConfig::default(), false).await;
    let bport = UdpSocket::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let (fwd, open) = switchable_forward(SocketAddr::from(([127, 0, 0, 1], bport))).await;
    let bopts = EstablishOptions {
        bind_port: Some(bport),
        advertise: vec![Advertise::Addr(fwd)],
        ..opts(Some(Duration::from_secs(1)))
    };
    let bcfg = behind_nat(&h, Ipv4Addr::new(127, 0, 0, 2)).await;
    let dcfg = behind_nat(&h, Ipv4Addr::new(127, 0, 0, 3)).await;
    let mut p = tunnel(bcfg, dcfg, bopts, opts(None), PathKind::Relayed).await;
    assert!(
        http_get(p.port, "/before")
            .await
            .ends_with("GET /before HTTP/1.1")
    );

    open.store(true, Ordering::SeqCst);
    for rx in [&mut p.box_ev, &mut p.dev_ev] {
        wait_event(rx, |e| {
            matches!(
                e,
                TunnelEvent::PathChanged {
                    path: PathKind::Direct
                }
            )
        })
        .await;
    }
    // Still the same tunnel, now direct: requests flow, the relay is idle.
    let before = h.relay.relay_stats().packets;
    assert!(
        http_get(p.port, "/after")
            .await
            .ends_with("GET /after HTTP/1.1")
    );
    websocket_roundtrip(p.port, "direct now").await;
    let after = h.relay.relay_stats().packets;
    assert!(
        after - before < 5,
        "relay still carried {} packets",
        after - before
    );
    assert!(
        p.box_ev.try_recv().is_err() && p.dev_ev.try_recv().is_err(),
        "tunnel changed again"
    );
}

/// Only device→box gets through (box behind a symmetric NAT with a
/// forward that drops replies, device on CGNAT): the box hears probes but
/// its acks never arrive, so neither side may call the punch a success —
/// both must end up relayed instead of a dead "direct" path.
#[tokio::test]
async fn one_way_path_falls_back_to_relay() {
    let h = start(RelayConfig::default(), false).await;
    let bport = UdpSocket::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let box_addr = SocketAddr::from(([127, 0, 0, 1], bport));
    let f = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let fwd = f.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = f.recv_from(&mut buf).await {
            if from != box_addr {
                let _ = f.send_to(&buf[..n], box_addr).await;
            }
        }
    });
    let bopts = EstablishOptions {
        bind_port: Some(bport),
        advertise: vec![Advertise::Addr(fwd)],
        ..opts(None)
    };
    let bcfg = behind_nat(&h, Ipv4Addr::new(127, 0, 0, 2)).await;
    let dcfg = behind_nat(&h, Ipv4Addr::new(127, 0, 0, 3)).await;
    let p = tunnel(bcfg, dcfg, bopts, opts(None), PathKind::Relayed).await;
    assert!(
        http_get(p.port, "/oneway")
            .await
            .ends_with("GET /oneway HTTP/1.1")
    );
}

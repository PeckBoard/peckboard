//! Tunnel data path over loopback: in-process relay, a box forwarding to a
//! local HTTP + WebSocket echo server, and a device exposing a local port.

use std::net::SocketAddr;
use std::time::Duration;

use base64::Engine;
use peckboard_relay::client::ClientConfig;
use peckboard_relay::keys::PairingSecret;
use peckboard_relay::proto::Role;
use peckboard_relay::server::{Relay, RelayConfig};
use peckboard_relay::tls;
use peckboard_relay::tunnel::{
    CancellationToken, CookieGate, DeviceEvent, DeviceOptions, ListenAddr, PairingLink,
    PunchedPath, STREAM_PING, STREAM_TCP, TunnelEvent, bind_listener, connect_device, connect_raw,
    establish, run_device, serve_box,
};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;

const NAME: &str = "relay.test";
const T: Duration = Duration::from_secs(10);

async fn relay() -> ClientConfig {
    let rc = RelayConfig {
        auth_delay: Duration::from_millis(20),
        punch_lead: Duration::from_millis(50),
        ..RelayConfig::default()
    };
    let (server_cfg, cert) = tls::self_signed(&[NAME.to_string()]).unwrap();
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let relay = Relay::new(rc, udp.local_addr().unwrap().port());
    let r = relay.clone();
    tokio::spawn(async move { r.serve_tls(tcp, TlsAcceptor::from(server_cfg)).await });
    tokio::spawn(async move { relay.serve_stun(udp).await });
    ClientConfig::pinned(addr, NAME, cert).unwrap()
}

/// HTTP/1.1 echo (`echo <request line>`) that also upgrades to a WebSocket
/// echoing text frames. Returns its address and a counter of accepted conns.
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

async fn echo_conn(mut s: TcpStream) {
    let head = read_head(&mut s).await;
    let key = head
        .lines()
        .find_map(|l| l.strip_prefix("Sec-WebSocket-Key: "))
        .map(str::to_string);
    let Some(key) = key else {
        let body = format!("echo {}", head.lines().next().unwrap_or(""));
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(resp.as_bytes()).await.unwrap();
        return;
    };
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    h.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let accept = base64::engine::general_purpose::STANDARD.encode(h.finalize());
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    s.write_all(resp.as_bytes()).await.unwrap();
    // Echo small masked text frames back unmasked.
    loop {
        let mut hdr = [0u8; 2];
        if s.read_exact(&mut hdr).await.is_err() {
            return;
        }
        let len = (hdr[1] & 0x7f) as usize;
        let mut mask = [0u8; 4];
        s.read_exact(&mut mask).await.unwrap();
        let mut p = vec![0u8; len];
        s.read_exact(&mut p).await.unwrap();
        for (i, b) in p.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
        let mut out = vec![0x81, len as u8];
        out.extend_from_slice(&p);
        s.write_all(&out).await.unwrap();
    }
}

fn events() -> (
    impl Fn(TunnelEvent) + Send + Sync + 'static,
    mpsc::UnboundedReceiver<TunnelEvent>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        move |ev| {
            let _ = tx.send(ev);
        },
        rx,
    )
}

async fn connected(rx: &mut mpsc::UnboundedReceiver<TunnelEvent>) {
    match tokio::time::timeout(T, rx.recv()).await.unwrap() {
        Some(TunnelEvent::Connected { .. }) => {}
        other => panic!("expected Connected, got {other:?}"),
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

#[tokio::test]
async fn http_and_websocket_through_tunnel() {
    let cfg = relay().await;
    let target = echo_server().await;
    let s = PairingSecret::generate();

    let (bcfg, bs) = (cfg.clone(), s.clone());
    let (on_box, mut box_ev) = events();
    tokio::spawn(async move {
        let path = establish(&bcfg, &bs, Role::Box).await.unwrap();
        serve_box(path, &bs, target, on_box).await
    });
    let path = tokio::time::timeout(T, establish(&cfg, &s, Role::Device))
        .await
        .unwrap()
        .unwrap();
    let listen = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listen.local_addr().unwrap().port();
    let (on_dev, mut dev_ev) = events();
    let ds = s.clone();
    tokio::spawn(async move { connect_device(path, &ds, &listen, on_dev).await });
    connected(&mut dev_ev).await;
    connected(&mut box_ev).await;

    // HTTP: several requests, including concurrent ones (one stream each).
    let r = http_get(port, "/hello").await;
    assert!(r.starts_with("HTTP/1.1 200 OK"), "{r}");
    assert!(r.ends_with("echo GET /hello HTTP/1.1"), "{r}");
    let (a, b) = tokio::join!(http_get(port, "/a"), http_get(port, "/b"));
    assert!(a.ends_with("GET /a HTTP/1.1") && b.ends_with("GET /b HTTP/1.1"));

    // WebSocket upgrade + round trips on one long-lived stream.
    let mut ws = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    ws.write_all(
        b"GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
          Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
    )
    .await
    .unwrap();
    let head = read_head(&mut ws).await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert!(head.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
    for msg in ["hello ws", "second frame"] {
        let mask = [1u8, 2, 3, 4];
        let mut f = vec![0x81, 0x80 | msg.len() as u8];
        f.extend_from_slice(&mask);
        f.extend(msg.bytes().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        ws.write_all(&f).await.unwrap();
        let mut back = vec![0u8; 2 + msg.len()];
        tokio::time::timeout(T, ws.read_exact(&mut back))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&back[..2], &[0x81, msg.len() as u8]);
        assert_eq!(&back[2..], msg.as_bytes());
    }
}

/// Two loopback sockets that can already reach each other — what a
/// successful punch hands over.
async fn paths() -> (PunchedPath, PunchedPath) {
    let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (aa, ba) = (a.local_addr().unwrap(), b.local_addr().unwrap());
    (
        PunchedPath {
            socket: a.into(),
            peer: ba,
            role: Role::Box,
        },
        PunchedPath {
            socket: b.into(),
            peer: aa,
            role: Role::Device,
        },
    )
}

#[tokio::test]
async fn wrong_secret_cannot_connect() {
    let target = echo_server().await;
    let (bp, dp) = paths().await;
    let s = PairingSecret::generate();
    let (on_box, mut box_ev) = events();
    let srv = tokio::spawn(async move { serve_box(bp, &s, target, on_box).await });
    let listen = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (on_dev, _dev_ev) = events();
    let r = tokio::time::timeout(
        Duration::from_secs(20),
        connect_device(dp, &PairingSecret::generate(), &listen, on_dev),
    )
    .await
    .unwrap();
    assert!(r.is_err(), "device with the wrong secret connected");
    assert!(srv.await.unwrap().is_err());
    assert!(matches!(box_ev.recv().await, Some(TunnelEvent::Error(_))));
}

#[tokio::test]
async fn device_cannot_choose_target() {
    let target = echo_server().await;
    // A service the device would like to reach instead of the target.
    let other = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let other_addr = other.local_addr().unwrap();
    let (bp, dp) = paths().await;
    let s = PairingSecret::generate();
    let bs = s.clone();
    let (on_box, _box_ev) = events();
    tokio::spawn(async move { serve_box(bp, &bs, target, on_box).await });
    let (_ep, conn) = connect_raw(dp, &s).await.unwrap();

    // Unknown stream types carrying an address are reset, never dialled.
    for ty in [0x00u8, 0x03, 0xff] {
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        send.write_all(&[ty]).await.unwrap();
        send.write_all(other_addr.to_string().as_bytes())
            .await
            .unwrap();
        let _ = send.finish();
        let r = tokio::time::timeout(T, recv.read_to_end(1024))
            .await
            .unwrap();
        assert!(r.is_err(), "type {ty:#x} stream was not reset");
    }
    // A TCP stream whose payload names another address still lands on the
    // box's target.
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(&[STREAM_TCP]).await.unwrap();
    let req = format!("CONNECT {other_addr} HTTP/1.1\r\nHost: {other_addr}\r\n\r\n");
    send.write_all(req.as_bytes()).await.unwrap();
    let _ = send.finish();
    let body = tokio::time::timeout(T, recv.read_to_end(4096))
        .await
        .unwrap()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&body).ends_with(&format!("echo CONNECT {other_addr} HTTP/1.1"))
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(300), other.accept())
            .await
            .is_err(),
        "box dialled a device-chosen address"
    );
}

/// A stranger can't take the box's one accept slot: QUIC from an address
/// other than the punched peer is refused, and a failed or stalled handshake
/// from the peer's own address doesn't stop the real device connecting.
#[tokio::test]
async fn stranger_cannot_block_the_paired_device() {
    let target = echo_server().await;
    let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let aa = a.local_addr().unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ba = b.local_addr().unwrap();
    let s = PairingSecret::generate();
    let bs = s.clone();
    let (on_box, mut box_ev) = events();
    let bp = PunchedPath {
        socket: a.into(),
        peer: ba,
        role: Role::Box,
    };
    tokio::spawn(async move { serve_box(bp, &bs, target, on_box).await });
    let dev_path = |sock: UdpSocket| PunchedPath {
        socket: sock.into(),
        peer: aa,
        role: Role::Device,
    };

    // Right secret, wrong address: refused.
    let stranger = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    assert!(connect_raw(dev_path(stranger), &s).await.is_err());
    // From the peer's address but unauthenticated: fails, box keeps going.
    let dp = dev_path(b);
    assert!(connect_raw(dp, &PairingSecret::generate()).await.is_err());
    // The real device, same address, still gets in.
    let b = loop {
        match UdpSocket::bind(ba).await {
            Ok(b) => break b,
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    };
    let (_ep, conn) = tokio::time::timeout(T, connect_raw(dev_path(b), &s))
        .await
        .unwrap()
        .unwrap();
    connected(&mut box_ev).await;
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(&[STREAM_PING, 7]).await.unwrap();
    let mut byte = [0u8; 1];
    tokio::time::timeout(T, recv.read_exact(&mut byte))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(byte, [7]);
}

#[tokio::test]
async fn box_echoes_pings() {
    let target = echo_server().await;
    let (bp, dp) = paths().await;
    let s = PairingSecret::generate();
    let bs = s.clone();
    let (on_box, _box_ev) = events();
    tokio::spawn(async move { serve_box(bp, &bs, target, on_box).await });
    let (_ep, conn) = connect_raw(dp, &s).await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(&[STREAM_PING, 7]).await.unwrap();
    let mut b = [0u8; 1];
    tokio::time::timeout(T, recv.read_exact(&mut b))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(b, [7]);
}

/// Like [`paths`], but every packet goes through a UDP forwarder; aborting
/// it black-holes the path, like a box that died without a CONNECTION_CLOSE.
async fn proxied_paths() -> (PunchedPath, PunchedPath, tokio::task::JoinHandle<()>) {
    let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let p = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (aa, ba, pa) = (
        a.local_addr().unwrap(),
        b.local_addr().unwrap(),
        p.local_addr().unwrap(),
    );
    let fwd = tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        while let Ok((n, from)) = p.recv_from(&mut buf).await {
            let to = if from == aa { ba } else { aa };
            let _ = p.send_to(&buf[..n], to).await;
        }
    });
    (
        PunchedPath {
            socket: a.into(),
            peer: pa,
            role: Role::Box,
        },
        PunchedPath {
            socket: b.into(),
            peer: pa,
            role: Role::Device,
        },
        fwd,
    )
}

#[tokio::test]
async fn device_notices_vanished_box() {
    let target = echo_server().await;
    let (bp, dp, fwd) = proxied_paths().await;
    let s = PairingSecret::generate();
    let bs = s.clone();
    let (on_box, mut box_ev) = events();
    let srv = tokio::spawn(async move { serve_box(bp, &bs, target, on_box).await });
    let listen = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listen.local_addr().unwrap().port();
    let (on_dev, mut dev_ev) = events();
    let dev = tokio::spawn(async move { connect_device(dp, &s, &listen, on_dev).await });
    connected(&mut dev_ev).await;
    connected(&mut box_ev).await;
    assert!(http_get(port, "/").await.starts_with("HTTP/1.1 200 OK"));

    // The box vanishes: network path first (so its close never arrives),
    // then its task.
    fwd.abort();
    let _ = fwd.await;
    srv.abort();
    let t0 = std::time::Instant::now();
    match tokio::time::timeout(Duration::from_secs(20), dev_ev.recv()).await {
        Ok(Some(TunnelEvent::Disconnected { reason })) => {
            assert!(reason.contains("ping"), "{reason}")
        }
        other => panic!("expected Disconnected within 20 s, got {other:?}"),
    }
    assert!(t0.elapsed() < Duration::from_secs(20));
    assert!(dev.await.unwrap().is_ok());
}

// ---- run_device + cookie gate -------------------------------------------

/// One box session (rendezvous, punch, serve until the tunnel ends) on its
/// own runtime. Dropping it shuts that runtime down — every task and socket
/// at once, like the box process dying (no CONNECTION_CLOSE is sent).
struct BoxProc(Option<tokio::runtime::Runtime>);

impl Drop for BoxProc {
    fn drop(&mut self) {
        if let Some(rt) = self.0.take() {
            rt.shutdown_background();
        }
    }
}

fn spawn_box(cfg: &ClientConfig, s: &PairingSecret, target: SocketAddr) -> BoxProc {
    let (cfg, s) = (cfg.clone(), s.clone());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    rt.spawn(async move {
        let path = establish(&cfg, &s, Role::Box).await.unwrap();
        let _ = serve_box(path, &s, target, |_| {}).await;
    });
    BoxProc(Some(rt))
}

struct Device {
    port: u16,
    ev: mpsc::UnboundedReceiver<DeviceEvent>,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

async fn start_device(cfg: &ClientConfig, s: &PairingSecret, gate: Option<&CookieGate>) -> Device {
    let mut opts = DeviceOptions::new(PairingLink::new(s.clone(), "unused.test"));
    opts.relay = Some(cfg.clone());
    opts.min_backoff = Duration::from_millis(100);
    if let Some(g) = gate {
        opts = opts.with_gate(g);
    }
    let listener = bind_listener(ListenAddr::Ephemeral).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, ev) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let task = tokio::spawn(run_device(opts, listener, cancel.clone(), move |e| {
        let _ = tx.send(e);
    }));
    Device {
        port,
        ev,
        cancel,
        task,
    }
}

async fn wait_for(ev: &mut mpsc::UnboundedReceiver<DeviceEvent>, want: fn(&DeviceEvent) -> bool) {
    let found = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(e) = ev.recv().await {
            eprintln!("device event: {e:?}");
            if want(&e) {
                return;
            }
        }
        panic!("device loop ended");
    })
    .await;
    assert!(found.is_ok(), "device event did not arrive within 30 s");
}

/// Send `req` and read until the peer closes; a dropped (reset)
/// connection yields whatever arrived, normally nothing.
async fn raw(port: u16, req: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(T, s.read_to_end(&mut out))
        .await
        .unwrap();
    String::from_utf8_lossy(&out).into_owned()
}

/// WebSocket upgrade; the response head, or "" if the connection is dropped.
async fn ws_upgrade(port: u16, cookie: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let req = format!(
        "GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n{cookie}\r\n"
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let mut b = [0u8; 1];
    tokio::time::timeout(T, async {
        while !buf.ends_with(b"\r\n\r\n") {
            match s.read(&mut b).await {
                Ok(1) => buf.push(b[0]),
                _ => break,
            }
        }
    })
    .await
    .unwrap();
    String::from_utf8_lossy(&buf).into_owned()
}

#[tokio::test]
async fn device_loop_reconnects_after_box_restart_and_stops_on_cancel() {
    let cfg = relay().await;
    let target = echo_server().await;
    let s = PairingSecret::generate();
    let first = spawn_box(&cfg, &s, target);
    let mut d = start_device(&cfg, &s, None).await;
    wait_for(&mut d.ev, |e| matches!(e, DeviceEvent::Connected { .. })).await;
    assert!(
        http_get(d.port, "/one")
            .await
            .ends_with("echo GET /one HTTP/1.1")
    );

    // The box goes away and comes back; the loop finds it again on its own.
    drop(first);
    wait_for(&mut d.ev, |e| matches!(e, DeviceEvent::Disconnected { .. })).await;
    let _second = spawn_box(&cfg, &s, target);
    wait_for(&mut d.ev, |e| matches!(e, DeviceEvent::Connected { .. })).await;
    assert!(
        http_get(d.port, "/two")
            .await
            .ends_with("echo GET /two HTTP/1.1")
    );

    // Cancel: the loop returns and the port stops accepting.
    d.cancel.cancel();
    let r = tokio::time::timeout(T, d.task).await.unwrap().unwrap();
    assert!(r.is_ok(), "{r:?}");
    assert!(TcpStream::connect(("127.0.0.1", d.port)).await.is_err());
}

#[tokio::test]
async fn cookie_gate_admits_only_the_booted_webview() {
    let cfg = relay().await;
    let target = echo_server().await;
    let s = PairingSecret::generate();
    let _box = spawn_box(&cfg, &s, target);
    let gate = CookieGate::new();
    let mut d = start_device(&cfg, &s, Some(&gate)).await;
    wait_for(&mut d.ev, |e| matches!(e, DeviceEvent::Connected { .. })).await;
    let key = gate.key().to_string();
    let get = |path: &str, extra: &str| {
        format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{extra}\r\n")
    };

    // No cookie, a wrong one, or a wrong boot key: dropped unanswered.
    assert_eq!(raw(d.port, &get("/", "")).await, "");
    assert_eq!(raw(d.port, &get("/", "Cookie: __pbm=nope\r\n")).await, "");
    let last = if key.ends_with('0') { '1' } else { '0' };
    let wrong_key = format!("Cookie: __pbm={}{last}\r\n", &key[..key.len() - 1]);
    assert_eq!(raw(d.port, &get("/", &wrong_key)).await, "");
    assert_eq!(raw(d.port, &get("/__pbm/boot?k=nope", "")).await, "");

    // Boot: answered on the device (the box's echo never sees it).
    let r = raw(d.port, &get(&gate.boot_path(), "")).await;
    assert!(r.starts_with("HTTP/1.1 200 OK\r\n"), "{r}");
    assert!(
        r.contains(&format!(
            "\r\nSet-Cookie: __pbm={key}; HttpOnly; SameSite=Strict; Path=/\r\n"
        )),
        "{r}"
    );
    assert!(
        r.contains("location.replace(\"/\")") && !r.contains("echo"),
        "{r}"
    );
    assert!(!r.contains("__pbm_shell"), "{r}");
    // `next`: back to the page the WebView was on (same-origin paths only).
    let r = raw(d.port, &get(&gate.boot_path_to("/s/x?y=1&z=2"), "")).await;
    assert!(r.contains("location.replace(\"/s/x?y=1&z=2\")"), "{r}");
    let evil = format!("{}&next=%2F%2Fevil.example", gate.boot_path());
    let r = raw(d.port, &get(&evil, "")).await;
    assert!(
        r.contains("location.replace(\"/\")") && !r.contains("evil"),
        "{r}"
    );

    // `shell=` (older app builds): ignored, never rejected, no cookie.
    for shell in [
        "tauri%3A%2F%2Flocalhost",
        "https%3A%2F%2Fevil.example",
        "%ZZ",
    ] {
        let r = raw(
            d.port,
            &get(&format!("{}&shell={shell}", gate.boot_path()), ""),
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 200 OK\r\n"), "{r}");
        assert!(r.contains("location.replace(\"/\")"), "{r}");
        assert!(!r.contains("__pbm_shell"), "{shell}: {r}");
    }

    // With the cookie (among others, any header case): forwarded.
    let ok = format!("cookie: a=b; __pbm={key}; c=d\r\n");
    let r = raw(d.port, &get("/hello", &ok)).await;
    assert!(r.ends_with("echo GET /hello HTTP/1.1"), "{r}");

    // WebSocket upgrades: same rule.
    assert_eq!(ws_upgrade(d.port, "").await, "");
    let head = ws_upgrade(d.port, &format!("Cookie: __pbm={key}\r\n")).await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
}

/// TCP proxy to `upstream` that holds every client→relay chunk for `delay`
/// (relay→client is immediate). STUN still goes straight to the relay, so
/// the client's UDP overtakes its TLS frames — a slow uplink.
async fn slow_uplink(upstream: SocketAddr, delay: Duration) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((client, _)) = l.accept().await {
            let relay = TcpStream::connect(upstream).await.unwrap();
            let (mut cr, mut cw) = client.into_split();
            let (mut rr, mut rw) = relay.into_split();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16 * 1024];
                while let Ok(n @ 1..) = cr.read(&mut buf).await {
                    tokio::time::sleep(delay).await;
                    if rw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
            tokio::spawn(async move {
                let _ = tokio::io::copy(&mut rr, &mut cw).await;
            });
        }
    });
    addr
}

/// Regression: the relay coordinates the punch as soon as both STUN
/// endpoints are known. A peer whose candidates frame (TLS) was overtaken by
/// its STUN Binding (UDP) used to be announced without its LAN address, so
/// the other side probed only the public one — on networks where that path
/// doesn't work back, the punch went one-sided and the reconnect after an
/// app resume cost ~15 s. `establish` must get its candidates applied first.
#[tokio::test]
async fn punch_carries_candidates_despite_slow_uplink() {
    use peckboard_relay::client::{Event, RelayClient};

    let cfg = relay().await;
    let s = PairingSecret::generate();
    let mut boxc = RelayClient::connect(&cfg, &s, Role::Box).await.unwrap();
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    boxc.stun_binding(&sock).await.unwrap();

    let dev_cfg = ClientConfig {
        relay: slow_uplink(cfg.relay, Duration::from_millis(300)).await,
        ..cfg.clone()
    };
    let ds = s.clone();
    let dev = tokio::spawn(async move { establish(&dev_cfg, &ds, Role::Device).await });

    let punch = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match boxc.next_event().await {
                Some(Event::Punch(p)) => return p,
                Some(_) => {}
                None => panic!("relay closed"),
            }
        }
    })
    .await
    .expect("no punch coordinated");
    dev.abort();
    assert!(
        !punch.peer_candidates.is_empty(),
        "punch sent before the device's candidates arrived"
    );
}

/// `establish_with`: the box binds the fixed port, and its advertised
/// candidates — an explicit address and a port on the STUN-observed IP —
/// reach the device in `PunchNow` next to the LAN candidate.
#[tokio::test]
async fn box_fixed_port_and_advertised_candidates() {
    use peckboard_relay::client::{Event, RelayClient};
    use peckboard_relay::tunnel::{Advertise, EstablishOptions, Registration, establish_with};
    use std::sync::{Arc, Mutex};

    let cfg = relay().await;
    let s = PairingSecret::generate();
    let fixed = UdpSocket::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let explicit: SocketAddr = "198.51.100.9:4000".parse().unwrap();
    let seen: Arc<Mutex<Option<Registration>>> = Arc::default();
    let (reg_tx, mut reg_rx) = mpsc::unbounded_channel();
    let opts = EstablishOptions {
        bind_port: Some(fixed),
        advertise: vec![Advertise::Port(fixed), Advertise::Addr(explicit)],
        on_registered: Some({
            let seen = seen.clone();
            Arc::new(move |r: &Registration| {
                *seen.lock().unwrap() = Some(r.clone());
                let _ = reg_tx.send(());
            })
        }),
        ..Default::default()
    };
    let (bcfg, bs) = (cfg.clone(), s.clone());
    let boxed = tokio::spawn(async move { establish_with(&bcfg, &bs, Role::Box, &opts).await });
    tokio::time::timeout(T, reg_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let reg = seen.lock().unwrap().clone().unwrap();
    assert_eq!(reg.local_port, fixed);
    let forwarded = SocketAddr::new(reg.public.ip(), fixed);
    assert!(reg.candidates.contains(&forwarded), "{reg:?}");
    assert!(reg.candidates.contains(&explicit), "{reg:?}");

    let mut dev = RelayClient::connect(&cfg, &s, Role::Device).await.unwrap();
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    dev.stun_binding(&udp).await.unwrap();
    let punch = tokio::time::timeout(T, async {
        loop {
            if let Some(Event::Punch(p)) = dev.next_event().await {
                return p;
            }
        }
    })
    .await
    .unwrap();
    for want in [forwarded, explicit] {
        assert!(punch.peer_candidates.contains(&want), "{punch:?}");
    }
    dev.punch(&udp, &punch, T).await.unwrap();
    let path = tokio::time::timeout(T, boxed)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(path.socket.local_addr().unwrap().port(), fixed);
}

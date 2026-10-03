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
    PunchedPath, STREAM_PING, STREAM_TCP, TunnelEvent, connect_device, connect_raw, establish,
    serve_box,
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
            socket: a,
            peer: ba,
            role: Role::Box,
        },
        PunchedPath {
            socket: b,
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
            socket: a,
            peer: pa,
            role: Role::Box,
        },
        PunchedPath {
            socket: b,
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

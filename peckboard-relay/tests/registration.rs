//! Registration gate: box identities, the relay's registry, the
//! `/register` HTTP endpoints, and the gate on the relay data channel.
//!
//! Symmetric-NAT model as in `relay_fallback.rs` (each peer's STUN goes
//! through a shim on its own loopback address). Linux-only: binds
//! 127.0.0.2/3.

#![cfg(target_os = "linux")]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use peckboard_relay::client::{
    ClientConfig, Event, IdentityStatus, RelayClient, RelayData, registration_status,
};
use peckboard_relay::identity::BoxIdentity;
use peckboard_relay::keys::{EXPORTER_LABEL, EXPORTER_LEN, PairingSecret};
use peckboard_relay::proto::{
    ALPN, ALPN_HTTP1, ALPN_V2, ClientMsg, Role, ServerMsg, read_frame, write_frame,
};
use peckboard_relay::registry::Registry;
use peckboard_relay::server::http::pow_solve;
use peckboard_relay::server::{Relay, RelayConfig};
use peckboard_relay::tls;
use peckboard_relay::tunnel::{
    EstablishOptions, PathKind, RelayFallback, TunnelError, TunnelEvent, connect_device,
    establish_with, registration_url, serve_box,
};
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio_rustls::{TlsAcceptor, TlsConnector};

const NAME: &str = "relay.test";
const T: Duration = Duration::from_secs(30);

struct Harness {
    relay: Relay,
    cfg: ClientConfig,
    stun: SocketAddr,
    cert: CertificateDer<'static>,
}

fn tmp_dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("peckrelay-it-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// In-process relay with `registry`. `old`: a relay that predates box
/// identities (no protocol v3).
async fn start(rc: RelayConfig, registry: Arc<Registry>, old: bool) -> Harness {
    let rc = RelayConfig {
        auth_delay: Duration::from_millis(20),
        punch_lead: Duration::from_millis(50),
        registration_pow_bits: 8,
        http_burst_per_ip: 100.0,
        ..rc
    };
    let (server_cfg, cert) = tls::self_signed(&[NAME.to_string()]).unwrap();
    let server_cfg = if old {
        tls::without_v3(&server_cfg)
    } else {
        server_cfg
    };
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let stun = udp.local_addr().unwrap();
    let relay = Relay::with_registry(rc, stun.port(), registry);
    let r = relay.clone();
    tokio::spawn(async move { r.serve_tls(tcp, TlsAcceptor::from(server_cfg)).await });
    let r = relay.clone();
    tokio::spawn(async move { r.serve_stun(udp).await });
    Harness {
        relay,
        cfg: ClientConfig::pinned(addr, NAME, cert.clone()).unwrap(),
        stun,
        cert,
    }
}

fn gated() -> RelayConfig {
    RelayConfig {
        registration_gate: true,
        ..RelayConfig::default()
    }
}

/// See `relay_fallback.rs`: a symmetric NAT in front of one peer's STUN.
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

fn opts(identity: Option<&BoxIdentity>, lan: bool) -> EstablishOptions {
    EstablishOptions {
        fallback: RelayFallback {
            after_failures: 1,
            upgrade_every: None,
            ..RelayFallback::default()
        },
        no_lan_candidate: !lan,
        identity: identity.cloned(),
        ..EstablishOptions::default()
    }
}

/// Box (with `identity`) and device both online on one pairing; returns
/// their data channels.
async fn pair(
    h: &Harness,
    identity: Option<&BoxIdentity>,
) -> (RelayClient, RelayClient, RelayData, RelayData) {
    let s = PairingSecret::generate();
    let mut b = RelayClient::connect_with_identity(&h.cfg, &s, Role::Box, identity)
        .await
        .unwrap();
    let mut d = RelayClient::connect(&h.cfg, &s, Role::Device)
        .await
        .unwrap();
    for c in [&mut b, &mut d] {
        tokio::time::timeout(T, async {
            while !matches!(c.next_event().await, Some(Event::PeerOnline)) {}
        })
        .await
        .unwrap();
    }
    let (bd, dd) = (b.take_data().unwrap(), d.take_data().unwrap());
    (b, d, bd, dd)
}

/// Does a datagram the box sends reach the device?
async fn box_to_device(bd: &RelayData, dd: &mut RelayData) -> bool {
    for _ in 0..5 {
        bd.tx.send(vec![0x40; 100]).await.unwrap();
        if let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(300), dd.rx.recv()).await {
            return true;
        }
    }
    false
}

// ---- echo target for full tunnels ---------------------------------------

async fn echo_server() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let body = format!(
                    "echo {}",
                    String::from_utf8_lossy(&buf[..n])
                        .lines()
                        .next()
                        .unwrap_or("")
                );
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    addr
}

async fn get_via(port: u16, path: &str) -> String {
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

/// Full establish + serve on both sides; asserts the path kind and that a
/// request crosses the tunnel.
async fn tunnel_works(
    bcfg: ClientConfig,
    dcfg: ClientConfig,
    bopts: EstablishOptions,
    want: PathKind,
) {
    let target = echo_server().await;
    let s = PairingSecret::generate();
    let (bs, ds) = (s.clone(), s);
    let dopts = opts(None, !bopts.no_lan_candidate);
    let boxed = tokio::spawn(async move {
        establish_with(&bcfg, &bs, Role::Box, &bopts)
            .await
            .map(|p| (p, bs))
    });
    let dev = tokio::time::timeout(T, establish_with(&dcfg, &ds, Role::Device, &dopts))
        .await
        .unwrap()
        .unwrap();
    let (bpath, bs) = tokio::time::timeout(T, boxed)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!((bpath.kind(), dev.kind()), (want, want));
    tokio::spawn(async move { serve_box(bpath, &bs, target, |_| {}).await });
    let listen = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listen.local_addr().unwrap().port();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        connect_device(dev, &ds, &listen, move |ev| {
            let _ = tx.send(ev);
        })
        .await
    });
    tokio::time::timeout(T, async {
        while !matches!(rx.recv().await, Some(TunnelEvent::Connected { .. })) {}
    })
    .await
    .unwrap();
    assert!(get_via(port, "/x").await.ends_with("echo GET /x HTTP/1.1"));
}

// ---- HTTP over the TLS listener ------------------------------------------

async fn http(h: &Harness, method: &str, target: &str, body: &str) -> (u16, String) {
    http_from(h, None, method, target, body).await
}

/// [`http`] from loopback address `src` (default 127.0.0.1).
async fn http_from(
    h: &Harness,
    src: Option<Ipv4Addr>,
    method: &str,
    target: &str,
    body: &str,
) -> (u16, String) {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(h.cert.clone()).unwrap();
    let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    cfg.alpn_protocols = vec![b"h2".to_vec(), ALPN_HTTP1.to_vec()];
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.bind(SocketAddr::new(
        IpAddr::V4(src.unwrap_or(Ipv4Addr::LOCALHOST)),
        0,
    ))
    .unwrap();
    let tcp = sock.connect(h.cfg.relay).await.unwrap();
    let mut tls = TlsConnector::from(Arc::new(cfg))
        .connect(ServerName::try_from(NAME).unwrap(), tcp)
        .await
        .unwrap();
    let req = format!(
        "{method} {target} HTTP/1.1\r\nHost: {NAME}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    tls.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    tokio::time::timeout(T, tls.read_to_string(&mut out))
        .await
        .unwrap()
        .unwrap();
    let (head, body) = out.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, body.to_string())
}

fn json_str<'a>(body: &'a str, field: &str) -> &'a str {
    let pat = format!("\"{field}\":\"");
    let rest = &body[body.find(&pat).unwrap() + pat.len()..];
    &rest[..rest.find('"').unwrap()]
}

/// Challenge → proof of work → `POST /api/register` for `key`, from `src`.
async fn register_from(h: &Harness, src: Option<Ipv4Addr>, key: &str) -> (u16, String) {
    let (_, ch) = http_from(h, src, "GET", "/api/register/challenge", "").await;
    let nonce = json_str(&ch, "nonce").to_string();
    let form = format!(
        "key={key}&nonce={nonce}&solution={}",
        pow_solve(&nonce, key, 8)
    );
    http_from(h, src, "POST", "/api/register", &form).await
}

// ---- tests -------------------------------------------------------------------

/// The registration page and API on the signaling listener: a browser gets
/// the page; challenge → proof of work → registered (persisted), single-use
/// nonces, bad proofs refused, everything else 404. Peers on the same port
/// are unaffected.
#[tokio::test]
async fn register_over_https() {
    let dir = tmp_dir();
    let reg = Arc::new(Registry::open_in(&dir).unwrap());
    let h = start(RelayConfig::default(), reg.clone(), false).await;
    let id = BoxIdentity::generate();
    let key = id.public_key_b64();
    assert!(
        registration_url("relay.peckboard.com", &id.public_key())
            .starts_with("https://relay.peckboard.com/register#")
    );

    let (st, page) = http(&h, "GET", "/register", "").await;
    assert_eq!(st, 200);
    assert!(page.contains("<title>Register your PeckBoard box</title>"));
    assert!(!registration_status(&h.cfg, &id.public_key()).await.unwrap());
    let (st, body) = http(&h, "GET", &format!("/api/registered?key={key}"), "").await;
    assert_eq!((st, body.as_str()), (200, "{\"registered\":false}"));

    // A bad proof burns the nonce.
    let (_, ch) = http(&h, "GET", "/api/register/challenge", "").await;
    let nonce = json_str(&ch, "nonce").to_string();
    assert!(ch.contains("\"difficulty\":8"));
    let bad = (0u64..)
        .map(|n| n.to_string())
        .find(|s| !peckboard_relay::server::http::pow_ok(&nonce, &key, s, 8))
        .unwrap();
    let form = format!("key={key}&nonce={nonce}&solution={bad}");
    assert_eq!(http(&h, "POST", "/api/register", &form).await.0, 400);
    let good = pow_solve(&nonce, &key, 8);
    let form = format!("key={key}&nonce={nonce}&solution={good}");
    let (st, body) = http(&h, "POST", "/api/register", &form).await;
    assert_eq!(st, 400, "reused nonce: {body}");

    // A fresh challenge, solved: registered, persisted, idempotent.
    let (_, ch) = http(&h, "GET", "/api/register/challenge", "").await;
    let nonce = json_str(&ch, "nonce").to_string();
    let form = format!(
        "key={key}&nonce={nonce}&solution={}",
        pow_solve(&nonce, &key, 8)
    );
    let (st, body) = http(&h, "POST", "/api/register", &form).await;
    assert_eq!(
        (st, body.as_str()),
        (200, "{\"registered\":true,\"new\":true}")
    );
    assert!(registration_status(&h.cfg, &id.public_key()).await.unwrap());
    assert!(Registry::open_in(&dir).unwrap().contains(&id.public_key()));
    let (_, ch) = http(&h, "GET", "/api/register/challenge", "").await;
    let nonce = json_str(&ch, "nonce").to_string();
    let form = format!(
        "key={key}&nonce={nonce}&solution={}",
        pow_solve(&nonce, &key, 8)
    );
    let (st, body) = http(&h, "POST", "/api/register", &form).await;
    assert_eq!(
        (st, body.as_str()),
        (200, "{\"registered\":true,\"new\":false}")
    );

    // Garbage key, unknown routes.
    assert_eq!(http(&h, "GET", "/api/registered?key=xyz", "").await.0, 400);
    assert_eq!(http(&h, "GET", "/", "").await.0, 404);
    assert_eq!(http(&h, "GET", "/api/register", "").await.0, 404);
    assert_eq!(http(&h, "POST", "/register", "").await.0, 404);

    // Signaling on the same port still works.
    let c = RelayClient::connect(&h.cfg, &PairingSecret::generate(), Role::Device)
        .await
        .unwrap();
    assert_eq!(c.protocol_version(), 3);
    let _ = std::fs::remove_dir_all(dir);
}
/// The registry can't be grown without bound from the page: new keys are
/// capped per address per day and in total (re-registering is free);
/// challenges only work from the address they were issued to; the status
/// check takes a JSON POST.
#[tokio::test]
async fn registration_is_capped_and_challenges_are_bound_to_the_address() {
    let h = start(
        RelayConfig {
            registration_max_keys: 3,
            registration_per_ip_per_day: 2.0,
            ..RelayConfig::default()
        },
        Arc::new(Registry::in_memory()),
        false,
    )
    .await;
    let keys: Vec<String> = (0..4)
        .map(|_| BoxIdentity::generate().public_key_b64())
        .collect();
    let other = Some(Ipv4Addr::new(127, 0, 0, 2));

    // Per-address daily cap.
    assert_eq!(register_from(&h, None, &keys[0]).await.0, 200);
    assert_eq!(register_from(&h, None, &keys[1]).await.0, 200);
    let (st, body) = register_from(&h, None, &keys[2]).await;
    assert_eq!(st, 429, "{body}");
    let (st, body) = register_from(&h, None, &keys[0]).await;
    assert_eq!(
        (st, body.as_str()),
        (200, "{\"registered\":true,\"new\":false}")
    );

    // A challenge is good only from the address it was issued to.
    let (_, ch) = http(&h, "GET", "/api/register/challenge", "").await;
    let nonce = json_str(&ch, "nonce").to_string();
    let form = format!(
        "key={}&nonce={nonce}&solution={}",
        keys[2],
        pow_solve(&nonce, &keys[2], 8)
    );
    let (st, body) = http_from(&h, other, "POST", "/api/register", &form).await;
    assert_eq!(st, 400, "{body}");

    // Total cap: another address fills the last slot, then it's full.
    assert_eq!(register_from(&h, other, &keys[2]).await.0, 200);
    let (st, body) = register_from(&h, other, &keys[3]).await;
    assert_eq!(st, 503, "{body}");
    assert_eq!(h.relay.registry().len(), 3);

    // Status over POST (JSON body), no key in the request line.
    let status = |k: &str| format!("{{\"key\":\"{k}\"}}");
    let (st, body) = http(&h, "POST", "/api/registered", &status(&keys[2])).await;
    assert_eq!((st, body.as_str()), (200, "{\"registered\":true}"));
    let (st, body) = http(&h, "POST", "/api/registered", &status(&keys[3])).await;
    assert_eq!((st, body.as_str()), (200, "{\"registered\":false}"));
    assert_eq!(
        http(&h, "POST", "/api/registered", "{\"key\":\"xyz\"}")
            .await
            .0,
        400
    );
}

/// Gate off (the default): an unregistered box relays as before, and the
/// relay says so.
#[tokio::test]
async fn gate_off_unregistered_box_still_relays() {
    let h = start(
        RelayConfig::default(),
        Arc::new(Registry::in_memory()),
        false,
    )
    .await;
    let id = BoxIdentity::generate();
    let (b, d, bd, mut dd) = pair(&h, Some(&id)).await;
    assert_eq!(
        b.identity_status(),
        Some(IdentityStatus {
            registered: false,
            gated: false
        })
    );
    assert!(b.identity_status().unwrap().relay_permitted());
    assert_eq!(d.identity_status().map(|s| s.registered), Some(false));
    assert!(box_to_device(&bd, &mut dd).await);

    // Full tunnel over symmetric NATs: relayed.
    let bcfg = behind_nat(&h, Ipv4Addr::new(127, 0, 0, 2)).await;
    let dcfg = behind_nat(&h, Ipv4Addr::new(127, 0, 0, 3)).await;
    tunnel_works(bcfg, dcfg, opts(Some(&id), false), PathKind::Relayed).await;
}

/// Gate on: an unregistered box gets rendezvous, forwarding and a direct
/// tunnel, but no relayed data; once registered it relays; revoked (by an
/// external edit of the registry file) it is refused again after reload.
#[tokio::test]
async fn gate_on_requires_registered_box_for_relay_data() {
    let dir = tmp_dir();
    let reg = Arc::new(
        Registry::open_in(&dir)
            .unwrap()
            .with_reload_interval(Duration::from_millis(50)),
    );
    let h = start(gated(), reg.clone(), false).await;
    let id = BoxIdentity::generate();

    // Unregistered: signaling works, data doesn't.
    let (b, mut d, bd, mut dd) = pair(&h, Some(&id)).await;
    assert_eq!(
        b.identity_status(),
        Some(IdentityStatus {
            registered: false,
            gated: true
        })
    );
    assert!(!b.identity_status().unwrap().relay_permitted());
    b.send(b"hello").await.unwrap();
    let got = tokio::time::timeout(T, async {
        loop {
            if let Some(Event::Message { plaintext, .. }) = d.next_event().await {
                return plaintext;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(got, b"hello");
    assert!(!box_to_device(&bd, &mut dd).await);
    let st = h.relay.relay_stats();
    assert_eq!((st.packets, st.active_pairs), (0, 0), "{st:?}");
    assert!(st.dropped_limit > 0);
    drop((b, d, bd, dd));

    // Direct still works for the unregistered box.
    tunnel_works(
        h.cfg.clone(),
        h.cfg.clone(),
        opts(Some(&id), true),
        PathKind::Direct,
    )
    .await;

    // Registered: the relay says so, and data flows.
    assert!(reg.add(id.public_key()).unwrap());
    let (b, _d, bd, mut dd) = pair(&h, Some(&id)).await;
    assert_eq!(b.identity_status().map(|s| s.registered), Some(true));
    assert!(box_to_device(&bd, &mut dd).await);

    // Revoked by another process (the admin CLI): denied after reload,
    // even on the live session.
    assert!(
        Registry::open_in(&dir)
            .unwrap()
            .revoke(&id.public_key())
            .unwrap()
    );
    tokio::time::sleep(Duration::from_millis(200)).await; // > reload interval
    while dd.rx.try_recv().is_ok() {}
    assert!(!box_to_device(&bd, &mut dd).await);
    assert_eq!(h.relay.relay_stats().active_pairs, 0);
    let _ = std::fs::remove_dir_all(dir);
}

/// Gate on, symmetric NATs, unregistered box: the box doesn't offer the
/// relay fallback, so both sides fail the punch (instead of a dead relayed
/// tunnel). Registered, the same setup relays.
#[tokio::test]
async fn gate_on_tunnel_relays_only_for_registered_box() {
    let reg = Arc::new(Registry::in_memory());
    let h = start(gated(), reg.clone(), false).await;
    let id = BoxIdentity::generate();
    let bcfg = behind_nat(&h, Ipv4Addr::new(127, 0, 0, 2)).await;
    let dcfg = behind_nat(&h, Ipv4Addr::new(127, 0, 0, 3)).await;

    let s = PairingSecret::generate();
    let (bs, bc, bo) = (s.clone(), bcfg.clone(), opts(Some(&id), false));
    let boxed = tokio::spawn(async move { establish_with(&bc, &bs, Role::Box, &bo).await });
    let dev = tokio::time::timeout(
        Duration::from_secs(90),
        establish_with(&dcfg, &s, Role::Device, &opts(None, false)),
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

    reg.add(id.public_key()).unwrap();
    tunnel_works(bcfg, dcfg, opts(Some(&id), false), PathKind::Relayed).await;
}

/// Old clients (Peckboard 0.1.59–0.1.62 speak v2 and present no identity):
/// still admitted for signaling on a gated relay — no identity frames ever
/// reach them — but never relayed for. And a v1-only client likewise.
#[tokio::test]
async fn legacy_clients_still_admitted() {
    let h = start(gated(), Arc::new(Registry::in_memory()), false).await;
    let s = PairingSecret::generate();
    let keys = s.derive();

    // Raw v2 client, as shipped before v3.
    let mut roots = rustls::RootCertStore::empty();
    roots.add(h.cert.clone()).unwrap();
    let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    cfg.alpn_protocols = vec![ALPN_V2.to_vec(), ALPN.to_vec()];
    let tcp = TcpStream::connect(h.cfg.relay).await.unwrap();
    let tls = TlsConnector::from(Arc::new(cfg))
        .connect(ServerName::try_from(NAME).unwrap(), tcp)
        .await
        .unwrap();
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(ALPN_V2));
    let mut ex = [0u8; EXPORTER_LEN];
    tls.get_ref()
        .1
        .export_keying_material(&mut ex, EXPORTER_LABEL, None)
        .unwrap();
    let (mut rd, mut wr) = tokio::io::split(tls);
    let hello = ClientMsg::Hello {
        role: Role::Box,
        rendezvous_id: keys.rendezvous_id,
        public_key: keys.public_key(),
    };
    write_frame(&mut wr, &hello.encode()).await.unwrap();
    let ServerMsg::Challenge { nonce } =
        ServerMsg::decode(&read_frame(&mut rd).await.unwrap()).unwrap()
    else {
        panic!("expected challenge");
    };
    let signature = keys.sign_challenge(&nonce, Role::Box, &ex);
    write_frame(&mut wr, &ClientMsg::Auth { signature }.encode())
        .await
        .unwrap();
    let reg = ServerMsg::decode(&read_frame(&mut rd).await.unwrap()).unwrap();
    assert!(matches!(reg, ServerMsg::Registered { .. }), "{reg:?}");

    // Its device (current client) pairs with it: the old box sees exactly
    // the frames it knows (no IdentityStatus).
    let mut dev = RelayClient::connect(&h.cfg, &s, Role::Device)
        .await
        .unwrap();
    let next = ServerMsg::decode(&read_frame(&mut rd).await.unwrap()).unwrap();
    assert_eq!(next, ServerMsg::PeerOnline);
    tokio::time::timeout(T, async {
        while !matches!(dev.next_event().await, Some(Event::PeerOnline)) {}
    })
    .await
    .unwrap();
    // Its relayed data is refused (unregistered by definition).
    write_frame(
        &mut wr,
        &ClientMsg::Data {
            packet: vec![1; 50],
        }
        .encode(),
    )
    .await
    .unwrap();
    write_frame(&mut wr, &ClientMsg::Ping.encode())
        .await
        .unwrap();
    let pong = ServerMsg::decode(&read_frame(&mut rd).await.unwrap()).unwrap();
    assert_eq!(pong, ServerMsg::Pong);
    assert_eq!(h.relay.relay_stats().packets, 0);

    // v1-only client: admitted, no identity status.
    let old = ClientConfig {
        v1_only: true,
        ..h.cfg.clone()
    };
    let c = RelayClient::connect(&old, &PairingSecret::generate(), Role::Box)
        .await
        .unwrap();
    assert_eq!((c.protocol_version(), c.identity_status()), (1, None));
}

/// New box (with identity) against a relay that predates v3: negotiates v2,
/// no identity sent or reported, relaying as before.
#[tokio::test]
async fn new_box_on_pre_v3_relay() {
    let h = start(
        RelayConfig::default(),
        Arc::new(Registry::in_memory()),
        true,
    )
    .await;
    let id = BoxIdentity::generate();
    let (b, _d, bd, mut dd) = pair(&h, Some(&id)).await;
    assert_eq!((b.protocol_version(), b.identity_status()), (2, None));
    assert!(box_to_device(&bd, &mut dd).await);
}

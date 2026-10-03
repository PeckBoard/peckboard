//! In-process relay + clients: rendezvous, E2E forwarding, indistinguishable
//! failures, STUN gating, rate limits, loopback hole punch.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use peckboard_relay::client::{ClientConfig, Event, RelayClient};
use peckboard_relay::keys::{EXPORTER_LABEL, EXPORTER_LEN, PairingSecret};
use peckboard_relay::proto::{ALPN, ClientMsg, Role, ServerMsg, read_frame, write_frame};
use peckboard_relay::server::{Relay, RelayConfig};
use peckboard_relay::{stun, tls};
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio_rustls::{TlsAcceptor, TlsConnector};

const NAME: &str = "relay.test";
const AUTH_DELAY: Duration = Duration::from_millis(120);

struct Harness {
    relay: Relay,
    cfg: ClientConfig,
    cert: CertificateDer<'static>,
    stun_addr: SocketAddr,
}

async fn start(mut rc: RelayConfig) -> Harness {
    rc.auth_delay = AUTH_DELAY;
    rc.punch_lead = Duration::from_millis(50);
    let (server_cfg, cert) = tls::self_signed(&[NAME.to_string()]).unwrap();
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let stun_addr = udp.local_addr().unwrap();
    let relay = Relay::new(rc, stun_addr.port());
    let r = relay.clone();
    tokio::spawn(async move { r.serve_tls(tcp, TlsAcceptor::from(server_cfg)).await });
    let r = relay.clone();
    tokio::spawn(async move { r.serve_stun(udp).await });
    let cfg = ClientConfig::pinned(addr, NAME, cert.clone()).unwrap();
    Harness {
        relay,
        cfg,
        cert,
        stun_addr,
    }
}

async fn next(c: &mut RelayClient, want: impl Fn(&Event) -> bool) -> Event {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let ev = c.next_event().await.expect("relay closed");
            if want(&ev) {
                return ev;
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

#[tokio::test]
async fn rendezvous_and_e2e_messages() {
    let h = start(RelayConfig::default()).await;
    let s = PairingSecret::generate();
    let mut boxc = RelayClient::connect(&h.cfg, &s, Role::Box).await.unwrap();
    let mut dev = RelayClient::connect(&h.cfg, &s, Role::Device)
        .await
        .unwrap();
    next(&mut boxc, |e| matches!(e, Event::PeerOnline)).await;
    next(&mut dev, |e| matches!(e, Event::PeerOnline)).await;

    let secret_text = b"offer: wg-pubkey=PLAINTEXT-MARKER-1234567890";
    boxc.send(secret_text).await.unwrap();
    let Event::Message { plaintext, sealed } =
        next(&mut dev, |e| matches!(e, Event::Message { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(plaintext, secret_text);
    // What the relay forwarded is ciphertext: right size (nonce + tag +
    // counter/timestamp header), no plaintext run.
    assert_eq!(sealed.len(), secret_text.len() + 40 + 16);
    assert!(
        !sealed
            .windows(8)
            .any(|w| secret_text.windows(8).any(|p| p == w))
    );

    dev.send(b"answer").await.unwrap();
    let Event::Message { plaintext, .. } =
        next(&mut boxc, |e| matches!(e, Event::Message { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(plaintext, b"answer");

    drop(dev);
    next(&mut boxc, |e| matches!(e, Event::PeerOffline)).await;
    assert_eq!(h.relay.id_count(), 1);
}

/// Drive the protocol by hand. `auth` gets (nonce, exporter) and returns the
/// Auth frame. Returns the server message kinds seen in the first ~600 ms
/// (after one Ping) and the Auth→Registered latency.
async fn raw_session(
    h: &Harness,
    hello: Vec<u8>,
    auth: impl FnOnce([u8; 32], [u8; EXPORTER_LEN]) -> Vec<u8>,
) -> (Vec<&'static str>, Duration) {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(h.cert.clone()).unwrap();
    let mut cc = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    cc.alpn_protocols = vec![ALPN.to_vec()];
    let tcp = TcpStream::connect(h.cfg.relay).await.unwrap();
    let tls = TlsConnector::from(Arc::new(cc))
        .connect(ServerName::try_from(NAME).unwrap(), tcp)
        .await
        .unwrap();
    let mut ex = [0u8; EXPORTER_LEN];
    tls.get_ref()
        .1
        .export_keying_material(&mut ex, EXPORTER_LABEL, None)
        .unwrap();
    let (mut rd, mut wr) = tokio::io::split(tls);
    write_frame(&mut wr, &hello).await.unwrap();
    let ServerMsg::Challenge { nonce } =
        ServerMsg::decode(&read_frame(&mut rd).await.unwrap()).unwrap()
    else {
        panic!("no challenge")
    };
    let t0 = Instant::now();
    write_frame(&mut wr, &auth(nonce, ex)).await.unwrap();
    let first = ServerMsg::decode(&read_frame(&mut rd).await.unwrap()).unwrap();
    let latency = t0.elapsed();
    let mut kinds = vec![kind(&first)];
    write_frame(&mut wr, &ClientMsg::Ping.encode())
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(600);
    while let Ok(Ok(f)) = tokio::time::timeout_at(deadline, read_frame(&mut rd)).await {
        kinds.push(kind(&ServerMsg::decode(&f).unwrap()));
    }
    (kinds, latency)
}

fn kind(m: &ServerMsg) -> &'static str {
    match m {
        ServerMsg::Challenge { .. } => "challenge",
        ServerMsg::Registered { .. } => "registered",
        ServerMsg::Pong => "pong",
        ServerMsg::PeerOnline => "peer-online",
        ServerMsg::PeerOffline => "peer-offline",
        ServerMsg::Forwarded { .. } => "forwarded",
        ServerMsg::PunchNow { .. } => "punch",
    }
}

#[tokio::test]
async fn wrong_secret_is_indistinguishable_from_no_peer() {
    let h = start(RelayConfig::default()).await;
    let s = PairingSecret::generate();
    let mut boxc = RelayClient::connect(&h.cfg, &s, Role::Box).await.unwrap();
    let box_keys = s.derive();

    // Baseline: a legitimate device whose box is offline.
    let lonely = PairingSecret::generate().derive();
    let hello = ClientMsg::Hello {
        role: Role::Device,
        rendezvous_id: lonely.rendezvous_id,
        public_key: lonely.public_key(),
    };
    let baseline = raw_session(&h, hello.encode(), |n, ex| {
        ClientMsg::Auth {
            signature: lonely.sign_challenge(&n, Role::Device, &ex),
        }
        .encode()
    })
    .await;

    // Attacker 1: wrong secret entirely (just another lonely id — the relay
    // can't tell), via the real client.
    let mut wrong = RelayClient::connect(&h.cfg, &PairingSecret::generate(), Role::Device)
        .await
        .unwrap();

    // Attacker 2: knows the box's rendezvous id but not S — signs with its
    // own key (public key mismatch).
    let atk = PairingSecret::generate().derive();
    let hello2 = ClientMsg::Hello {
        role: Role::Device,
        rendezvous_id: box_keys.rendezvous_id,
        public_key: atk.public_key(),
    };
    let a2 = raw_session(&h, hello2.encode(), |n, ex| {
        // Valid signature by the attacker's key over the box's id.
        use ed25519_dalek::Signer;
        let msg =
            peckboard_relay::keys::auth_message(&n, &box_keys.rendezvous_id, Role::Device, &ex);
        ClientMsg::Auth {
            signature: atk.signing.sign(&msg).to_bytes(),
        }
        .encode()
    })
    .await;

    // Attacker 3: right id + box public key, garbage signature.
    let hello3 = ClientMsg::Hello {
        role: Role::Device,
        rendezvous_id: box_keys.rendezvous_id,
        public_key: box_keys.public_key(),
    };
    let a3 = raw_session(&h, hello3.encode(), |_, _| {
        ClientMsg::Auth { signature: [7; 64] }.encode()
    })
    .await;

    // Attacker 4: malformed Hello and Auth.
    let a4 = raw_session(&h, vec![0x01, 0xff, 1, 2, 3], |_, _| vec![0x99; 3]).await;

    let expected = vec!["registered", "pong"];
    for (name, (kinds, lat)) in [
        ("baseline", &baseline),
        ("a2", &a2),
        ("a3", &a3),
        ("a4", &a4),
    ] {
        assert_eq!(kinds, &expected, "{name}");
        assert!(*lat >= AUTH_DELAY, "{name} answered early: {lat:?}");
        assert!(
            *lat < AUTH_DELAY + Duration::from_millis(80),
            "{name} slow: {lat:?}"
        );
    }
    // The box never hears about any of them; the wrong-secret client sees no peer.
    wrong.ping().await.unwrap();
    assert!(matches!(next(&mut wrong, |_| true).await, Event::Pong));
    boxc.ping().await.unwrap();
    assert!(matches!(next(&mut boxc, |_| true).await, Event::Pong));
}

#[tokio::test]
async fn unauthenticated_stun_gets_no_reply() {
    let h = start(RelayConfig::default()).await;
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let txid = [9u8; 12];
    let mut plain = vec![0, 1, 0, 0, 0x21, 0x12, 0xA4, 0x42];
    plain.extend_from_slice(&txid);
    let forged = stun::build_request(&txid, "deadbeefdeadbeefdeadbeefdeadbeef", b"guess");
    let mut buf = [0u8; 600];
    for pkt in [plain, forged, b"GET / HTTP/1.1\r\n\r\n".to_vec()] {
        sock.send_to(&pkt, h.stun_addr).await.unwrap();
        let r = tokio::time::timeout(Duration::from_millis(300), sock.recv_from(&mut buf)).await;
        assert!(r.is_err(), "relay answered unauthenticated STUN");
    }
    // A credential issued over TLS works — and a wrong password for that
    // real username still gets nothing.
    let c = RelayClient::connect(&h.cfg, &PairingSecret::generate(), Role::Box)
        .await
        .unwrap();
    let cred = c.stun_credential();
    let bad = stun::build_request(&txid, &cred.username, b"wrong");
    sock.send_to(&bad, h.stun_addr).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), sock.recv_from(&mut buf))
            .await
            .is_err()
    );
    let mapped = c.stun_binding(&sock).await.unwrap();
    assert_eq!(mapped, sock.local_addr().unwrap());
}

#[tokio::test]
async fn per_ip_connection_rate_limit() {
    let rc = RelayConfig {
        conn_rate_per_ip: 0.0,
        conn_burst_per_ip: 3.0,
        ..RelayConfig::default()
    };
    let h = start(rc).await;
    let mut ok = Vec::new();
    for _ in 0..3 {
        ok.push(
            RelayClient::connect(&h.cfg, &PairingSecret::generate(), Role::Box)
                .await
                .expect("within burst"),
        );
    }
    assert!(
        RelayClient::connect(&h.cfg, &PairingSecret::generate(), Role::Box)
            .await
            .is_err(),
        "4th connection should be refused"
    );
}

#[tokio::test]
async fn wrong_alpn_is_refused() {
    let h = start(RelayConfig::default()).await;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(h.cert.clone()).unwrap();
    let mut cc = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    cc.alpn_protocols = vec![b"http/1.1".to_vec()];
    let tcp = TcpStream::connect(h.cfg.relay).await.unwrap();
    let r = TlsConnector::from(Arc::new(cc))
        .connect(ServerName::try_from(NAME).unwrap(), tcp)
        .await;
    assert!(r.is_err(), "HTTP ALPN must fail the TLS handshake");
}

#[tokio::test]
async fn loopback_hole_punch() {
    let h = start(RelayConfig::default()).await;
    let s = PairingSecret::generate();
    let mut boxc = RelayClient::connect(&h.cfg, &s, Role::Box).await.unwrap();
    let mut dev = RelayClient::connect(&h.cfg, &s, Role::Device)
        .await
        .unwrap();
    let ub = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ud = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    boxc.set_candidates(&[ub.local_addr().unwrap()])
        .await
        .unwrap();
    dev.set_candidates(&[ud.local_addr().unwrap()])
        .await
        .unwrap();
    boxc.stun_binding(&ub).await.unwrap();
    dev.stun_binding(&ud).await.unwrap();

    let Event::Punch(pb) = next(&mut boxc, |e| matches!(e, Event::Punch(_))).await else {
        unreachable!()
    };
    let Event::Punch(pd) = next(&mut dev, |e| matches!(e, Event::Punch(_))).await else {
        unreachable!()
    };
    assert_eq!(pb.nonce, pd.nonce);
    assert_eq!(pb.peer_public, ud.local_addr().unwrap());
    assert_eq!(pb.peer_candidates, vec![ud.local_addr().unwrap()]);
    let t = Duration::from_secs(3);
    let (rb, rd) = tokio::join!(boxc.punch(&ub, &pb, t), dev.punch(&ud, &pd, t));
    assert_eq!(rb.unwrap(), ud.local_addr().unwrap());
    assert_eq!(rd.unwrap(), ub.local_addr().unwrap());

    // Retry: another round on request, after the minimum spacing.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    boxc.request_punch().await.unwrap();
    let Event::Punch(p2) = next(&mut boxc, |e| matches!(e, Event::Punch(_))).await else {
        unreachable!()
    };
    assert_eq!(p2.attempt, pb.attempt + 1);
    assert_ne!(p2.nonce, pb.nonce);
}

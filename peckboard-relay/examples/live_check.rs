//! Throwaway end-to-end check against a deployed relay:
//!
//!   cargo run --manifest-path peckboard-relay/Cargo.toml --example live_check \
//!     [-- relay.peckboard.com:443]
//!
//! Runs box + device with a fresh random pairing secret, exchanges E2E
//! messages, gets punch candidates and punches, then checks that a wrong
//! secret / wrong key / garbage auth all look exactly like an offline peer.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use peckboard_relay::client::{ClientConfig, Event, RelayClient};
use peckboard_relay::keys::{EXPORTER_LABEL, EXPORTER_LEN, PairingSecret, auth_message};
use peckboard_relay::proto::{ALPN, ClientMsg, Role, ServerMsg, read_frame, write_frame};
use rustls::pki_types::ServerName;
use tokio::net::{TcpStream, UdpSocket};
use tokio_rustls::TlsConnector;

async fn next(c: &mut RelayClient, want: impl Fn(&Event) -> bool) -> anyhow::Result<Event> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let ev = c.next_event().await.context("relay closed")?;
            if want(&ev) {
                return Ok(ev);
            }
        }
    })
    .await
    .context("timed out waiting for event")?
}

/// Every event seen within `d` (after one Ping).
async fn drain(c: &mut RelayClient, d: Duration) -> Vec<String> {
    let _ = c.ping().await;
    let mut out = vec![];
    let deadline = tokio::time::Instant::now() + d;
    while let Ok(Some(ev)) = tokio::time::timeout_at(deadline, c.next_event()).await {
        out.push(
            format!("{ev:?}")
                .split([' ', '{', '('])
                .next()
                .unwrap()
                .to_string(),
        );
    }
    out
}

fn kind(m: &ServerMsg) -> &'static str {
    match m {
        ServerMsg::Challenge { .. } => "challenge",
        ServerMsg::Registered { .. } => "registered",
        ServerMsg::Pong => "pong",
        ServerMsg::PeerOnline => "peer-online",
        ServerMsg::PeerOffline => "peer-offline",
        ServerMsg::Forwarded { .. } => "forwarded",
        ServerMsg::Data { .. } => "data",
        ServerMsg::PunchNow { .. } => "punch",
        ServerMsg::IdentityStatus { .. } => "identity-status",
    }
}

/// Hand-driven session; returns message kinds (after one Ping, 1.5 s
/// window) and Auth→first-reply latency.
async fn raw_session(
    cfg: &ClientConfig,
    hello: Vec<u8>,
    auth: impl FnOnce([u8; 32], [u8; EXPORTER_LEN]) -> Vec<u8>,
) -> anyhow::Result<(Vec<&'static str>, Duration)> {
    let mut cc = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_root_certificates(cfg.roots.clone())
    .with_no_client_auth();
    cc.alpn_protocols = vec![ALPN.to_vec()];
    let tcp = TcpStream::connect(cfg.relay).await?;
    let tls = TlsConnector::from(Arc::new(cc))
        .connect(ServerName::try_from(cfg.server_name.clone())?, tcp)
        .await?;
    let mut ex = [0u8; EXPORTER_LEN];
    tls.get_ref()
        .1
        .export_keying_material(&mut ex, EXPORTER_LABEL, None)?;
    let (mut rd, mut wr) = tokio::io::split(tls);
    write_frame(&mut wr, &hello).await?;
    let ServerMsg::Challenge { nonce } = ServerMsg::decode(&read_frame(&mut rd).await?)? else {
        bail!("no challenge")
    };
    let t0 = Instant::now();
    write_frame(&mut wr, &auth(nonce, ex)).await?;
    let first = ServerMsg::decode(&read_frame(&mut rd).await?)?;
    let lat = t0.elapsed();
    let mut kinds = vec![kind(&first)];
    write_frame(&mut wr, &ClientMsg::Ping.encode()).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    while let Ok(Ok(f)) = tokio::time::timeout_at(deadline, read_frame(&mut rd)).await {
        kinds.push(kind(&ServerMsg::decode(&f)?));
    }
    Ok((kinds, lat))
}

/// Local address the OS would use to reach `dst` (a LAN candidate).
fn local_ip_toward(dst: IpAddr) -> anyhow::Result<IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0")?;
    s.connect((dst, 9))?;
    Ok(s.local_addr()?.ip())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let target = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "relay.peckboard.com:443".into());
    let host = target.rsplit_once(':').context("host:port")?.0.to_string();
    let relay = tokio::net::lookup_host(&target)
        .await?
        .find(SocketAddr::is_ipv4)
        .context("resolve")?;
    println!("relay {host} -> {relay}");
    let cfg = ClientConfig::webpki(relay, &host);

    // --- Real rendezvous -------------------------------------------------
    let s = PairingSecret::generate();
    let mut boxc = RelayClient::connect(&cfg, &s, Role::Box).await?;
    let mut dev = RelayClient::connect(&cfg, &s, Role::Device).await?;
    next(&mut boxc, |e| matches!(e, Event::PeerOnline)).await?;
    next(&mut dev, |e| matches!(e, Event::PeerOnline)).await?;
    println!("ok: both peers online (TLS 1.3 + webpki-verified cert)");

    let text = b"live-check: E2E MARKER 0123456789";
    boxc.send(text).await?;
    let Event::Message { plaintext, sealed } =
        next(&mut dev, |e| matches!(e, Event::Message { .. })).await?
    else {
        unreachable!()
    };
    anyhow::ensure!(plaintext == text, "plaintext mismatch");
    anyhow::ensure!(
        !sealed.windows(8).any(|w| text.windows(8).any(|p| p == w)),
        "relay-visible blob contains plaintext"
    );
    dev.send(b"answer").await?;
    let Event::Message { plaintext, .. } =
        next(&mut boxc, |e| matches!(e, Event::Message { .. })).await?
    else {
        unreachable!()
    };
    anyhow::ensure!(plaintext == b"answer");
    println!(
        "ok: E2E messages both ways (relay saw {} opaque bytes)",
        sealed.len()
    );

    let lan = local_ip_toward(relay.ip())?;
    let ub = UdpSocket::bind((lan, 0)).await?;
    let ud = UdpSocket::bind((lan, 0)).await?;
    boxc.set_candidates(&[ub.local_addr()?]).await?;
    dev.set_candidates(&[ud.local_addr()?]).await?;
    let pub_b = boxc.stun_binding(&ub).await?;
    let pub_d = dev.stun_binding(&ud).await?;
    println!(
        "ok: authenticated STUN answered (box/device public ports {} / {})",
        pub_b.port(),
        pub_d.port()
    );
    let Event::Punch(pb) = next(&mut boxc, |e| matches!(e, Event::Punch(_))).await? else {
        unreachable!()
    };
    let Event::Punch(pd) = next(&mut dev, |e| matches!(e, Event::Punch(_))).await? else {
        unreachable!()
    };
    anyhow::ensure!(pb.nonce == pd.nonce, "nonce mismatch");
    anyhow::ensure!(
        pb.peer_candidates == vec![ud.local_addr()?],
        "box got wrong candidates"
    );
    anyhow::ensure!(
        pd.peer_candidates == vec![ub.local_addr()?],
        "device got wrong candidates"
    );
    println!(
        "ok: PunchNow on both sides, attempt {}, peer candidates decrypted",
        pb.attempt
    );
    let t = Duration::from_secs(5);
    let (rb, rd) = tokio::join!(boxc.punch(&ub, &pb, t), dev.punch(&ud, &pd, t));
    println!(
        "ok: punch box->{:?} device->{:?}",
        rb.map(|a| a.port()).map_err(|e| e.to_string()),
        rd.map(|a| a.port()).map_err(|e| e.to_string())
    );

    // --- Indistinguishability -------------------------------------------
    let box_keys = s.derive();
    // Baseline: legit device whose box is offline.
    let lonely = PairingSecret::generate().derive();
    let baseline = raw_session(
        &cfg,
        ClientMsg::Hello {
            role: Role::Device,
            rendezvous_id: lonely.rendezvous_id,
            public_key: lonely.public_key(),
        }
        .encode(),
        |n, ex| {
            ClientMsg::Auth {
                signature: lonely.sign_challenge(&n, Role::Device, &ex),
            }
            .encode()
        },
    )
    .await?;
    // Knows the live box's id, signs with its own key.
    let atk = PairingSecret::generate().derive();
    let wrong_key = raw_session(
        &cfg,
        ClientMsg::Hello {
            role: Role::Device,
            rendezvous_id: box_keys.rendezvous_id,
            public_key: atk.public_key(),
        }
        .encode(),
        |n, ex| {
            use ed25519_dalek::Signer;
            let m = auth_message(&n, &box_keys.rendezvous_id, Role::Device, &ex);
            ClientMsg::Auth {
                signature: atk.signing.sign(&m).to_bytes(),
            }
            .encode()
        },
    )
    .await?;
    // Right id + pubkey, garbage signature.
    let bad_sig = raw_session(
        &cfg,
        ClientMsg::Hello {
            role: Role::Device,
            rendezvous_id: box_keys.rendezvous_id,
            public_key: box_keys.public_key(),
        }
        .encode(),
        |_, _| ClientMsg::Auth { signature: [7; 64] }.encode(),
    )
    .await?;
    let garbage = raw_session(&cfg, vec![0x01, 0xff, 1, 2, 3], |_, _| vec![0x99; 3]).await?;
    for (name, (kinds, lat)) in [
        ("offline-peer baseline", &baseline),
        ("wrong key for live id", &wrong_key),
        ("bad signature", &bad_sig),
        ("malformed hello/auth", &garbage),
    ] {
        println!("  {name:<24} {kinds:?} auth->reply {lat:?}");
    }
    for (name, r) in [
        ("wrong key", &wrong_key),
        ("bad sig", &bad_sig),
        ("garbage", &garbage),
    ] {
        anyhow::ensure!(
            r.0 == baseline.0,
            "{name}: message sequence differs from baseline"
        );
    }

    // Wrong secret via the real client vs a lonely legit device.
    let mut wrong = RelayClient::connect(&cfg, &PairingSecret::generate(), Role::Device).await?;
    let mut alone = RelayClient::connect(&cfg, &PairingSecret::generate(), Role::Device).await?;
    let (w, a) = tokio::join!(
        drain(&mut wrong, Duration::from_secs(2)),
        drain(&mut alone, Duration::from_secs(2))
    );
    println!("  wrong-secret client events {w:?}; lonely client events {a:?}");
    anyhow::ensure!(w == a, "wrong secret distinguishable from offline peer");
    // The live box never heard about any of it.
    let b = drain(&mut boxc, Duration::from_secs(2)).await;
    println!("  live box events during attacks {b:?}");
    anyhow::ensure!(b == ["Pong"], "box saw attacker traffic");
    println!("ok: wrong secret / key / signature indistinguishable from an offline peer");
    Ok(())
}

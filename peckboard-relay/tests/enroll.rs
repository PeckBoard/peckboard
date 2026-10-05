//! Pairing v2 enrollment over loopback: v2 links enroll a device key once,
//! the device then runs on its own key and the rendezvous secret `R`, and
//! legacy pairings still connect and upgrade.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ed25519_dalek::SigningKey;
use peckboard_relay::client::ClientConfig;
use peckboard_relay::identity::BoxIdentity;
use peckboard_relay::keys::{PairingSecret, RendezvousSecret};
use peckboard_relay::proto::Role;
use peckboard_relay::server::{Relay, RelayConfig};
use peckboard_relay::tls;
use peckboard_relay::tunnel::enroll::{EnrollMsg, EnrollRequest, exporter, read_msg, write_msg};
use peckboard_relay::tunnel::{
    BoxCredential, CancellationToken, DeviceCredential, DeviceEvent, DeviceOptions, EnrollHandler,
    EnrollMode, LinkMode, ListenAddr, PairingLink, PunchedPath, RefuseReason, STREAM_ENROLL,
    STREAM_PING, STREAM_TCP, TunnelError, TunnelEvent, async_trait, bind_listener,
    connect_raw_with, establish, quinn, run_device, serve_box_with,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;

const NAME: &str = "relay.test";
const T: Duration = Duration::from_secs(30);
const FAR: u64 = 4_000_000_000;

// ---- harness ------------------------------------------------------------

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

/// HTTP `echo <request line>`.
async fn echo_server() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut b = [0u8; 1];
                while !buf.ends_with(b"\r\n\r\n") {
                    if s.read(&mut b).await.unwrap_or(0) == 0 {
                        return;
                    }
                    buf.push(b[0]);
                }
                let head = String::from_utf8_lossy(&buf);
                let body = format!("echo {}", head.lines().next().unwrap_or(""));
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

/// In-memory box state for one device row: first key wins, the same key
/// gets the same `R` again.
#[derive(Default)]
struct Mem {
    st: Mutex<St>,
    /// Told about every first grant (the test starts the `R` loop).
    granted: Mutex<Option<GrantTx>>,
}

type GrantTx = mpsc::UnboundedSender<([u8; 32], RendezvousSecret)>;

#[derive(Default)]
struct St {
    device: Option<[u8; 32]>,
    r: Option<RendezvousSecret>,
    enroll_calls: u32,
    modes: Vec<EnrollMode>,
    names: Vec<String>,
    acked: u32,
    refused: Vec<RefuseReason>,
    reuse: Vec<RefuseReason>,
    activated: Vec<[u8; 32]>,
}

impl Mem {
    fn new() -> (
        Arc<Self>,
        mpsc::UnboundedReceiver<([u8; 32], RendezvousSecret)>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let m = Arc::new(Self::default());
        *m.granted.lock().unwrap() = Some(tx);
        (m, rx)
    }

    fn st<R>(&self, f: impl FnOnce(&St) -> R) -> R {
        f(&self.st.lock().unwrap())
    }
}

#[async_trait]
impl EnrollHandler for Mem {
    async fn enroll(
        &self,
        mode: EnrollMode,
        device_key: [u8; 32],
        name: &str,
        _from: SocketAddr,
    ) -> Result<RendezvousSecret, RefuseReason> {
        let mut st = self.st.lock().unwrap();
        st.enroll_calls += 1;
        st.modes.push(mode);
        st.names.push(name.to_string());
        match st.device {
            Some(d) if d == device_key => Ok(st.r.clone().unwrap()),
            Some(_) => {
                st.refused.push(RefuseReason::AlreadyUsed);
                Err(RefuseReason::AlreadyUsed)
            }
            None => {
                let r = RendezvousSecret::generate();
                st.device = Some(device_key);
                st.r = Some(r.clone());
                if let Some(tx) = &*self.granted.lock().unwrap() {
                    let _ = tx.send((device_key, r.clone()));
                }
                Ok(r)
            }
        }
    }

    async fn acked(&self, _device_key: [u8; 32]) {
        self.st.lock().unwrap().acked += 1;
    }

    async fn link_reuse(&self, _from: SocketAddr, reason: RefuseReason) {
        self.st.lock().unwrap().reuse.push(reason);
    }

    async fn activated(&self, device_key: [u8; 32], _from: SocketAddr) {
        self.st.lock().unwrap().activated.push(device_key);
    }
}

type Events = mpsc::UnboundedReceiver<TunnelEvent>;

/// A box loop at the relay: establish with the credential's rendezvous
/// secret, serve, repeat.
fn box_loop(
    cfg: ClientConfig,
    cred: BoxCredential,
    handler: Option<Arc<dyn EnrollHandler>>,
    target: SocketAddr,
) -> (tokio::task::JoinHandle<()>, Events) {
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        loop {
            let Ok(path) = establish(&cfg, &cred.relay_secret(), Role::Box).await else {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            };
            let tx = tx.clone();
            let _ = serve_box_with(path, &cred, target, handler.clone(), move |e| {
                let _ = tx.send(e);
            })
            .await;
        }
    });
    (task, rx)
}

struct Device {
    port: u16,
    ev: mpsc::UnboundedReceiver<DeviceEvent>,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

fn device(cfg: &ClientConfig, mut opts: DeviceOptions) -> impl Future<Output = Device> {
    opts.relay = Some(cfg.clone());
    opts.min_backoff = Duration::from_millis(100);
    opts.device_name = "test phone".into();
    async move {
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
}

async fn wait_for(
    ev: &mut mpsc::UnboundedReceiver<DeviceEvent>,
    want: impl Fn(&DeviceEvent) -> bool,
) -> DeviceEvent {
    tokio::time::timeout(T, async {
        while let Some(e) = ev.recv().await {
            eprintln!("device event: {e:?}");
            if want(&e) {
                return e;
            }
        }
        panic!("device loop ended");
    })
    .await
    .expect("device event did not arrive")
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
            relay_identity: None,
        },
        PunchedPath {
            socket: b.into(),
            peer: aa,
            role: Role::Device,
            relay_identity: None,
        },
    )
}

/// Serve one box connection with `cred` on a fresh punched pair; returns
/// the device's path.
async fn box_once(
    cred: BoxCredential,
    handler: Option<Arc<dyn EnrollHandler>>,
) -> (
    PunchedPath,
    Events,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let target = echo_server().await;
    let (bp, dp) = paths().await;
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        serve_box_with(bp, &cred, target, handler, move |e| {
            let _ = tx.send(e);
        })
        .await
    });
    (dp, rx, task)
}

/// Hand-rolled device side of one enrollment on `conn`, signed for the
/// channel binding `x` (normally `conn`'s own).
async fn enroll_raw(
    conn: &quinn::Connection,
    x: [u8; 32],
    mode: EnrollMode,
    key: &SigningKey,
) -> anyhow::Result<EnrollMsg> {
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&[STREAM_ENROLL]).await?;
    let req = EnrollRequest::sign(&x, mode, key, "raw");
    write_msg(&mut send, &EnrollMsg::Request(req)).await?;
    let m = read_msg(&mut recv).await?;
    if matches!(m, EnrollMsg::Grant(_)) {
        write_msg(&mut send, &EnrollMsg::Ack).await?;
        let _ = send.finish();
    }
    Ok(m)
}

fn key(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn link_cred(s: &PairingSecret, id: &BoxIdentity, mode: LinkMode) -> BoxCredential {
    BoxCredential::Link {
        s: s.clone(),
        identity: id.clone(),
        mode,
    }
}

fn v2_link(s: &PairingSecret, id: &BoxIdentity) -> PairingLink {
    PairingLink::new_v2(s.clone(), "unused.test", id.public_key(), FAR)
}

// ---- tests --------------------------------------------------------------

/// A v2 link enrolls, the credential is saved before the box gets the
/// ack, the device moves to `rid(R)` with its own key, the box sees the
/// activation, and data flows.
#[tokio::test]
async fn new_link_enrolls_then_runs_on_r() {
    let cfg = relay().await;
    let target = echo_server().await;
    let id = BoxIdentity::generate();
    let s = PairingSecret::generate();
    let (mem, mut granted) = Mem::new();
    let h: Arc<dyn EnrollHandler> = mem.clone();
    let (_link_loop, _) = box_loop(
        cfg.clone(),
        link_cred(&s, &id, LinkMode::Enroll),
        Some(h.clone()),
        target,
    );
    // Start the `R` loop as soon as the box granted (as the box service
    // does when the row turns staged).
    let (cfg2, id2, h2) = (cfg.clone(), id.clone(), h.clone());
    tokio::spawn(async move {
        let (d, r) = granted.recv().await.unwrap();
        let cred = BoxCredential::Enrolled {
            r,
            identity: id2,
            device_key: d,
        };
        let (task, _ev) = box_loop(cfg2, cred, Some(h2), target);
        let _ = task.await;
    });

    let saved: Arc<Mutex<Option<String>>> = Arc::default();
    let acked_at_save = Arc::new(Mutex::new(None));
    let mut opts = DeviceOptions::new(DeviceCredential::Link {
        link: v2_link(&s, &id),
        device_key: key(7),
    });
    let (sv, mem2, at) = (saved.clone(), mem.clone(), acked_at_save.clone());
    opts.on_enrolled = Some(Arc::new(move |c| {
        *at.lock().unwrap() = Some(mem2.st(|st| st.acked));
        *sv.lock().unwrap() = Some(c.encode());
        Ok(())
    }));
    let mut dev = device(&cfg, opts).await;
    let ev = wait_for(&mut dev.ev, |e| matches!(e, DeviceEvent::Enrolled { .. })).await;
    let DeviceEvent::Enrolled {
        box_fingerprint,
        legacy_upgrade,
    } = ev
    else {
        unreachable!()
    };
    assert_eq!(box_fingerprint, id.fingerprint());
    assert!(!legacy_upgrade);
    assert_eq!(
        *acked_at_save.lock().unwrap(),
        Some(0),
        "saved after the ack"
    );
    wait_for(&mut dev.ev, |e| matches!(e, DeviceEvent::Connected { .. })).await;
    wait_for(&mut dev.ev, |e| matches!(e, DeviceEvent::Activated)).await;

    let r = http_get(dev.port, "/after-enroll").await;
    assert!(r.ends_with("echo GET /after-enroll HTTP/1.1"), "{r}");
    let d = key(7).verifying_key().to_bytes();
    mem.st(|st| {
        assert_eq!(st.modes, [EnrollMode::Link]);
        assert_eq!(st.names, ["test phone"]);
        assert_eq!(st.acked, 1);
        assert!(st.activated.contains(&d));
    });
    // The saved credential is the one the device runs on.
    let c = peckboard_relay::tunnel::EnrolledCredential::parse(
        saved.lock().unwrap().as_deref().unwrap(),
    )
    .unwrap();
    assert_eq!(c.device_public_key(), d);
    let r = mem.st(|st| st.r.clone().unwrap());
    assert!(*c.rendezvous() == r);
    dev.cancel.cancel();
    dev.task.await.unwrap().unwrap();
}

/// `on_enrolled` fails (the keychain write crashed): no ack, the device
/// retries with the same key, and the box re-delivers the same `R`.
#[tokio::test]
async fn failed_save_retries_and_gets_the_same_r() {
    let cfg = relay().await;
    let id = BoxIdentity::generate();
    let s = PairingSecret::generate();
    let (mem, _granted) = Mem::new();
    let (_l, _) = box_loop(
        cfg.clone(),
        link_cred(&s, &id, LinkMode::Enroll),
        Some(mem.clone()),
        echo_server().await,
    );
    let rs: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut opts = DeviceOptions::new(DeviceCredential::Link {
        link: v2_link(&s, &id),
        device_key: key(3),
    });
    let rs2 = rs.clone();
    opts.on_enrolled = Some(Arc::new(move |c| {
        let mut v = rs2.lock().unwrap();
        v.push(c.encode());
        if v.len() == 1 {
            anyhow::bail!("disk full");
        }
        Ok(())
    }));
    let mut dev = device(&cfg, opts).await;
    let failed = wait_for(&mut dev.ev, |e| matches!(e, DeviceEvent::Failed(_))).await;
    assert!(format!("{failed:?}").contains("Couldn't save the pairing key"));
    wait_for(&mut dev.ev, |e| matches!(e, DeviceEvent::Enrolled { .. })).await;
    let v = rs.lock().unwrap().clone();
    assert_eq!(v.len(), 2);
    assert_eq!(v[0], v[1], "re-delivered credential differs");
    mem.st(|st| {
        assert_eq!(st.enroll_calls, 2);
        assert_eq!(st.acked, 1);
    });
    dev.cancel.cancel();
}

/// A wrong `k` in the link: the device refuses the real box's handshake
/// and never sends an enrollment.
#[tokio::test]
async fn wrong_box_key_fails_handshake() {
    let id = BoxIdentity::generate();
    let s = PairingSecret::generate();
    let (mem, _g) = Mem::new();
    let (dp, _ev, _t) = box_once(link_cred(&s, &id, LinkMode::Enroll), Some(mem.clone())).await;
    let wrong = DeviceCredential::Link {
        link: v2_link(&s, &BoxIdentity::generate()),
        device_key: key(1),
    };
    assert!(connect_raw_with(dp, &wrong).await.is_err());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(mem.st(|st| st.enroll_calls), 0);
}

/// Someone holding `S` but not the box identity key can't impersonate the
/// box to a device with a v2 link or an enrolled credential.
#[tokio::test]
async fn box_impersonation_with_s_is_refused() {
    let real = BoxIdentity::generate();
    let fake = BoxIdentity::generate();
    let s = PairingSecret::generate();
    let (mem, _g) = Mem::new();
    let (dp, _ev, _t) = box_once(link_cred(&s, &fake, LinkMode::Enroll), Some(mem.clone())).await;
    let dev = DeviceCredential::Link {
        link: v2_link(&s, &real),
        device_key: key(1),
    };
    assert!(connect_raw_with(dp, &dev).await.is_err());
    // An enrolled device pinned to the real box, against a box that has
    // the right `R` and device key but the wrong identity.
    let r = RendezvousSecret::generate();
    let d = key(2);
    let (dp, _ev, _t) = box_once(
        BoxCredential::Enrolled {
            r: r.clone(),
            identity: fake,
            device_key: d.verifying_key().to_bytes(),
        },
        None,
    )
    .await;
    let enrolled =
        peckboard_relay::tunnel::EnrolledCredential::new(r, d, real.public_key(), "unused.test");
    assert!(
        connect_raw_with(dp, &DeviceCredential::Enrolled(enrolled))
            .await
            .is_err()
    );
    assert_eq!(mem.st(|st| st.enroll_calls), 0);
}

/// Second device, same link, while staged: refused `already_used`. The
/// first device asking again gets the identical `R`.
#[tokio::test]
async fn second_enrollment_is_refused_and_first_is_idempotent() {
    let id = BoxIdentity::generate();
    let s = PairingSecret::generate();
    let (mem, _g) = Mem::new();
    let h: Arc<dyn EnrollHandler> = mem.clone();
    let mut grants = Vec::new();
    for k in [key(1), key(2), key(1)] {
        let (dp, _ev, _t) = box_once(link_cred(&s, &id, LinkMode::Enroll), Some(h.clone())).await;
        let dev = DeviceCredential::Link {
            link: v2_link(&s, &id),
            device_key: k.clone(),
        };
        let (_ep, conn) = connect_raw_with(dp, &dev).await.unwrap();
        let x = exporter(&conn).unwrap();
        grants.push(enroll_raw(&conn, x, EnrollMode::Link, &k).await.unwrap());
    }
    let r = |m: &EnrollMsg| match m {
        EnrollMsg::Grant(g) => *g.r.as_bytes(),
        other => panic!("expected a grant, got {other:?}"),
    };
    assert!(matches!(
        grants[1],
        EnrollMsg::Refused(RefuseReason::AlreadyUsed)
    ));
    assert_eq!(r(&grants[0]), r(&grants[2]));
    mem.st(|st| assert_eq!(st.refused, [RefuseReason::AlreadyUsed]));

    // After activation the link loop refuses everyone and reports the reuse.
    let (dp, _ev, _t) = box_once(
        link_cred(&s, &id, LinkMode::Refuse(RefuseReason::AlreadyUsed)),
        Some(h.clone()),
    )
    .await;
    let (_ep, conn) = connect_raw_with(
        dp,
        &DeviceCredential::Link {
            link: v2_link(&s, &id),
            device_key: key(1),
        },
    )
    .await
    .unwrap();
    let x = exporter(&conn).unwrap();
    let m = enroll_raw(&conn, x, EnrollMode::Link, &key(1))
        .await
        .unwrap();
    assert!(matches!(m, EnrollMsg::Refused(RefuseReason::AlreadyUsed)));
    mem.st(|st| {
        assert_eq!(st.reuse, [RefuseReason::AlreadyUsed]);
        assert_eq!(st.enroll_calls, 3, "refuse loop must not reach enroll()");
    });
}

/// A request captured on one connection and replayed on another is
/// rejected before the handler sees it (channel binding).
#[tokio::test]
async fn replayed_request_on_another_connection_is_rejected() {
    let id = BoxIdentity::generate();
    let s = PairingSecret::generate();
    let (mem, _g) = Mem::new();
    let h: Arc<dyn EnrollHandler> = mem.clone();
    let dev = DeviceCredential::Link {
        link: v2_link(&s, &id),
        device_key: key(4),
    };
    let (dp1, _e1, _t1) = box_once(link_cred(&s, &id, LinkMode::Enroll), Some(h.clone())).await;
    let (_ep1, conn1) = connect_raw_with(dp1, &dev).await.unwrap();
    let x1 = exporter(&conn1).unwrap();
    let (dp2, _e2, _t2) = box_once(link_cred(&s, &id, LinkMode::Enroll), Some(h.clone())).await;
    let (_ep2, conn2) = connect_raw_with(dp2, &dev).await.unwrap();
    assert_ne!(x1, exporter(&conn2).unwrap());
    // Signed for connection 1, sent on connection 2: stream reset.
    let r = enroll_raw(&conn2, x1, EnrollMode::Link, &key(4)).await;
    assert!(r.is_err(), "replay answered: {r:?}");
    assert_eq!(mem.st(|st| st.enroll_calls), 0);
}

/// An `S`-authenticated `/2` connection is enrollment only: TCP streams are
/// reset, pings answered.
#[tokio::test]
async fn link_connection_is_enroll_only() {
    let id = BoxIdentity::generate();
    let s = PairingSecret::generate();
    let (mem, _g) = Mem::new();
    let (dp, _ev, _t) = box_once(link_cred(&s, &id, LinkMode::Enroll), Some(mem)).await;
    let dev = DeviceCredential::Link {
        link: v2_link(&s, &id),
        device_key: key(5),
    };
    let (_ep, conn) = connect_raw_with(dp, &dev).await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(&[STREAM_TCP]).await.unwrap();
    send.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
    let _ = send.finish();
    let r = tokio::time::timeout(T, recv.read_to_end(1024))
        .await
        .unwrap();
    assert!(
        r.is_err(),
        "TCP stream was served on an enroll-only connection"
    );
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(&[STREAM_PING, 0]).await.unwrap();
    let mut b = [9u8; 1];
    tokio::time::timeout(T, recv.read_exact(&mut b))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(b, [0]);
}

/// The `Enrolled` loop accepts only the enrolled key: not the `S`-derived
/// device cert, not a random key.
#[tokio::test]
async fn enrolled_loop_accepts_only_the_device_key() {
    let id = BoxIdentity::generate();
    let s = PairingSecret::generate();
    let r = RendezvousSecret::generate();
    let d = key(6);
    let cred = || BoxCredential::Enrolled {
        r: r.clone(),
        identity: id.clone(),
        device_key: d.verifying_key().to_bytes(),
    };
    let as_dev = |k: SigningKey| {
        DeviceCredential::Enrolled(peckboard_relay::tunnel::EnrolledCredential::new(
            r.clone(),
            k,
            id.public_key(),
            "unused.test",
        ))
    };
    let (mem, _g) = Mem::new();
    let (dp, _ev, _t) = box_once(cred(), Some(mem.clone())).await;
    let s_cert = DeviceCredential::Link {
        link: v2_link(&s, &id),
        device_key: d.clone(),
    };
    assert!(
        box_rejects(dp, &s_cert).await,
        "S-derived device cert accepted"
    );
    let (dp, _ev, _t) = box_once(cred(), Some(mem.clone())).await;
    assert!(
        box_rejects(dp, &as_dev(key(99))).await,
        "random key accepted"
    );
    assert!(mem.st(|st| st.activated.is_empty()));
    let (dp, _ev, _t) = box_once(cred(), Some(mem.clone())).await;
    let (_ep, conn) = connect_raw_with(dp, &as_dev(d.clone())).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(conn.close_reason().is_none());
    mem.st(|st| assert_eq!(st.activated, [d.verifying_key().to_bytes()]));
}

/// TLS 1.3: the client's handshake completes before the box has checked
/// the client cert, so a rejection shows up as the box closing at once.
async fn box_rejects(dp: PunchedPath, cred: &DeviceCredential) -> bool {
    match connect_raw_with(dp, cred).await {
        Err(_) => true,
        Ok((_ep, conn)) => tokio::time::timeout(Duration::from_secs(5), conn.closed())
            .await
            .is_ok(),
    }
}

/// Old apps on a v2 link (`/1`): closed with `update-app`; on a refusing
/// link: closed with the refusal, reported as a reuse.
#[tokio::test]
async fn old_app_on_new_link_is_told_to_update() {
    let id = BoxIdentity::generate();
    let s = PairingSecret::generate();
    let legacy = DeviceCredential::Legacy {
        link: PairingLink::new(s.clone(), "unused.test"),
        upgrade_key: None,
    };
    for (mode, want) in [
        (LinkMode::Enroll, "update-app"),
        (LinkMode::Refuse(RefuseReason::Expired), "link-expired"),
    ] {
        let (mem, _g) = Mem::new();
        let (dp, mut ev, task) = box_once(link_cred(&s, &id, mode), Some(mem.clone())).await;
        let (_ep, conn) = connect_raw_with(dp, &legacy).await.unwrap();
        let e = tokio::time::timeout(T, conn.closed()).await.unwrap();
        match e {
            quinn::ConnectionError::ApplicationClosed(c) => {
                assert_eq!(&c.reason[..], want.as_bytes())
            }
            other => panic!("expected {want}, got {other:?}"),
        }
        task.await.unwrap().unwrap();
        let mut reasons = Vec::new();
        while let Ok(e) = ev.try_recv() {
            reasons.push(format!("{e:?}"));
        }
        assert!(reasons.iter().any(|r| r.contains(want)), "{reasons:?}");
        let reused = mem.st(|st| st.reuse.clone());
        match mode {
            LinkMode::Enroll => assert!(reused.is_empty()),
            LinkMode::Refuse(r) => assert_eq!(reused, [r]),
        }
    }
}

/// An expired link refuses enrollment; `run_device` gives up with
/// `EnrollRefused(expired)`.
#[tokio::test]
async fn expired_link_refuses_and_run_device_gives_up() {
    let cfg = relay().await;
    let id = BoxIdentity::generate();
    let s = PairingSecret::generate();
    let (mem, _g) = Mem::new();
    let (_l, _) = box_loop(
        cfg.clone(),
        link_cred(&s, &id, LinkMode::Refuse(RefuseReason::Expired)),
        Some(mem.clone()),
        echo_server().await,
    );
    let mut dev = device(
        &cfg,
        DeviceOptions::new(DeviceCredential::Link {
            link: v2_link(&s, &id),
            device_key: key(8),
        }),
    )
    .await;
    wait_for(&mut dev.ev, |e| {
        matches!(
            e,
            DeviceEvent::EnrollRefused {
                reason: RefuseReason::Expired
            }
        )
    })
    .await;
    let err = tokio::time::timeout(T, dev.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(
        err.downcast_ref::<TunnelError>(),
        Some(TunnelError::EnrollRefused {
            reason: RefuseReason::Expired
        })
    ));
    mem.st(|st| assert_eq!(st.reuse, [RefuseReason::Expired]));
}

/// A legacy pairing still connects on `/1` and upgrades in place: the
/// tunnel keeps serving, and the next round runs on `R` with the device
/// key.
#[tokio::test]
async fn legacy_pairing_connects_and_upgrades() {
    let cfg = relay().await;
    let target = echo_server().await;
    let id = BoxIdentity::generate();
    let s = PairingSecret::generate();
    let (mem, mut granted) = Mem::new();
    let h: Arc<dyn EnrollHandler> = mem.clone();
    let (_legacy_loop, _) = box_loop(
        cfg.clone(),
        BoxCredential::Legacy {
            s: s.clone(),
            identity: Some(id.clone()),
        },
        Some(h.clone()),
        target,
    );
    let (cfg2, id2, h2) = (cfg.clone(), id.clone(), h.clone());
    tokio::spawn(async move {
        let (d, r) = granted.recv().await.unwrap();
        let cred = BoxCredential::Enrolled {
            r,
            identity: id2,
            device_key: d,
        };
        let (task, _ev) = box_loop(cfg2, cred, Some(h2), target);
        let _ = task.await;
    });
    let saved: Arc<Mutex<u32>> = Arc::default();
    let sv = saved.clone();
    let mut opts = DeviceOptions::new(DeviceCredential::Legacy {
        link: PairingLink::new(s.clone(), "unused.test"),
        upgrade_key: Some(key(9)),
    });
    opts.on_enrolled = Some(Arc::new(move |_| {
        *sv.lock().unwrap() += 1;
        Ok(())
    }));
    let kick = opts.kick.clone();
    let mut dev = device(&cfg, opts).await;
    wait_for(&mut dev.ev, |e| matches!(e, DeviceEvent::Connected { .. })).await;
    let ev = wait_for(&mut dev.ev, |e| matches!(e, DeviceEvent::Enrolled { .. })).await;
    assert!(matches!(
        ev,
        DeviceEvent::Enrolled {
            legacy_upgrade: true,
            ..
        }
    ));
    assert_eq!(*saved.lock().unwrap(), 1);
    // The legacy tunnel is still up.
    let r = http_get(dev.port, "/legacy").await;
    assert!(r.ends_with("echo GET /legacy HTTP/1.1"), "{r}");
    mem.st(|st| assert_eq!(st.modes, [EnrollMode::LegacyUpgrade]));
    // Next round (network change): on `R`, activated.
    assert!(kick.network_changed());
    wait_for(&mut dev.ev, |e| matches!(e, DeviceEvent::Activated)).await;
    let r = http_get(dev.port, "/upgraded").await;
    assert!(r.ends_with("echo GET /upgraded HTTP/1.1"), "{r}");
    mem.st(|st| assert_eq!(st.activated, [key(9).verifying_key().to_bytes()]));
    dev.cancel.cancel();
}

/// A box without enrollment support (old box): the upgrade stream is
/// reset, the device stays legacy and keeps working, without retrying the
/// upgrade on the next round.
#[tokio::test]
async fn legacy_upgrade_against_old_box_stays_legacy() {
    let cfg = relay().await;
    let s = PairingSecret::generate();
    let (_l, _) = box_loop(
        cfg.clone(),
        BoxCredential::Legacy {
            s: s.clone(),
            identity: None,
        },
        None,
        echo_server().await,
    );
    let mut opts = DeviceOptions::new(DeviceCredential::Legacy {
        link: PairingLink::new(s.clone(), "unused.test"),
        upgrade_key: Some(key(10)),
    });
    let saves = Arc::new(Mutex::new(0u32));
    let sv = saves.clone();
    opts.on_enrolled = Some(Arc::new(move |_| {
        *sv.lock().unwrap() += 1;
        Ok(())
    }));
    let kick = opts.kick.clone();
    let mut dev = device(&cfg, opts).await;
    wait_for(&mut dev.ev, |e| matches!(e, DeviceEvent::Connected { .. })).await;
    let r = http_get(dev.port, "/old-box").await;
    assert!(r.ends_with("echo GET /old-box HTTP/1.1"), "{r}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(kick.network_changed());
    wait_for(&mut dev.ev, |e| matches!(e, DeviceEvent::Connected { .. })).await;
    let r = http_get(dev.port, "/still-legacy").await;
    assert!(r.ends_with("echo GET /still-legacy HTTP/1.1"), "{r}");
    while let Ok(e) = dev.ev.try_recv() {
        assert!(
            !matches!(e, DeviceEvent::Enrolled { .. } | DeviceEvent::Activated),
            "{e:?}"
        );
    }
    assert_eq!(*saves.lock().unwrap(), 0);
    dev.cancel.cancel();
}

/// Peckboard's old-style tunnel API (`&PairingSecret`) is the legacy
/// credential: an old device still connects to a box serving `Legacy`.
#[tokio::test]
async fn legacy_device_api_still_connects() {
    let s = PairingSecret::generate();
    let (dp, _ev, _t) = box_once(
        BoxCredential::Legacy {
            s: s.clone(),
            identity: Some(BoxIdentity::generate()),
        },
        None,
    )
    .await;
    let (_ep, conn) = peckboard_relay::tunnel::connect_raw(dp, &s).await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(&[STREAM_TCP]).await.unwrap();
    send.write_all(b"GET /old HTTP/1.1\r\n\r\n").await.unwrap();
    let _ = send.finish();
    let body = tokio::time::timeout(T, recv.read_to_end(4096))
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&body).ends_with("echo GET /old HTTP/1.1"));
}

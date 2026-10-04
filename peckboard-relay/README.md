# peckboard-relay

Rendezvous server. It introduces a user's device to their Peckboard box;
after that, traffic flows **directly** between the two over UDP hole
punching. Only when no direct path can be punched does the relay forward
the tunnel's end-to-end encrypted QUIC datagrams (see Relay Fallback); it
never sees plaintext.

Production: `relay.peckboard.com` — 443/tcp (TLS 1.3 signaling + ACME
TLS-ALPN-01) and 3478/udp (authenticated STUN).

## Protocol

1. The box generates a 32-byte pairing secret `S` and shares it only
   out-of-band (QR / link). HKDF-SHA256(`S`) yields a rendezvous id, an
   Ed25519 keypair, and an XChaCha20-Poly1305 E2E key (`src/keys.rs`).
2. A peer opens TLS 1.3 with ALPN `peckrelay/1` and sends
   `Hello{role, id, pubkey}`. The relay sends a random `Challenge`; the peer
   signs `nonce ‖ id ‖ role ‖ TLS-exporter` (channel-bound). First
   registration of an id records its pubkey **in memory only**.
3. After a fixed delay the relay sends `Registered` with a short-lived STUN
   credential. The peer sends an RFC 5389 Binding request with
   USERNAME + MESSAGE-INTEGRITY to 3478/udp; the relay records the observed
   public endpoint.
4. Once both peers of an id have endpoints, the relay sends each
   `PunchNow{peer public endpoint, start time, nonce, attempt, peer's sealed
candidates}`. `PunchRequest` asks for another round (bounded, spaced).
5. Everything peers exchange through the relay (`Forward`, candidate lists)
   is E2E-sealed; the relay only sees opaque blobs. `Forward` messages also
   carry, inside the ciphertext, a per-sender counter (seeded from the
   sender's µs clock, so it survives reconnects without stored state) and a
   unix-ms timestamp. The receiver drops any counter ≤ the last one it
   accepted from that sender (role within the id) and any timestamp outside
   ±120 s — replays and reordered-older messages are discarded.

## Multiple Devices

The relay deliberately holds one box slot and one device slot per
rendezvous id. Multiple devices are supported by giving **each device its
own pairing secret**: the box generates a fresh `S` per paired device and
registers one rendezvous id per device. This keeps every device's keys,
E2E channel, and replay state independent, and makes revocation
per-device — the box forgets that device's secret and stops registering its
id, without re-pairing the others.

Wire format: `src/proto.rs`. Peer library: `src/client.rs` (`client`
feature, on by default) — reused by the Peckboard box and `peckboard
connect`. The relay server itself (`server`/`tls`/`limits` modules, ACME,
CLI) and the `peckboard-relay` binary need feature `server`, so peers never
compile rustls-acme, clap or tracing-subscriber.

## Tunnel API

Feature `tunnel` (implies `client`; `src/tunnel.rs`) adds the data path:
direct after a punch (the relay is not involved), relayed when punching
fails (`src/tunnel/relayed.rs`).

```rust
use peckboard_relay::tunnel::*;

// Resolve `host[:port]` (default 443, prefers IPv4) → webpki ClientConfig.
pub async fn relay_config(host: &str) -> anyhow::Result<ClientConfig>;

// Rendezvous + hole punch. Box: waits for its device indefinitely (drop to
// cancel). Device: TunnelError::PeerOffline if the box isn't seen in 30 s.
// Punch rounds keep failing (both NATs hard): with a v2 relay and v2 peer,
// both switch to a relayed path (see Relay Fallback); otherwise
// TunnelError::PunchFailed { rounds } after 4 rounds. Downcast the
// anyhow::Error to tell them apart. Re-STUNs + pings the relay every 20 s
// while waiting; a direct path drops the relay session on return, a
// relayed one keeps it.
pub async fn establish(cfg: &ClientConfig, secret: &PairingSecret, role: Role)
    -> anyhow::Result<PunchedPath>;

// Same, with a fixed local UDP port and extra advertised candidates (sent
// sealed next to the LAN candidate; the punch probes them like any other).
// For a box behind a symmetric NAT: forward the port on the router and
// advertise it. Advertise::Port(p) = p on the STUN-observed public IP
// (public_ip_hint until STUN answers). Default options = `establish`.
pub async fn establish_with(cfg: &ClientConfig, secret: &PairingSecret, role: Role,
    opts: &EstablishOptions) -> anyhow::Result<PunchedPath>;

#[derive(Clone, Default)]
pub struct EstablishOptions {
    pub bind_port: Option<u16>,               // None: ephemeral
    pub advertise: Vec<Advertise>,            // Addr(SocketAddr) | Port(u16)
    pub public_ip_hint: Option<IpAddr>,
    pub on_registered: Option<OnRegistered>,  // Fn(&Registration{local_port, public, candidates})
    pub fallback: RelayFallback,  // { enabled: true, after_failures: 2, upgrade_every: Some(30 s) }
}

pub struct PunchedPath {
    pub socket: PathSocket,  // Direct(UdpSocket: the one the punch used) | Relayed(RelayedPath)
    pub peer: SocketAddr,    // direct: as seen from the socket; relayed: its STUN endpoint
    pub role: Role,
}
impl PunchedPath { fn kind(&self) -> PathKind; fn path_watch(&self) -> watch::Receiver<PathKind>; }
pub enum PathKind { Direct, Relayed }  // as_str(): "direct" | "relayed"

// Box: accept one QUIC connection (15 s window) from the paired device and
// forward every stream to `target`. Only `path.peer` may connect (others are
// refused); handshakes run concurrently, so a failed or stalled one can't
// shut the device out. Ok(()) when an established connection ends; Err if
// no authenticated device connects.
pub async fn serve_box(path: PunchedPath, secret: &PairingSecret, target: SocketAddr,
    on_event: impl Fn(TunnelEvent) + Send + Sync + 'static) -> anyhow::Result<()>;

// Device: connect, then each connection accepted on `listen` becomes a
// stream. Ok(()) when the tunnel ends (keep the listener; the next call
// serves whatever queued meanwhile); Err if the handshake fails.
pub async fn connect_device(path: PunchedPath, secret: &PairingSecret,
    listen: &tokio::net::TcpListener,
    on_event: impl Fn(TunnelEvent) + Send + Sync + 'static) -> anyhow::Result<()>;

#[derive(Clone, Debug)]
pub enum TunnelEvent {
    Connected { peer: SocketAddr, rtt_ms: u32, path: PathKind },
    PathChanged { path: PathKind },  // relayed tunnel upgraded to direct (or back)
    Disconnected { reason: String },
    Error(String),
}

// `peckboard://pair/<base64url(S)>?relay=<host[:port]>`; relay defaults to
// DEFAULT_RELAY ("relay.peckboard.com") when absent.
pub struct PairingLink { pub secret: PairingSecret, pub relay: String }
impl PairingLink { fn new(secret, relay: &str); fn to_uri(&self) -> String;
                   fn parse(link: &str) -> anyhow::Result<Self>; }
```

Box loop: `loop { let p = establish(.., Role::Box).await?; serve_box(p, ..).await; }`
— one rendezvous id per paired device, so run one loop per device secret.

### Device Loop and Loopback Gate

What `peckboard-connect` and the mobile app run. Stable names: `run_device`,
`DeviceOptions`, `CookieGate` (all in `peckboard_relay::tunnel`).

```rust
// Bind the local port. Prefer = fixed port, free port on the same IP if taken.
pub enum ListenAddr { Exact(SocketAddr), Prefer(SocketAddr), Ephemeral /* 127.0.0.1:0 */ }
pub async fn bind_listener(addr: ListenAddr) -> std::io::Result<TcpListener>;

#[derive(Clone)]
pub struct DeviceOptions {
    pub link: PairingLink,
    pub relay: Option<ClientConfig>,         // None: relay_config(&link.relay) per attempt
    pub accept_filter: Option<AcceptFilter>, // None: forward every local connection
    pub min_backoff: Duration,               // 1 s, doubles per failure ...
    pub max_backoff: Duration,               // ... up to 30 s
    pub stable_after: Duration,              // 30 s up resets the backoff
    pub give_up_on_punch_failure: bool,      // false: retry forever
}
impl DeviceOptions {
    pub fn new(link: PairingLink) -> Self;                // the defaults above
    pub fn with_gate(self, gate: &CookieGate) -> Self;    // accept_filter = gate.filter()
}

// Rendezvous → punch → connect → serve `listener` → back off → repeat.
// Ok(()) once `cancel` fires: tunnel + all streams closed. A listener passed
// by value is dropped (port stops accepting); pass Arc<TcpListener> to keep
// the port bound across a stop/restart (mobile background). Err only with
// give_up_on_punch_failure (punch failed before any tunnel came up). To
// resume: call again with a new token.
pub async fn run_device<L: Borrow<TcpListener> + Send>(opts: DeviceOptions, listener: L,
    cancel: CancellationToken,  // re-exported tokio_util token
    on_event: impl Fn(DeviceEvent) + Send + Sync + 'static) -> anyhow::Result<()>;

#[derive(Clone, Debug)]
pub enum DeviceEvent {     // per attempt: Connecting, then Connected..Disconnected
    Connecting,            //   or a failure, then Retrying
    Connected { peer: SocketAddr, rtt_ms: u32, path: PathKind },
    PathChanged { path: PathKind },      // relayed → direct upgrade (or back)
    Disconnected { reason: String },     // "stopped" after cancel
    PeerOffline,                         // box not at the relay (offline / revoked)
    PunchFailed { rounds: u32 },         // both NATs hard: no direct path
    Failed(String),                      // relay unreachable, handshake, local accept
    Retrying { after: Duration },
}

// Runs on each accepted local connection before it becomes a stream:
// Some(Admitted { head, tcp }) forwards `head` then the rest of `tcp`, None
// drops (it may answer the connection itself). Inspect with
// TcpStream::peek; consumed bytes reach the box only as `head`.
pub type AcceptFilter = Arc<dyn Fn(TcpStream)
    -> Pin<Box<dyn Future<Output = Option<Admitted>> + Send>> + Send + Sync>;
pub struct Admitted { pub head: Vec<u8>, pub tcp: TcpStream }

#[derive(Clone)] // cheap; Debug never prints the key
pub struct CookieGate { .. }
impl CookieGate {
    pub const COOKIE: &str = "__pbm";
    pub const BOOT_PATH: &str = "/__pbm/boot";
    pub fn new() -> Self;               // random 256-bit key, 64 hex chars
    pub fn key(&self) -> &str;
    pub fn boot_path(&self) -> String;  // "/__pbm/boot?k=<key>"
    pub fn boot_path_to(&self, next: &str) -> String; // ... "&next=<path>"
    pub fn filter(&self) -> AcceptFilter;
    pub async fn admit(&self, tcp: TcpStream) -> Option<Admitted>;
}
```

Cookie gate: the WebView first loads `http://127.0.0.1:<port>` +
`boot_path()`. The device answers that itself (the box never sees it):
`200` with `Set-Cookie: __pbm=<key>; HttpOnly; SameSite=Strict; Path=/`,
`Cache-Control: no-store`, and a page that does `location.replace("/")`
(meta refresh + script; `boot_path_to` lands on a validated same-origin
path instead). That is a same-origin navigation, so the Strict
cookie rides along — an HTTP 302 would inherit the boot load's cross-site
initiator (Tauri origin → 127.0.0.1) and the Strict cookie could be
withheld; `replace` also drops the key from history. Every other
connection's first request head (HTTP or WebSocket upgrade, ≤16 KiB, 3 s,
at most 64 connections being checked at once) must carry a `Cookie` header
with `__pbm=<key>` (constant-time compare) or the connection is dropped
unanswered; a wrong boot key is dropped too. That head is forwarded without
the `__pbm` cookie (the Peckboard box also drops it from every later
request). Only the first request of a keep-alive connection is checked.
Cookies are scoped by host, not port, so every box shares one `__pbm`
slot: give each box its own gate, boot the WebView through it on every box
switch, and replace it (and re-boot) whenever the port may have been
exposed.

Mobile shape: on connect `bind_listener(Prefer(127.0.0.1:<box port>))` into
an `Arc`, then `tokio::spawn(run_device(opts, l.clone(), token.clone(), ..))`
with a fresh `CookieGate`; on background `token.cancel()` and await the
task, keeping `l` bound; on foreground the same with the same `l` and a new
gate. `peckboard-connect` runs the same loop (`--gate` turns the cookie gate
on; off by default) and maps `DeviceEvent`s to its terminal messages.

### Tunnel Wire Format

Wire: QUIC (quinn 0.11) over the punched socket; box = server, device =
client. TLS 1.3, ALPN `peckboard-tunnel/1`, mutual auth: each side presents
a self-signed cert for an Ed25519 key HKDF'd from `S` under its own label
(`tunnel-ed25519/box`, `tunnel-ed25519/device` — distinct from the relay
auth and E2E keys), and the peer's custom verifier checks the TLS 1.3
CertificateVerify signature against the expected derived key (no CA). Each
bidi stream = one TCP connection: 1 type byte (`0x01` = forward to the
box's target) then raw bytes, half-close propagated; any other type is
reset and never dialled — the target is box-side config only. Liveness:
the device opens one `0x02` stream and writes a byte every 5 s, the box
echoes it; 15 s without a pong (device) or ping (box) drops the tunnel, so
a box that dies without closing is noticed in ~15 s. A peer that predates
`0x02` resets it / never opens it, and the other side falls back to the
QUIC idle timeout (60 s; keep-alive 15 s). Reconnect = new `establish`.

Try it without Peckboard:

```bash
cargo run --manifest-path peckboard-relay/Cargo.toml --features tunnel \
  --example box_forward -- 8000          # prints a pairing link
# Hand the link over on stdin or via the environment, not as an argument
# (argv is visible in the process list; peckboard-connect warns if you do).
cargo run --manifest-path peckboard-connect/Cargo.toml -- - < link.txt
PECKBOARD_LINK='<link>' cargo run --manifest-path peckboard-connect/Cargo.toml
# --save stores it (0600) so later runs need no link at all.
```

## Relay Fallback

When no punch round gets through (typically symmetric NAT × mobile CGNAT),
the tunnel runs **through the relay** instead (Tailscale-DERP style). The
relay still cannot read it: the payload is the same pairing-pinned QUIC,
and the relay only forwards its ciphertext.

- **Versioning**: protocol v2 = v1 + two frames, `ClientMsg::Data` (`0x10`)
  and `ServerMsg::Data` (`0x90`), each the type byte + one raw datagram
  (≤ 2048 B). Negotiated by ALPN at the TLS handshake: v2 clients offer
  `[peckrelay/2, peckrelay/1]`, the relay prefers `peckrelay/2`. An old
  relay picks v1, an old client never offers v2; v2 frames only ever flow
  on v2 sessions, so every old/new combination keeps working direct-only.
- **Transport**: the existing authenticated TLS session (no second
  connection): auth and slot ownership are already proven, a re-registering
  peer still replaces the old session (and with it the old relayed path),
  and each peer keeps one TCP connection (per-IP connection caps unchanged).
  Signaling and data have separate queues; signaling always goes first and
  a full data queue drops datagrams instead of blocking (UDP semantics —
  QUIC's congestion control backs off).
- **Switch**: once both peers are online, each v2 peer sends the other a
  sealed `RELAY_CAP` message. After `RelayFallback::after_failures` (2)
  failed rounds, a side whose peer is capable sends a sealed `RELAY_GO` and
  both return `PunchedPath { socket: PathSocket::Relayed(..) }`. The relay
  session stays open (it carries the data); QUIC runs over a quinn
  `AsyncUdpSocket` on the data channel, `serve_box` / `connect_device` /
  `run_device` unchanged.
- **Upgrade**: while relayed, the box asks for another punch round after
  30 s, doubling to 10 min (`RelayFallback::upgrade_every`; within the
  relay's 8-round budget per registration). Both sides punch from the
  socket `establish` used; on success that socket also sends directly. A
  path counts as direct while direct packets keep arriving (12 s); until
  the first one, datagrams go both ways at once; when they stop, back to
  the relay. QUIC never notices (its peer address is a fixed placeholder).
- **Status**: `PunchedPath::kind()`, `TunnelEvent`/`DeviceEvent::Connected
{ path }` and `::PathChanged { path }` with `PathKind::{Direct, Relayed}`.
  The box shows `path: direct|relayed` in device status; the app shows
  "Relayed … end-to-end encrypted" on its connect screen.
- **Relay side**: no disk state and no content logging — only aggregate
  counters (`Relay::relay_stats`, logged every 30 s when they change).
  Limits (flags; env `PECKRELAY_*` in `/etc/peckboard-relay.env`):

  | Flag                   | Default   | What                                       |
  | ---------------------- | --------- | ------------------------------------------ |
  | `--relay-rate-per-id`  | 512 KiB/s | bytes/s per rendezvous id, both directions |
  | `--relay-burst-per-id` | 2 MiB     | burst per id                               |
  | `--relay-rate-per-ip`  | 1 MiB/s   | bytes/s per sending IP (v6 /64, /56, /48)  |
  | `--relay-burst-per-ip` | 4 MiB     | burst per IP                               |
  | `--relay-rate-global`  | 50 MiB/s  | bytes/s across the relay                   |
  | `--relay-burst-global` | 100 MiB   | global burst                               |
  | `--relay-max-pairs`    | 1000      | ids relaying at once (fair-share, below)   |
  | `--relay-idle-secs`    | 60        | idle id frees its pair slot                |
  | `--no-relay-data`      | off       | disable; the relay stops offering v2       |

  Over-limit datagrams are dropped silently. The protocol version a relay
  offers is visible in its ALPN list (no other new surface).

## Registration Gate

Each box has a permanent Ed25519 identity (`identity::BoxIdentity`,
separate from the pairing keys). Protocol v3 (ALPN `peckrelay/3`, offered
as `[v3, v2, v1]`) lets a box answer the challenge with `IdentityAuth`:
the pairing signature plus its identity key and a signature over
`"peckrelay/3 box-identity" ‖ key ‖ nonce ‖ rendezvous id ‖ TLS exporter`
(single-session, like the pairing proof). Every v3 session then gets one
`IdentityStatus { registered, gated }` right after the first `Registered`.
v1/v2 clients never offer v3 and never see the new frames; v3 clients on an
older relay negotiate v2 and report no status.

- `--registration-gate` (`PECKRELAY_REGISTRATION_GATE`, default **off**):
  only an id whose box slot proved a registered identity may use the relay
  data channel; others are dropped like a full relay (`dropped_limit`).
  Rendezvous, forwarding, punching and STUN never consult the registry.
  A box told `registered: false, gated: true` doesn't offer the relay
  fallback, so a hopeless punch fails as `PunchFailed`. Old boxes (no
  identity) count as unregistered.
- `--registration-pow-bits` (`PECKRELAY_REGISTRATION_POW_BITS`, default 18).
- Registry: `<state-dir>/registered-boxes.txt`, one
  `<base64url key> <registered_at unix secs>` per line, written atomically
  (0600). A running relay re-checks its mtime every 30 s (edits/revokes take
  effect without a restart; a file that fails to parse keeps the last good
  set). Admin: `peckboard-relay registry list|add <key>|revoke <key>
[--state-dir DIR]` — run it as the service user (`sudo -u peckrelay`) so
  the file stays readable by the relay.
- Registration page, on the same :443 listener for TLS clients that
  negotiate `http/1.1` or no ALPN (one request per connection, per-IP rate
  limited): `GET /register` (static page; box key in the URL fragment,
  proof of work `sha256("peckrelay-register:<nonce>:<key>:<n>")` solved in
  the browser with WebCrypto), `GET /api/register/challenge`,
  `POST /api/register` (form `key`, `nonce`, `solution`), and
  `GET /api/registered?key=` → `{"registered":bool}`. Everything else is 404. Registration works with the gate off, so boxes can pre-register.

## Non-Discoverability

- Unknown id, bad signature, wrong pubkey, malformed Hello/Auth all get the
  exact sequence a legitimate peer with no partner online gets (Challenge →
  Registered at the same fixed delay → Pong), and nothing else.
- STUN replies only to requests whose MESSAGE-INTEGRITY verifies against a
  credential issued over TLS; everything else is silently dropped.
- No banners, no status/metrics/version endpoints; the only HTTP is the
  registration page above. Clients offering other ALPNs fail the TLS
  handshake.
- Logs never contain ids or secrets; client IPs are a salted 4-byte hash
  unless `--log-full-ips`.
- Admin is local only: flags, `SIGUSR1` (toggle debug logs), `SIGTERM`.

## Limits

Every per-address limit keys on the canonical client address: v4-mapped
(`::ffff:a.b.c.d`), 6to4 (`2002::/16`) and Teredo (`2001:0::/32`)
addresses fold to their embedded IPv4. IPv6 is limited per /64, /56 and /48
at once; the /56 gets 2× and the /48 4× the per-/64 budget
(`limits::V6_56_SCALE`, `V6_48_SCALE`), so a free /48 buys about as much as
one IPv4 address. Defaults live in `RelayConfig::default()` (flag where
listed).

| Limit                                                    | Default                                  |
| -------------------------------------------------------- | ---------------------------------------- |
| Connections, global (`--max-connections`)                | 8192                                     |
| Connections per IPv4 (`--max-connections-per-ip`)        | 256 (carrier NAT, see below)             |
| Connections per IPv6 /64 (`--max-connections-per-v6-64`) | 32 (/56: 64, /48: 128)                   |
| New connections per IP                                   | 2/s, burst 64; global 200/s, burst 1000  |
| STUN per source IP (checked first)                       | 5 pps, burst 40                          |
| STUN per credential                                      | 10 pps, burst 40                         |
| STUN global (valid username only)                        | 2000 pps, burst 5000                     |
| Signaling messages per session                           | 10/s, burst 40                           |
| Frames / blobs / datagrams                               | 8 KiB / 4 KiB / 2 KiB                    |
| Handshake / idle timeout                                 | 10 s / 90 s                              |
| Unpaired session lifetime                                | 30 min + 0-10 min jitter                 |
| Ids (`--max-ids`)                                        | 100,000; full ⇒ drop longest-idle id     |
| Id TTL, never paired / paired                            | 10 min / 24 h after the last peer leaves |
| New ids per IP                                           | burst 256, refill 256/h; global 100/s    |
| Queued bytes per session, signaling / data               | 32 KiB / 96 KiB (+64 B per frame)        |
| Queued bytes, relay-wide                                 | 256 MiB                                  |
| Minimum drain rate while backlogged                      | 4 KiB/s over 15 s; 10 s per frame        |
| Relay pair slots per IPv4 / per IPv6 /64                 | 32 / 8 (each pair counts once per end)   |

- **Eviction, not refusal**: when the table (or one address's cap) is full,
  a newcomer displaces the oldest handshaking or decoy session, then the
  oldest session whose peer is offline. Paired sessions are never evicted;
  if only those are left, the newcomer is refused. Sessions evicted but
  still tearing down may overshoot the cap by 1/8 (at least 16).
- **Unpaired lifetime**: a session with no peer online, decoy or not, is
  closed after 30-40 min and the client reconnects (the box after 2 s). The
  clock restarts whenever a peer leaves. Decoys get the identical treatment,
  so the lifetime reveals nothing.
- **Unpaired `Data`**: a v2 `Data` frame with no online peer costs a
  signaling token, counts as `dropped_no_peer`, and doesn't reset the idle
  timer. Only a paired session uses relay slots or bandwidth.
- **Slow readers**: frames past a session's byte budget are dropped. A
  session whose socket accepts less than the drain rate while it has a
  backlog is closed, and so is one that can't take a frame within 10 s.
- **Relay slots**: a pair takes a slot on its first datagram. When all
  1000 are held, it takes the slot of a pair whose busiest address holds
  strictly more slots than its own will, which shares slots max-min fairly
  between addresses. Otherwise its datagrams are dropped, with a retry at
  most once per second.
- **Carrier NAT**: one public IPv4 address can front a few hundred mobile
  subscribers, because carriers hand each one a block of 512-4096 ports.
  256 sessions per address lets all of them use Peckboard at once, and
  eviction keeps one subscriber's decoys from locking the others out.
- Ed25519 membership signatures are verified strictly (`verify_strict`, weak
  keys refused).

## Run Locally

```bash
cargo run --manifest-path peckboard-relay/Cargo.toml --features server -- \
  --dev-self-signed --state-dir "$(mktemp -d)" \
  --listen 127.0.0.1:24430 --stun-listen 127.0.0.1:24780
```

`--dev-self-signed` writes `dev-cert.der` into the state dir for clients
to pin (`ClientConfig::pinned`).

## Test

```bash
cargo test   --manifest-path peckboard-relay/Cargo.toml --all-features
cargo test   --manifest-path peckboard-relay/Cargo.toml   # default (client) only
cargo clippy --manifest-path peckboard-relay/Cargo.toml --all-features --all-targets
```

## Deploy

```bash
SSH_OPTS="-o StrictHostKeyChecking=yes -o UserKnownHostsFile=..." \
  peckboard-relay/deploy.sh admin@relay-host
```

Builds a static `x86_64-unknown-linux-musl` binary (`cargo zigbuild` if
available, else the repo's `tmp-musl-tools/x86_64-linux-musl-cross` gcc),
refuses anything not statically linked, uploads it plus
`deploy/peckboard-relay.service`, creates the `peckrelay` system user if
missing, installs to `/opt/peckboard-relay/peckboard-relay` (directory and
files root:peckrelay 0750, so the service user can run but never replace
its binary; the previous binary is kept as `peckboard-relay.prev`), and
restarts the service. The unit runs as `peckrelay` with only
`CAP_NET_BIND_SERVICE`, `NoNewPrivileges`, `ProtectSystem=strict`,
`PrivateTmp`, a syscall filter, `MemoryMax=2G`, and a 0700
`StateDirectory=/var/lib/peckrelay` holding the ACME cache (files 0600; its
only writable path). `IPAddressDeny` for private ranges is staged but
commented out until Infra confirms the source address LAN boxes arrive
from. Open 443/tcp and 3478/udp in the host firewall; DNS
`relay.peckboard.com` must point at the host before first start so Let's
Encrypt can validate via TLS-ALPN-01.

## Ops Runbook

Production host: `relay.peckboard.com` (208.98.54.37), Ubuntu 24.04, SSH
from the synload LAN only (`ssh relay-jump`, user `ops`, sudo). Below,
`R=relay-jump` and `SSH_OPTS` pins the host key file.

- **Deploy / update**: `SSH_OPTS=... peckboard-relay/deploy.sh $R`.
  Restarting drops live sessions; peers reconnect and re-register.
- **Rollback**: `SSH_OPTS=... peckboard-relay/deploy.sh $R --rollback` swaps
  in `peckboard-relay.prev` and restarts.
- **Status / logs**: `systemctl status peckboard-relay`,
  `journalctl -u peckboard-relay -f`. Logs carry no ids/secrets and only
  salted IP hashes. `systemctl kill -s USR1 peckboard-relay` toggles debug
  logging.
- **Runtime flags**: `/etc/peckboard-relay.env` (root, 0600), e.g.
  `PECKRELAY_ARGS=--acme-staging` or `PECKRELAY_ACME_CONTACT=ops@...`;
  `systemctl restart peckboard-relay` after editing.
- **Certificates**: issued and renewed in-process via TLS-ALPN-01 on :443
  (no cron, no certbot); renewal starts well before expiry while the service
  runs. Check: `openssl s_client -connect relay.peckboard.com:443 -alpn
peckrelay/1 </dev/null | openssl x509 -noout -issuer -dates`. Force
  re-issue: stop, delete `/var/lib/peckrelay/acme/cached_cert_*`, start
  (mind Let's Encrypt limits: 5 duplicate certs/week — try
  `--acme-staging` first). Staging and production caches are keyed apart.
- **Smoke test**: `cargo run --manifest-path peckboard-relay/Cargo.toml
--example live_check` — real box+device rendezvous, E2E messages, punch,
  and wrong-secret indistinguishability against the live relay.

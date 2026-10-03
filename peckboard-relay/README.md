# peckboard-relay

Handshake-only rendezvous server. It introduces a user's device to their
Peckboard box; after that, all traffic flows **directly** between the two
over UDP hole punching. The relay never carries user traffic.

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
connect`.

## Non-Discoverability

- Unknown id, bad signature, wrong pubkey, malformed Hello/Auth all get the
  exact sequence a legitimate peer with no partner online gets (Challenge →
  Registered at the same fixed delay → Pong), and nothing else.
- STUN replies only to requests whose MESSAGE-INTEGRITY verifies against a
  credential issued over TLS; everything else is silently dropped.
- No HTTP, no banners, no status/metrics/version endpoints. Clients offering
  other ALPNs fail the TLS handshake; no ALPN → bare close.
- Logs never contain ids or secrets; client IPs are a salted 4-byte hash
  unless `--log-full-ips`.
- Admin is local only: flags, `SIGUSR1` (toggle debug logs), `SIGTERM`.

## Limits

Per-IP (/64 for v6) and global connection-rate and STUN-rate token buckets,
max connections (global + per IP), 8 KiB frames / 4 KiB blobs, per-session
message rate, handshake (10 s) and idle (90 s) timeouts, capped id table
with TTL eviction. Defaults in `RelayConfig::default()`.

## Run Locally

```bash
cargo run --manifest-path peckboard-relay/Cargo.toml -- \
  --dev-self-signed --state-dir "$(mktemp -d)" \
  --listen 127.0.0.1:24430 --stun-listen 127.0.0.1:24780
```

`--dev-self-signed` writes `dev-cert.der` into the state dir for clients
to pin (`ClientConfig::pinned`).

## Test

```bash
cargo test  --manifest-path peckboard-relay/Cargo.toml
cargo clippy --manifest-path peckboard-relay/Cargo.toml --all-targets
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
missing, installs to `/opt/peckboard-relay/peckboard-relay` (root:peckrelay
0750; the previous binary is kept as `peckboard-relay.prev`), and restarts
the service. The unit runs as `peckrelay` with only `CAP_NET_BIND_SERVICE`,
`NoNewPrivileges`, `ProtectSystem=strict`, `PrivateTmp`, a syscall filter,
and a 0700 `StateDirectory=/var/lib/peckrelay` holding the ACME cache
(files 0600). Open 443/tcp and 3478/udp in the host firewall; DNS
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

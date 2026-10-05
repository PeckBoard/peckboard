---
title: Running Your Own Relay
parent: Remote Access
nav_order: 2
---

# Running Your Own Relay

The relay is a single static Linux binary, `peckboard-relay`, built from the `peckboard-relay/` directory of the repository. Running your own gives you a relay under your control with your own limits, and you can optionally require boxes to register before they may use the relayed fallback. This page covers what the host needs, building and installing with the bundled deploy script, pointing your boxes at it, and the registration gate. [Security and Encryption]({{ "/remote-access/security.html" | relative_url }}) describes what your relay will and won't be able to see, and how the bundled unit is hardened.

## Requirements

You need a Linux host with a public IP address and three things in place before first start:

- a DNS name, such as `relay.example.com`, pointing at the host;
- TCP port **443** reachable from the internet — the relay serves its encrypted signaling there and obtains its own Let's Encrypt certificate through the TLS-ALPN-01 challenge on the same port, so nothing else may listen on 443;
- UDP port **3478** reachable, for the relay's authenticated address discovery (STUN).

Relayed traffic rides the existing 443 connections, so no further ports are needed. Open both ports in the host firewall yourself; the deploy script does not touch it. No release ships a prebuilt relay binary.

## Build and Install

`peckboard-relay/deploy.sh` builds a static `x86_64-unknown-linux-musl` binary on your machine and installs it on the host over SSH. It needs the musl target (`rustup target add x86_64-unknown-linux-musl`) plus either [`cargo zigbuild`](https://github.com/rust-cross/cargo-zigbuild) or a musl cross compiler named in `CC_x86_64_unknown_linux_musl`, and the remote user needs `sudo`. It refuses to deploy a binary that isn't statically linked.

The bundled unit `peckboard-relay/deploy/peckboard-relay.service` names the official domain, so first change `--domain relay.peckboard.com` in its `ExecStart` line to your DNS name, then run:

```bash
SSH_OPTS="-o StrictHostKeyChecking=yes" peckboard-relay/deploy.sh admin@relay.example.com
```

The script creates a `peckrelay` system user if missing, installs the binary as `/opt/peckboard-relay/peckboard-relay` — owned by root, so the service can run it but never replace it — keeps the previous binary as `peckboard-relay.prev`, installs and enables the systemd unit, and restarts the service. `--no-restart` installs without restarting; `--rollback` swaps `peckboard-relay.prev` back in and restarts.

The unit runs the relay as `peckrelay` with only the capability to bind port 443, a read-only filesystem apart from its state directory, a system-call filter, and a 2 GB memory cap. Its state — the certificate cache and the registry of registered boxes — lives in `/var/lib/peckrelay`, which systemd creates with mode 0700 and passes to the relay as its `--state-dir`.

<details markdown="1">
<summary>Building without the deploy script</summary>

```bash
cargo build --release --features server --manifest-path peckboard-relay/Cargo.toml
```

Run the result with `--domain`, `--state-dir`, and, if wanted, `--acme-contact`. `--listen` (default `[::]:443`) and `--stun-listen` (default `[::]:3478`) set the two sockets; `--stun-public-port` advertises a different UDP port when a NAT in front of the host maps 3478 elsewhere. Add `--acme-staging` while testing to stay clear of Let's Encrypt's rate limits.

</details>

<details markdown="1">
<summary>Changing settings after install</summary>

The unit reads `/etc/peckboard-relay.env` (create it as root, mode 0600). Put the ACME contact address and any of the `PECKRELAY_*` variables below in it, plus extra command-line flags as `PECKRELAY_ARGS`, then `sudo systemctl restart peckboard-relay`:

```bash
PECKRELAY_ACME_CONTACT=ops@example.com
PECKRELAY_REGISTRATION_GATE=true
PECKRELAY_ARGS=--acme-staging
```

`deploy.sh` rewrites `/etc/systemd/system/peckboard-relay.service` on every run, so keep host-specific unit changes in the copy you deploy from, or in a drop-in made with `sudo systemctl edit peckboard-relay`. The unit also blocks private address ranges (`IPAddressDeny=`, inbound as well as outbound); if your boxes reach the relay over a LAN rather than through its public address, clear that line in a drop-in.

Logs: `journalctl -u peckboard-relay -f`. `sudo systemctl kill -s USR1 peckboard-relay` toggles debug logging. The certificate renews itself while the service runs.

</details>

## Point a Box at It

On each box, open Settings → Connections → **Remote Access**, replace the **Relay host** with your DNS name — add `:port` only if the relay isn't on 443 — and press **Save**. Devices paired earlier carry the old relay in their pairing link, so pair them again with **+ Pair device**.

The box checks the relay's certificate against the public certificate authorities, which is why the relay needs a real DNS name and its Let's Encrypt certificate. `--dev-self-signed` swaps in a throwaway self-signed certificate (written to `dev-cert.der` in the state directory) for local development and tests; boxes and the app will not connect to a relay running that way.

## Relayed Traffic Limits

The relayed fallback is on by default. Each limit is a flag, or the matching environment variable in `/etc/peckboard-relay.env`; over-limit packets are dropped and the encrypted connection slows down to fit.

| Flag / variable                                         | Default   | Limit                                        |
| ------------------------------------------------------- | --------- | -------------------------------------------- |
| `--relay-rate-per-id` / `PECKRELAY_RELAY_RATE_PER_ID`   | 512 KiB/s | per pairing, both directions together        |
| `--relay-burst-per-id` / `PECKRELAY_RELAY_BURST_PER_ID` | 2 MiB     | burst per pairing                            |
| `--relay-rate-per-ip` / `PECKRELAY_RELAY_RATE_PER_IP`   | 1 MiB/s   | per sending IP address (IPv6: per /64)       |
| `--relay-burst-per-ip` / `PECKRELAY_RELAY_BURST_PER_IP` | 4 MiB     | burst per IP address                         |
| `--relay-rate-global` / `PECKRELAY_RELAY_RATE_GLOBAL`   | 50 MiB/s  | across the whole relay                       |
| `--relay-burst-global` / `PECKRELAY_RELAY_BURST_GLOBAL` | 100 MiB   | global burst                                 |
| `--relay-max-pairs` / `PECKRELAY_RELAY_MAX_PAIRS`       | 1000      | pairings relaying at the same time           |
| `--relay-idle-secs` / `PECKRELAY_RELAY_IDLE_SECS`       | 60        | idle seconds before a pairing frees its slot |

Rates are bytes per second. `--no-relay-data` (`PECKRELAY_NO_RELAY_DATA=true`) turns the fallback off entirely for a rendezvous-only relay: boxes and devices still connect directly through it, and a pair that cannot connect directly fails instead of being relayed.

## Optional Registration Gate

**The gate is off by default.** On an ungated relay every box may use the relayed fallback without registering, and the box's Remote Access section shows a reminder that registration isn't required yet.

Start the relay with `--registration-gate` (`PECKRELAY_REGISTRATION_GATE=true`) and only registered boxes may use the relayed fallback; rendezvous, direct connections, and STUN stay open to all. Boxes register exactly as on the official relay: their **Register** button opens `https://<your relay>/register#<box key>`, a page your relay serves on port 443, where a browser proof-of-work check stands in for a captcha. `--registration-pow-bits` (`PECKRELAY_REGISTRATION_POW_BITS`, default 18, range 8–28) sets its difficulty; each extra bit doubles the work. The page works with the gate off too, so boxes can register ahead of switching it on.

Registered boxes are kept in `registered-boxes.txt` in the state directory, one public key and registration time per line. The running relay rereads the file within about 30 seconds of a change, so additions and revocations need no restart. Manage it with the `registry` subcommand, run as the service user so the file stays readable by the relay:

```bash
sudo -u peckrelay /opt/peckboard-relay/peckboard-relay registry list
sudo -u peckrelay /opt/peckboard-relay/peckboard-relay registry add <box-key>
sudo -u peckrelay /opt/peckboard-relay/peckboard-relay registry revoke <box-key>
```

`add` registers a box without the proof-of-work step. A box's key is the part after `#` in the registration link its **Register** button opens. All three default to `--state-dir /var/lib/peckrelay`; pass `--state-dir` if yours differs.

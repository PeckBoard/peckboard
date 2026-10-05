---
title: Security and Encryption
parent: Remote Access
nav_order: 3
---

# Security and Encryption

The relay behind [Remote Access]({{ "/remote-access.html" | relative_url }}) is built so that it never needs to be trusted with your data. Everything between your box and your devices is end-to-end encrypted with keys that come from the pairing secret, which the relay never receives. A relay operator, or an attacker who takes over the relay, can block connections and see who talks to whom and when, but cannot read or alter the traffic. This page explains how, for cautious users and for anyone running [their own relay]({{ "/remote-access/self-hosted-relay.html" | relative_url }}).

## The Pairing Secret

Every paired device gets its own random 32-byte _pairing secret_, created by the box and handed over only inside the QR code or `peckboard://pair/…` link. Both ends run it through HKDF-SHA256 to derive several independent values:

- a _rendezvous id_ — the name the two ends meet under at the relay, which reveals nothing about the secret;
- an Ed25519 key pair that proves to the relay a peer belongs to that pairing;
- an XChaCha20-Poly1305 key for the messages the two ends exchange through the relay;
- two further Ed25519 keys, one for the box and one for the device, that authenticate the encrypted tunnel itself.

The relay only ever sees the rendezvous id and the first public key. The secret, and every key derived from it, stays on the box and the device.

## Talking to the Relay

Boxes and devices connect to the relay over TLS 1.3 only, with a fixed set of application protocol names (ALPN); clients offering anything else fail the handshake. Boxes and the app check the relay's certificate against the public certificate authorities.

To join its pairing, a peer signs a random challenge from the relay together with its rendezvous id, its role, and a value exported from that very TLS session. Because the signature is bound to the session, a captured signature cannot be replayed on another connection. Signatures are verified strictly: weak keys and non-canonical signatures are refused.

**The relay does not reveal which pairings exist.** A connection with a malformed handshake, an unknown rendezvous id, the wrong key, or a bad signature is quietly turned into a _decoy_: it gets the same challenge, the same "registered" reply after the same fixed delay, the same responses to pings, and the same session lifetime as a genuine peer whose partner is offline — and nothing else. A prober cannot tell a wrong guess from a real pairing whose box is switched off. Address discovery over UDP (STUN) answers only requests carrying a valid short-lived credential issued over a TLS session, and the relay has no banners, status pages, or version endpoints; its only web page is the registration page.

Messages the two ends pass to each other through the relay — connection candidates, including the box's LAN addresses, and coordination messages — are sealed with XChaCha20-Poly1305 under the pairing key, bound to the rendezvous id and the sender's role so they cannot be reflected back or moved to another pairing. Each carries an encrypted counter and timestamp, and the receiver drops replays and anything more than 120 seconds out of step.

## The Tunnel

Once the relay has introduced them, the box and the device punch a direct UDP path and run QUIC over it. The TLS 1.3 handshake inside QUIC is mutually authenticated without any certificate authority: each side presents its pairing-derived Ed25519 key and checks that the other side's matches the one it expects. Only someone holding the pairing secret can complete it, and the session keys come from TLS 1.3's fresh key exchange. The box accepts the QUIC connection only from the address the punch reached, and the device cannot choose where the box forwards its traffic — only to PeckBoard's own web interface.

**When no direct path exists, the relayed fallback carries the same QUIC packets.** The relay forwards ciphertext it cannot open; the handshake and encryption are identical, so a relayed connection is exactly as private as a direct one. The **Relayed** badge tells you only which route the packets take.

Inside the tunnel, you still sign in to PeckBoard as usual: a pairing link gets a device to the sign-in page, not past it.

## Box Identity and Registration

Each box also has a permanent Ed25519 _identity key_, separate from any pairing and stored as `remote_access_identity` (mode 0600) in its data directory. A box proves it holds this key during the relay handshake with a signature bound to the TLS session, just like the pairing proof. Relays that use the registration gate let only registered identities use the relayed fallback; registering takes a browser proof-of-work check rather than an account. [Official Relay]({{ "/remote-access/official-relay.html#register-your-box" | relative_url }}) covers the steps.

## What the Relay Can and Cannot See

Being honest about the limits matters as much as the encryption. The relay **can** see:

- the public IP address and port of each box and device, and when they connect and disconnect;
- which two endpoints meet under each rendezvous id;
- the size and timing of relayed packets, and how much traffic each pairing relays;
- on relays that support registration, the box's identity key — which links all of one box's pairings together.

It **cannot** see the pairing secret or any key derived from it, the content of any message or tunnel packet, which pages you open or what you type, your LAN addresses, or your PeckBoard credentials.

An attacker who fully controls the relay gains the same view. They could refuse service, drop or delay packets, or point peers at wrong addresses — the connection then fails rather than reaching them, because they cannot complete the tunnel handshake. Over time they could also follow a phone's changing IP address. They could not decrypt past or present traffic, impersonate your box or a device, or recover a pairing secret.

## Hardening on the Relay Host

The relay keeps its state in memory: rendezvous ids, public keys, and session data vanish on restart, and peers simply re-register. It writes only two things to disk: the TLS certificate cache and, if used, the list of registered boxes, both owner-only in its state directory. Relayed packets are forwarded as-is and never stored or logged; the relay logs only aggregate counters. Logs carry neither rendezvous ids nor secrets, and client addresses appear as a salted 4-byte hash unless the operator passes `--log-full-ips`. Administration is local only — command-line flags, signals, and the `registry` subcommand; nothing administrative is reachable over the network.

The bundled systemd unit runs the relay as the dedicated `peckrelay` user, holding only the capability to bind port 443, with `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, private `/tmp` and devices, a system-call filter limited to the `@system-service` set, a 2 GB memory cap, and its state directory as the one writable path. The binary itself is owned by root, so a compromised relay process cannot replace it.

Every resource has a limit — connections and new sessions per address, the rate of address-discovery requests, signaling messages per session, queued bytes, and relayed bandwidth per pairing, per IP address, and overall — so one client cannot exhaust the relay for everyone else. [Running Your Own Relay]({{ "/remote-access/self-hosted-relay.html#relayed-traffic-limits" | relative_url }}) lists the bandwidth limits you can tune.

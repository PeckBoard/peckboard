---
title: Official Relay
parent: Remote Access
nav_order: 1
---

# Official Relay

`relay.peckboard.com` is the relay every box uses for [Remote Access]({{ "/remote-access.html" | relative_url }}) unless you point it elsewhere. It introduces your devices to your box and, when no direct path exists, forwards their encrypted traffic. Meeting through it and connecting directly is free and needs no account; the relayed fallback needs a one-time registration of your box, done in a browser in a few seconds.

## What the Relay Does and Sees

The relay has two jobs. As a _rendezvous_ server it tells the box and a device each other's public address so they can open a direct connection. As a _fallback_ it carries their traffic when both networks block a direct connection.

It cannot read either. Each pairing link holds a secret that the box creates and hands only to the device, through the QR code or the link itself. Both ends derive their encryption keys from that secret, and the relay never receives it, so whatever passes through the relay is ciphertext. The relay keeps no traffic on disk and logs no content, only aggregate counters; client addresses in its logs are a salted hash, not the IP itself. It does see connection metadata — addresses, timing, and traffic volume. [Security and Encryption]({{ "/remote-access/security.html" | relative_url }}) covers exactly what it can and cannot learn.

## Register Your Box

Since 2026-10-05 the official relay carries relayed traffic only for registered boxes. Registration ties a box's permanent identity key — created the first time you turn remote access on and stored as `remote_access_identity` in the data directory — to a list the relay keeps. There is no account, no email address, and no third-party captcha.

1. Turn remote access on in Settings → Connections → **Remote Access**. If the box isn't registered, the relay's registration page opens in a new tab automatically. Otherwise press the **Register** button next to **Not registered — relayed fallback unavailable**.
2. The page, **Register your PeckBoard box**, comes from the relay itself at `https://relay.peckboard.com/register#…`; the part after `#` is your box's public key. Press **Register**.
3. Your browser solves a short proof-of-work puzzle — "Checking you're a person, not a script (a few seconds)…" — then reports **Registered**, and you can close the tab.

Settings checks back on its own and switches to **Registered with relay.peckboard.com**. If you close the tab before finishing, press **Open again**.

Registration needs box version 0.1.64 or later; older boxes have no identity key and cannot register, so upgrade first.

<details markdown="1">
<summary>What changes if you don't register</summary>

Nothing changes for direct connections, rendezvous, or address discovery — those stay open to every box. Only the relayed fallback is refused, so a device on a network that blocks direct connections shows _Can't connect_ instead of falling back to the relay. Most home and office networks connect directly and never need the fallback.

</details>

## Fair-Use Limits

Relayed traffic is rate-limited per box, per IP address, and across the whole relay, so a single large transfer cannot crowd out everyone else. When a limit is reached, the relay drops the excess and the encrypted connection slows down to fit; it is not cut off. Direct connections never pass through the relay and are not affected.

If you need more relayed bandwidth than the shared relay offers, or want the relay under your own control, [run your own]({{ "/remote-access/self-hosted-relay.html" | relative_url }}).

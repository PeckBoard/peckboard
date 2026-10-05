---
title: Privacy Policy
nav_exclude: true
---

# Privacy Policy

_Last updated: October 5, 2026_

This policy covers the PeckBoard apps for iPhone, macOS, and Windows, and the
`relay.peckboard.com` service they use to reach your own PeckBoard server
("box").

## What the App Collects

Nothing. The app has no accounts, no analytics, no advertising, no tracking,
and no third-party SDKs. We do not collect, receive, sell, or share any data
about you.

## What Stays on Your Device

- **Pairing secrets** for the boxes you pair are stored in your device's
  secure storage (iOS Keychain, macOS Keychain, Windows Credential Manager).
- **App settings** (such as box names) are stored locally.

Removing a pairing or deleting the app removes this data.

## Where Your Traffic Goes

The app talks only to the boxes you pair and the relay configured on your box
(`relay.peckboard.com` unless you run your own). Connections are end-to-end
encrypted with keys derived from your pairing secret; the relay introduces
the two ends and, when a direct connection is impossible, forwards the
encrypted traffic without being able to read it. The relay holds no accounts
and keeps no record of your traffic's contents; like any internet service it
can observe connection metadata (IP addresses, connection times, traffic
volume). Details are in [Security and Encryption]({{ "/remote-access/security.html" | relative_url }}).

## Microphone

The app asks for microphone access only if you use the voice assistant. Audio
is streamed end-to-end encrypted to your own box for processing and is never
sent to us or to any third party.

## Your Content

Everything you do in PeckBoard — projects, sessions, messages, files — lives
on your own box, under your control. We never see it.

## Changes and Contact

Changes to this policy appear on this page with an updated date. Questions:
open an issue at [github.com/PeckBoard/peckboard](https://github.com/PeckBoard/peckboard/issues)
or see [Support]({{ "/support.html" | relative_url }}).

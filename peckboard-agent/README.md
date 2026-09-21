# peckboard-agent

The Peckboard remote-control daemon. It runs on your machine and dials
**home to Peckboard over an outbound WebSocket** (agent → server, never the
reverse), so it works from behind firewalls and NAT with no inbound ports
open. Once enrolled, a Peckboard session can drive the machine through the
capabilities you opt in to: run commands, manage servers, take screenshots,
and control the mouse and keyboard.

> **Security:** this daemon ships remote code execution and input control.
> Every capability except `echo` is **disabled by default** and must be
> opted in per machine. A global kill-switch stops all remote control
> instantly. Read [Capabilities & Permissions](#capabilities--permissions)
> before enabling anything.

> **⚠️ Unsigned binaries.** The released binaries are **not code-signed or
> notarized yet.** macOS Gatekeeper and Windows SmartScreen will warn or
> block on first run — see [Unsigned Binary Workarounds](#unsigned-binary-workarounds).
> Gatekeeper notarization and Windows Authenticode signing are a planned
> follow-up.

## Install

Download the binary for your platform from the Peckboard release page:

| Platform              | Asset                                |
| --------------------- | ------------------------------------ |
| macOS (Apple Silicon) | `peckboard-agent-macos-arm64`        |
| macOS (Intel)         | `peckboard-agent-macos-x86_64`       |
| Linux (x86_64)        | `peckboard-agent-linux-x86_64`       |
| Linux (arm64)         | `peckboard-agent-linux-arm64`        |
| Windows (x86_64)      | `peckboard-agent-windows-x86_64.exe` |

Each asset ships with a matching `.sha256`. Verify, then place it on your
`PATH`:

```sh
# macOS / Linux
sha256sum -c peckboard-agent-linux-x86_64.sha256   # (shasum -a 256 -c on macOS)
chmod +x peckboard-agent-linux-x86_64
sudo mv peckboard-agent-linux-x86_64 /usr/local/bin/peckboard-agent
```

Windows binaries link the CRT statically and macOS binaries are
self-contained — nothing to install. Linux binaries link glibc and libxcb
(the screenshot backend): preinstalled on desktop distros, and on minimal
servers `apt install libxcb1` (or the distro equivalent) satisfies it.

### Unsigned Binary Workarounds

**macOS** — Gatekeeper quarantines downloads from an unsigned developer.
Clear the quarantine attribute after verifying the checksum:

```sh
xattr -d com.apple.quarantine /usr/local/bin/peckboard-agent
```

(Or: System Settings → Privacy & Security → "Open Anyway" after the first
blocked launch.)

**Windows** — SmartScreen shows "Windows protected your PC" on first run.
Choose **More info → Run anyway**. To unblock from PowerShell:

```powershell
Unblock-File .\peckboard-agent-windows-x86_64.exe
```

## Enroll

In the Peckboard **Agents** panel, add a device and copy its one-time
enrollment token. Then, on the machine:

```sh
peckboard-agent enroll --server https://your-peckboard-host:3345 --token <TOKEN>
```

This writes the server URL and token to the local config and exits. The
token is a bearer secret sent on the WebSocket upgrade; the server stores
only its hash. Re-running `enroll` updates the server/token and preserves
your capability flags.

Config file location (per OS, via the `directories` crate):

| OS      | Path                                                                   |
| ------- | ---------------------------------------------------------------------- |
| Linux   | `~/.config/peckboard-agent/config.json` (or `$XDG_CONFIG_HOME`)        |
| macOS   | `~/Library/Application Support/board.Peck.peckboard-agent/config.json` |
| Windows | `%APPDATA%\Peck\peckboard-agent\config\config.json`                    |

## Run

```sh
peckboard-agent run
```

Connects to the enrolled server, advertises the capabilities you have
enabled, and serves requests until stopped. It auto-reconnects if the
connection drops. Set `RUST_LOG=peckboard_agent=debug` for verbose logs.

While a session is actively controlling the machine, the daemon surfaces a
visible "controlling this machine" indicator.

## Capabilities & Permissions

Capabilities are **deny-by-default.** Only `echo` runs out of the box; every
other capability must be turned on explicitly in `config.json`:

```jsonc
{
  "server_url": "https://your-peckboard-host:3345",
  "token": "…",
  "kill_switch": false, // set true to instantly refuse ALL capabilities
  "capabilities": {
    "echo": true,
    "terminal": false, // run commands
    "server": false, // start/stop/restart servers + logs + health
    "screenshot": false, // capture the screen
    "mouse": false, // move/click the pointer
    "keyboard": false, // synthesize keystrokes
  },
}
```

Restart `peckboard-agent run` after editing the config. Flipping
`kill_switch` to `true` is the one lever that refuses everything regardless
of the per-capability flags.

### OS Permission Grants

Some capabilities need the operating system to grant the daemon access.
Enabling the flag in `config.json` is necessary but **not sufficient** — the
OS gate must also be granted.

**macOS** (System Settings → Privacy & Security):

| Capability          | Grant required                                                                         |
| ------------------- | -------------------------------------------------------------------------------------- |
| `screenshot`        | **Screen Recording** — add the `peckboard-agent` binary (or its parent app / terminal) |
| `mouse`, `keyboard` | **Accessibility** — add the `peckboard-agent` binary                                   |

macOS prompts on first use; if you dismissed the prompt, add the binary
manually under the relevant section and restart the daemon. When running as
a launchd service, grant the permission to the binary at its installed path.

**Linux:**

- `screenshot`, `mouse`, `keyboard` need access to the graphical session.
  Under **X11** this works when the daemon runs in the user's session with
  `DISPLAY`/`XAUTHORITY` set. Under **Wayland**, screen capture and input
  injection are restricted by the compositor and may require portal
  permissions or `uinput` access — grant per your distro/compositor.

**Windows:**

- No per-capability system grant is required for `screenshot`, `mouse`, or
  `keyboard` when the daemon runs in the interactive desktop session. A
  daemon running in `session 0` (a bare Windows service) cannot see or drive
  the interactive desktop — run it in the logged-in user session for those
  capabilities.

## Running as a Service

Run the daemon in the background so it reconnects after reboots. Enroll
first, then install one of the following.

### Linux — systemd (user service)

Input/screen capabilities need the graphical session, so a **user** service
is usually right:

```ini
# ~/.config/systemd/user/peckboard-agent.service
[Unit]
Description=Peckboard remote-control agent
After=network-online.target

[Service]
ExecStart=%h/.local/bin/peckboard-agent run
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now peckboard-agent
loginctl enable-linger "$USER"   # keep the user service running after logout
journalctl --user -u peckboard-agent -f
```

For a headless `terminal`/`server`-only agent, a system service
(`/etc/systemd/system/peckboard-agent.service` with `User=`) also works.

### macOS — launchd (LaunchAgent)

A **LaunchAgent** (per-user, runs in the GUI session) is required for
screen/input capabilities:

```xml
<!-- ~/Library/LaunchAgents/board.peck.peckboard-agent.plist -->
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>board.peck.peckboard-agent</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/bin/peckboard-agent</string>
    <string>run</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/tmp/peckboard-agent.out.log</string>
  <key>StandardErrorPath</key><string>/tmp/peckboard-agent.err.log</string>
</dict>
</plist>
```

```sh
launchctl load ~/Library/LaunchAgents/board.peck.peckboard-agent.plist
launchctl start board.peck.peckboard-agent
```

Grant Screen Recording / Accessibility to `/usr/local/bin/peckboard-agent`
(see above) or the capabilities will be refused by macOS.

### Windows — service

Windows services run in `session 0` and **cannot drive the interactive
desktop**, so a service is appropriate only for `terminal`/`server`
capabilities. For screenshot/mouse/keyboard, run the agent in the logged-in
session instead (e.g. a Startup shortcut / Scheduled Task at logon).

Using the built-in Service Control Manager:

```powershell
sc.exe create peckboard-agent binPath= "\"C:\Program Files\peckboard-agent\peckboard-agent.exe\" run" start= auto
sc.exe start peckboard-agent
```

`sc.exe` expects a service application; a plain console binary may exit
under SCM. For a robust wrapper around a console binary, use
[NSSM](https://nssm.cc/):

```powershell
nssm install peckboard-agent "C:\Program Files\peckboard-agent\peckboard-agent.exe" run
nssm start peckboard-agent
```

For desktop-driving capabilities, prefer a **logon** Scheduled Task:

```powershell
schtasks /create /tn peckboard-agent /tr "\"C:\Program Files\peckboard-agent\peckboard-agent.exe\" run" /sc onlogon /rl highest
```

## Building From Source

```sh
# host target
cargo build --release --manifest-path peckboard-agent/Cargo.toml

# all release targets: CI builds on every main push
#   .github/workflows/build-agent.yml  (also workflow_dispatch)
#   release-promote.yml attaches the artifacts to each tagged release
# local cross-builds for installed targets:
peckboard-agent/build-release.sh x86_64-unknown-linux-gnu
```

The daemon is a standalone crate (not a workspace member); always build it
with `--manifest-path peckboard-agent/Cargo.toml`.

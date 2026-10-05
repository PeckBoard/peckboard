---
title: Remote Agent
nav_order: 11
---

# Remote Agent

`peckboard-agent` is a small daemon you run on another machine — a laptop, a build box, a Windows VM — so PeckBoard sessions can work on it. It dials out to your PeckBoard over a WebSocket, so the machine needs no open inbound ports, and it does nothing until you switch on each capability yourself. This page covers installing, enrolling, and running it, starting it at boot, and what to check when it misbehaves.

A _capability_ is one kind of action the agent will perform for a session: running a command, managing a configured server, taking a screenshot, or moving the mouse and typing. Once a capability is on, an agent working in a session can say "take a screenshot of the build box" and get one back.

## Download and Verify

Each [release](https://github.com/PeckBoard/peckboard/releases) carries one agent binary per platform, each with a matching `.sha256` file:

- `peckboard-agent-macos-arm64` / `peckboard-agent-macos-x86_64`
- `peckboard-agent-linux-x86_64` / `peckboard-agent-linux-arm64`
- `peckboard-agent-windows-x86_64.exe`

Download the binary and its checksum into the same folder, verify, and put the binary on your `PATH`:

```bash
sha256sum -c peckboard-agent-linux-x86_64.sha256      # macOS: shasum -a 256 -c …
chmod +x peckboard-agent-linux-x86_64
sudo mv peckboard-agent-linux-x86_64 /usr/local/bin/peckboard-agent
```

On Windows, compare the output of `Get-FileHash .\peckboard-agent-windows-x86_64.exe` with the hash in the `.sha256` file. Windows and macOS binaries need nothing else installed. Linux binaries need `libxcb1` for screenshots — present on desktop distributions; on a minimal server, `apt install libxcb1` or your distribution's equivalent.

<details markdown="1">
<summary>The binaries are not code-signed yet</summary>

macOS Gatekeeper and Windows SmartScreen warn about, or block, the agent on first run. After verifying the checksum, clear the macOS quarantine flag:

```bash
xattr -d com.apple.quarantine /usr/local/bin/peckboard-agent
```

or allow it under System Settings → Privacy & Security → **Open Anyway** after the first blocked launch. On Windows choose **More info → Run anyway** in the SmartScreen dialog, or unblock the file from PowerShell with `Unblock-File .\peckboard-agent-windows-x86_64.exe`.

</details>

## Enroll the Machine

Enrolling gives the agent its own token so PeckBoard knows which machine is calling. In PeckBoard, open **Agents** from the navigation rail and press **+ Enroll agent**. Enter a **Machine name** (for example "Work laptop"), pick its **Platform**, and press **Enroll**. The **Agent enrolled** dialog then shows an **Install command** with the token already filled in:

```bash
peckboard-agent enroll --server https://your-peckboard-host:3345 --token <TOKEN>
```

Run it on the machine, as the user the agent will run as — the configuration is stored per user. The token is shown only once and cannot be recovered; if it is lost, delete the agent from the list and enroll it again. Running `enroll` a second time replaces the server and token but keeps your capability settings.

The `--server` value is the address your browser used to open PeckBoard, copied from the page. The agent checks the server's HTTPS certificate against the public certificate authorities, so PeckBoard's default self-signed certificate is rejected. Either install a certificate from a public authority under Settings → Administration → TLS / HTTPS, or, on a network you trust, enroll against the plain-HTTP address (`http://your-peckboard-host:3344`) — the token then travels unencrypted.

## Run It

```bash
peckboard-agent run
```

The agent connects, tells PeckBoard which capabilities it will serve, and keeps running until stopped. It reconnects on its own if the link drops. The machine's row in the **Agents** list turns online, and shows how many requests are in flight. Set `RUST_LOG=peckboard_agent=debug` for verbose logs.

## Turn On Capabilities

Everything except `echo` — a harmless connectivity probe — is off until you enable it in the agent's `config.json`. `enroll` creates the file:

| OS      | Path                                                                   |
| ------- | ---------------------------------------------------------------------- |
| Linux   | `~/.config/peckboard-agent/config.json` (or under `$XDG_CONFIG_HOME`)  |
| macOS   | `~/Library/Application Support/board.Peck.peckboard-agent/config.json` |
| Windows | `%APPDATA%\Peck\peckboard-agent\config\config.json`                    |

Set a capability to `true` to allow it, then restart `peckboard-agent run`:

```jsonc
{
  "server_url": "https://your-peckboard-host:3345",
  "token": "…",
  "kill_switch": false,
  "capabilities": {
    "echo": true,
    "terminal": false, // run shell commands
    "server": false, // start/stop/restart/logs/health of servers listed below
    "screenshot": false, // capture the screen or one window
    "mouse": false, // move, click, drag, scroll
    "keyboard": false, // type text and key combos
  },
  "servers": {
    "web": {
      "command": "npm run dev",
      "cwd": "/home/me/site",
      "health_command": "curl -fsS http://localhost:5173",
    },
  },
}
```

The `server` capability can only start processes you name under `servers`; a session asks for `web`, never for an arbitrary command line. `cwd` and `health_command` are optional — without a health command, "healthy" means the process is still running.

**`kill_switch: true` refuses every capability, `echo` included, whatever the individual flags say.** It is the one setting that stops all remote control on the machine. From PeckBoard's side, **Disable** in the machine's menu in the **Agents** list has the same effect — the server drops the agent's connection within seconds and refuses it until you choose **Enable** — and **Delete** revokes its token permanently.

Every request the agent receives, including refused ones, is written to `audit.jsonl` next to `config.json`, and **Recent actions** in the machine's menu shows the same log in PeckBoard.

<details markdown="1">
<summary>Operating-system permissions for screenshots, mouse, and keyboard</summary>

Enabling the flag is necessary but not enough — the operating system has to allow it too.

On **macOS**, add the `peckboard-agent` binary under System Settings → Privacy & Security: **Screen Recording** for `screenshot`, **Accessibility** for `mouse` and `keyboard`. macOS asks on first use; if you dismissed the prompt, add the binary by hand and restart the agent.

On **Linux**, these capabilities need the graphical session. Under X11 they work when the agent runs inside your session with `DISPLAY` and `XAUTHORITY` set. Under Wayland the agent refuses mouse and keyboard outright, and screenshots go through the compositor's capture support or the desktop screenshot portal.

On **Windows**, no extra grant is needed as long as the agent runs in the logged-in desktop session. A Windows service runs in session 0, which cannot see or drive the desktop.

</details>

## What Sessions Can Do

Agents in your sessions reach enrolled machines through the `remote_agent_*` tools: `remote_agent_list` to see machines and their state, `remote_agent_run` to run a command (up to 600 seconds), `remote_agent_server` to start, stop, restart, or read logs and health of a configured server, `remote_agent_screenshot` to capture a monitor or a single window, and `remote_agent_mouse` and `remote_agent_keyboard` for input. Each call is refused unless the capability is enabled on the machine. Whenever mouse or keyboard input is synthesized, the agent logs a warning that `peckboard-agent` is controlling the machine and, where the desktop supports it, shows a notification at most every 10 seconds.

**Only one session controls a machine at a time.** Before acting, a session takes the machine's lock with `remote_agent_lock`. A lock lasts 30 seconds, every call tops it up to at least 15 seconds, and `remote_agent_unlock` releases it early. While another session holds it, other sessions are refused; a lock cannot be taken over while a command is still running.

## Start at Boot

Enroll first, then install the agent as a background service under the same user.

<details markdown="1">
<summary>Linux: systemd user service</summary>

Screen and input capabilities need the graphical session, so a user service is usually right:

```ini
# ~/.config/systemd/user/peckboard-agent.service
[Unit]
Description=Peckboard remote-control agent
After=network-online.target

[Service]
ExecStart=/usr/local/bin/peckboard-agent run
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now peckboard-agent
loginctl enable-linger "$USER"   # keep it running after you log out
journalctl --user -u peckboard-agent -f
```

A headless agent that only needs `terminal` and `server` can instead run as a system service (`/etc/systemd/system/peckboard-agent.service` with `User=`), enrolled as that user.

</details>

<details markdown="1">
<summary>macOS: launchd LaunchAgent</summary>

A per-user LaunchAgent runs in the GUI session, which screen and input capabilities need:

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

```bash
launchctl load ~/Library/LaunchAgents/board.peck.peckboard-agent.plist
launchctl start board.peck.peckboard-agent
```

Grant Screen Recording and Accessibility to `/usr/local/bin/peckboard-agent` itself, or macOS refuses those capabilities.

</details>

<details markdown="1">
<summary>Windows: logon task or service</summary>

For screenshots, mouse, and keyboard, start the agent in your logged-in session with a logon task:

```powershell
schtasks /create /tn peckboard-agent /tr "\"C:\Program Files\peckboard-agent\peckboard-agent.exe\" run" /sc onlogon /rl highest
```

A Windows service runs in session 0 and cannot drive the desktop, so use one only for `terminal` and `server`. The agent is a plain console program rather than a native service, so wrap it with [NSSM](https://nssm.cc/):

```powershell
nssm install peckboard-agent "C:\Program Files\peckboard-agent\peckboard-agent.exe" run
nssm start peckboard-agent
```

</details>

## Troubleshooting

**The machine shows offline.** Run `peckboard-agent run` in a terminal and read the log. `connection attempt failed` means it cannot reach the server or rejected its certificate — check the `--server` address and the certificate note under [Enroll the Machine](#enroll-the-machine). The agent retries on its own, waiting 1 second at first and backing off to 30 seconds, so a server restart or network blip heals without intervention. PeckBoard marks a silent agent offline after 90 seconds without traffic.

**It connects, then drops right away.** The machine is disabled or was deleted in the **Agents** list. Enable it, or delete it and enroll again. Running a second copy of the agent with the same token also replaces the first connection.

**A capability is refused.** Check its flag in `config.json`, that `kill_switch` is `false`, that you restarted the agent after editing, and — for screenshots and input — the operating-system permissions above. Calling `remote_agent_echo` from a session proves the whole path works without touching the machine.

**"this session does not hold the lock".** The session must call `remote_agent_lock` first. If another session holds the lock, it frees within 30 seconds of that session's last call, or as soon as it calls `remote_agent_unlock`.

To reach your PeckBoard itself from a phone or another computer, rather than drive a machine from it, see [Remote Access]({{ "/remote-access.html" | relative_url }}).

---
title: Getting Started
nav_order: 2
---

# Getting Started

PeckBoard is a single server binary that you run on your own machine and open in a browser, or with `--desktop` in a native window. This page covers the two ways to get that binary — downloading a prebuilt release or building from source — and the first launch: signing in and creating a project.

## Download a Release

Each tagged release on the [releases page](https://github.com/PeckBoard/peckboard/releases) carries one standalone binary per platform, named for the operating system and CPU:

- `peckboard-linux-x86_64` / `peckboard-linux-arm64` — CLI/server, no WebKitGTK runtime dep
- `peckboard-linux-x86_64-desktop` / `peckboard-linux-arm64-desktop` — same binary plus a native window (`--desktop`); needs `libwebkit2gtk-4.1-0` at runtime
- `peckboard-macos-x86_64` / `peckboard-macos-arm64` — server + desktop (system WebView)
- `peckboard-windows-x86_64.exe` / `peckboard-windows-arm64.exe` — server + desktop (WebView2)

Download the file for your machine. On Linux and macOS it is a bare executable, so mark it as one and run it; on Windows, run the `.exe` directly:

```bash
chmod +x peckboard-macos-arm64
./peckboard-macos-arm64
```

The web interface, database, and TLS certificate generator are all inside the binary, so there is nothing else to install to run it as a server. Linux `--desktop` needs the WebKitGTK shared library (`libwebkit2gtk-4.1-0` on Debian/Ubuntu). Running agents needs a provider. The setup wizard installs and signs in to the Claude Code, Codex, Cursor, Grok, and Kimi CLIs for you, on your own subscription; Ollama connects to an Ollama server you point it at. The [Providers]({{ "/providers.html" | relative_url }}) page lists every supported CLI and its sign-in flow.

## Download the Remote Agent

The optional **peckboard-agent** daemon runs on another machine and dials home to your PeckBoard over an outbound WebSocket, so sessions can drive that machine — commands, servers, screenshots, mouse and keyboard — without opening inbound ports. The same [releases page](https://github.com/PeckBoard/peckboard/releases) ships one agent binary per platform, named `peckboard-agent-<os>-<cpu>`. Every capability except a connectivity probe is off until you enable it on the machine. [Remote Agent]({{ "/remote-agent.html" | relative_url }}) walks through verifying, enrolling, permissions, and starting it at boot.

To reach PeckBoard itself from your phone or another computer, see [Remote Access]({{ "/remote-access.html" | relative_url }}).

## Build from Source

A source build needs a stable [Rust](https://rustup.rs/) toolchain and [Node.js](https://nodejs.org/). The frontend is built first because the Rust compiler embeds its output into the binary; `scripts/build.sh` runs both steps in order:

```bash
git clone https://github.com/PeckBoard/peckboard.git
cd peckboard
./scripts/build.sh
./target/release/peckboard
```

<details markdown="1">
<summary>Step-by-step source build</summary>

The script is equivalent to running the two builds yourself:

```bash
cd web
npm install        # one-time dependency install
npm run build      # writes web/dist/
cd ..
cargo build --release
```

The frontend build must come first: `cargo build --release` embeds whatever is in `web/dist/` at that moment, and `web/dist/` is not checked into git, so skipping the step produces a binary that serves a blank page. The first compile takes a while because SQLite is compiled from source; later builds are incremental. The finished binary at `target/release/peckboard` is self-contained — the machine that runs it does not need Node.js.

</details>

## First Launch

On its first start the server creates an admin account and prints the username and password to the terminal in a banner. Copy them — they are shown only once. If they are lost, run the binary again with `--reset-password` to print a new password; the [Configuration]({{ "/configuration.html" | relative_url }}) page covers accounts, ports, and the data directory in detail.

Open `http://localhost:3344` in a browser and sign in with the printed credentials. `peckboard --desktop` opens a native window on that same URL instead; a first-run desktop launch signs in automatically so the setup wizard appears without a terminal. Linux users who want a launcher icon can run `peckboard --install-desktop-entry`. The server also listens for HTTPS on port `3345` with a self-signed certificate covering `localhost`, the loopback addresses, and every address this machine answers on — browsers still warn on the first HTTPS visit for a self-signed cert (only uploading your own certificate under Settings → Administration → TLS / HTTPS removes that warning); plain HTTP is fine for a first look.

Signing in for the first time opens a short setup wizard with four steps:

1. **Welcome**: replace the printed one-time password with your own.
2. **Connect your AI**: install the AI tool you have a plan for (Claude Code, Codex, Cursor, Grok or Kimi Code) and sign in, one click each. A default model is picked for you.
3. **Where files live**: accept the ready-made `PeckBoard` folder in your home directory, or choose another. The AI can read and change files there.
4. **You're ready**: start a chat, create a project, or open a short getting-started checklist.

The wizard only appears once, on a fresh install, and everything it sets can be changed later in Settings. The checklist stays on the Sessions page until you finish or hide it, and can be reopened from the menu under your avatar. Using PeckBoard from a phone or another computer is set up separately, under Settings → Secure connection (HTTPS).

A good next step is a project. A _project_ pairs a folder on your machine with a board of _cards_: tasks that PeckBoard's agents pick up and complete. Press **+ New project**, give it a name and the folder it should work in. The simple Task workflow is already selected. Then add a card and press **Add and start**, and the board appears:

![A project board with cards in Backlog, In Progress, Review, and Done columns]({{ "/assets/screenshots/board.png" | relative_url }})

Each column is a step in the project's workflow, and agents move cards across the board as they finish them. [Core Concepts]({{ "/core-concepts.html" | relative_url }}) explains how cards, workers, and sessions fit together.

## Using the Assistant

Press **Listen** — the microphone near the bottom of the navigation rail — allow microphone access, and talk: "what's running?", "open the storefront project". Listening needs Chrome, Edge, or Safari (including iPhone and iPad); Firefox has no speech recognition. Browsers only grant the microphone on a secure page, so from another device use the HTTPS port (`3345`) rather than plain HTTP. The natural Kokoro voice downloads to the server the first time the panel opens; the browser's own voice speaks meanwhile. [Feature Tour]({{ "/features.html#voice-assistant" | relative_url }}) covers everything it can do.

---
title: Getting Started
nav_order: 2
---

# Getting Started

PeckBoard is a single server binary that you run on your own machine and open in a browser. This page covers the two ways to get that binary — downloading a prebuilt release or building from source — and the first launch: signing in and creating a project.

## Download a Release

Each tagged release on the [releases page](https://github.com/PeckBoard/peckboard/releases) carries one standalone binary per platform, named for the operating system and CPU:

- `peckboard-linux-x86_64`
- `peckboard-linux-arm64`
- `peckboard-macos-x86_64`
- `peckboard-macos-arm64`
- `peckboard-windows-x86_64.exe`
- `peckboard-windows-arm64.exe`

Download the file for your machine. On Linux and macOS it is a bare executable, so mark it as one and run it; on Windows, run the `.exe` directly:

```bash
chmod +x peckboard-macos-arm64
./peckboard-macos-arm64
```

The web interface, database, and TLS certificate generator are all inside the binary, so there is nothing else to install. Running agents on Claude models needs the Claude Code CLI — `claude` installed and signed in on the same machine; the Grok, Kimi, and Cursor providers sign in from Settings, Ollama connects to an Ollama server you point it at, and the built-in mock models work with nothing installed at all.

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

Open `http://localhost:3344` in a browser and sign in with the printed credentials. The server also listens for HTTPS on port `3345` with a self-signed certificate, so browsers warn on the first HTTPS visit; plain HTTP is fine for a first look.

The last step is to create a project. A _project_ pairs a folder on your machine with a board of _cards_ — tasks that PeckBoard's agents pick up and complete. Press **+ New project**, give it a name, the folder it should work in, and a workflow for its cards, and the board appears:

![A project board with cards in Backlog, In Progress, Review, and Done columns]({{ "/assets/screenshots/board.png" | relative_url }})

Each column is a step in the project's workflow, and agents move cards across the board as they finish them. [Core Concepts]({{ "/core-concepts.html" | relative_url }}) explains how cards, workers, and experts fit together.

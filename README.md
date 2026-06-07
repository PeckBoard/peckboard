# Peckboard

A remote control panel for [Claude Code](https://claude.com/claude-code). Peckboard spawns and manages Claude CLI child processes, exposes them through a mobile-friendly web UI, and orchestrates multi-agent workflows on a kanban board.

Ships as a **single executable binary** — frontend assets, database migrations, and TLS certs are all embedded or generated at runtime. Drop the binary on a host, run it, point a browser at it.

## What's in the box

- **Pluggable agent providers** — sessions are driven by any registered `AgentProvider`; built-ins are the real Claude CLI (`claude:*` model ids) and a scripted `mock:*` provider for tests and offline dev
- **Sessions** — spawn agent subprocesses with streaming JSON output; resume, interrupt, replay
- **Kanban board** — cards flow through workflow steps, each step driven by a dedicated worker session (one session per card for life, via `--resume`)
- **Real-time UI** — Axum WebSocket server streams events; clients reconnect with `resume-from-seq`
- **Auth** — multi-user, Argon2 password hashing, JWT with server-side session storage and revocation
- **Push notifications** — VAPID/web-push on session and worker events
- **TLS** — self-signed ECDSA cert auto-generated and rotated; user-provided certs supported
- **mDNS** — advertise as `<name>.local` for LAN discovery
- **Plugins** — Extism WASM sandbox with `*.before` / `*.after` / `*.failed` hooks on every operation
- **MCP server** — per-worker stdio subprocess exposing `complete_step`, `create_card`, `finish_card`, `ask_user`, `write_report`, etc.

See `docs/architecture/overview.md` for the full design.

## Requirements

- **Rust** — stable toolchain (2024 edition). Install via [rustup](https://rustup.rs):
  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  . "$HOME/.cargo/env"
  ```
- **Node.js + npm** — for building the frontend (`web/`)
- **Claude CLI** — required if you want to actually drive sessions with Claude. Install it and make sure `claude` is on `PATH`, or set `claudeBinary` in config. Not needed for dev or e2e runs that use the `mock:*` provider.
- **Build toolchain** — needed because `libsqlite3-sys` compiles SQLite from source:
  - Debian/Ubuntu: `sudo apt install build-essential pkg-config`
  - Fedora: `sudo dnf install gcc gcc-c++ make pkgconf-pkg-config`
  - macOS: Xcode Command Line Tools (`xcode-select --install`)

No OpenSSL needed — Peckboard uses `rustls`.

## Build

The release binary embeds the compiled frontend from `web/dist/`, so build the frontend first:

```bash
# 1. Frontend
cd web
npm install
npm run build
cd ..

# 2. Binary
cargo build --release
```

The resulting binary is `target/release/peckboard`.

## Run

```bash
./target/release/peckboard
```

On first launch Peckboard will:
1. Create the data directory (default `~/.peckboard/`)
2. Run embedded Diesel migrations against `peckboard.db`
3. Generate a self-signed TLS cert under `certs/`
4. Print an mDNS hostname (`<name>.local`)
5. Show the registration page on the web UI — the first user becomes admin

Browse to:
- `http://localhost:3333` — HTTP
- `https://localhost:3345` — HTTPS (accept the self-signed cert)
- `https://<name>.local:3345` — from any device on the LAN

### Common flags

```bash
peckboard --port 8080 --https-port 8443
peckboard --data-dir ./tmp-data        # throwaway profile
peckboard --reset-password              # reset the single user's password, print it, exit
peckboard --reset-password --user alice # reset a specific user (required when >1 user exists)
peckboard --reset-mdns-name             # regenerate mDNS hostname
peckboard --log-level debug
peckboard --json                        # JSON log output
```

Full list of args, env vars, and `config.json` keys: `docs/architecture/config.md`.

## Develop

One command starts both processes:

```bash
./scripts/dev.sh
```

This runs the Axum backend on `http://localhost:3333` (debug build, incremental — first start is slow, subsequent restarts are fast) and the Vite dev server on `http://localhost:5173` with HMR. Browse to **`http://localhost:5173`**: Vite proxies `/api/*` and `/ws` to the backend so the two behave as one app.

Frontend edits hot-swap instantly. Backend edits require a manual restart — install `cargo-watch` if you want auto-restart:

```bash
cargo install cargo-watch
cargo watch -x run
```

If you'd rather run the two by hand:

```bash
# Terminal 1
cargo run

# Terminal 2
cd web && npm run dev
```

You only need `cargo build --release` for shipping the single embedded binary or for running the Playwright e2e suite (which boots the release binary). Edit React under `web/src/`, edit Rust under `src/`.

### Project layout

```
src/
  main.rs           entry point
  config.rs         CLI args + config.json
  db/               Diesel schema, models, CRUD
  auth/             JWT, rate limiting, password hashing
  routes/           HTTP API handlers
  ws/               WebSocket broadcaster
  provider/
    agent.rs        AgentProvider trait + shared event-emit helper
    manager.rs      provider-agnostic dispatcher (picks backend by model prefix)
    registry.rs     registry of registered providers + model metadata
    claude/         Claude CLI provider (process spawn + stream-json parser)
    mock/           scripted mock provider for tests and offline dev
  worker/           kanban worker orchestrator, watchdog
  service/          push, TLS, mDNS, wake-lock, MCP server
  plugin/           Extism plugin manager + hook dispatcher
  frontend.rs       rust-embed of web/dist/

web/                React + Vite + Zustand SPA

migrations/         Diesel migrations (embedded into binary)

docs/architecture/  design docs — read these before changing things
docs/api/           HTTP and WebSocket API contracts
docs/frontend/      frontend design notes
docs/tasks/         active task scratchpads
```

## Data directory

Everything Peckboard writes lives under `--data-dir` (default `~/.peckboard/`):

```
~/.peckboard/
  peckboard.db        SQLite — sessions, projects, cards, events, auth, subscriptions
  config.json         persisted config (CLI args > env > this file > defaults)
  certs/              self-signed TLS cert + key (0o600)
  worker-mcp/         per-session MCP config JSON consumed by the Claude CLI
  reports/            markdown reports + attachments organized by date
  attachments/        per-session uploads
  plugins/            drop .wasm plugin files here
```

## Tests, lint, format

Backend:

```bash
cargo test                              # unit + integration (incl. tests/mock_provider.rs)
cargo clippy --all-targets --no-deps    # lint
cargo fmt                               # format (or --check)
```

Frontend + e2e:

```bash
cd web
npm run lint                            # ESLint
npm run format                          # Prettier (or format:check)
npm run e2e:install                     # one-time: install Playwright browsers
npm run e2e                             # Playwright suite — boots the release binary
```

The Playwright `webServer` block boots `target/release/peckboard` against a fresh `mktemp -d` data dir, and the suite uses `mock:*` model ids so it doesn't depend on the real Claude CLI or an LLM bill. Diesel CRUD tests use in-memory SQLite; plugin tests bring up a real Extism sandbox.

See [`AGENTS.md`](./AGENTS.md) for the full "definition of done" cycle and the expectations around tests for new features.

## Documentation

- `docs/architecture/overview.md` — start here
- `docs/architecture/config.md` — every config knob
- `docs/architecture/auth-security.md` — auth model and JWT lifecycle
- `docs/architecture/event-log.md` — append-only event log (source of truth)
- `docs/architecture/worker-lifecycle.md` — kanban worker orchestration
- `docs/architecture/plugins.md` — WASM plugin contract and hook points
- `docs/architecture/mcp-tools.md` — MCP tools available to workers

## License

TBD.

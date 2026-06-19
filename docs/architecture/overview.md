# Architecture Overview

Peckboard is a remote control panel for Claude Code. It spawns and manages Claude CLI child processes, exposes them over a web UI, and orchestrates multi-agent workflows through a kanban board.

## Distribution

Peckboard is distributed as a **single executable binary**. No additional files, config, or runtime dependencies are required to run it. Everything is embedded at compile time:

- **Database migrations** — baked in via Diesel's `embed_migrations!()`
- **Frontend assets** — the compiled React SPA is embedded in the binary
- **TLS certificates** — generated at runtime via `rcgen` (no cert files shipped)
- **Default config** — sensible defaults compiled in; data directory created on first run

Download one file, run it.

## High-Level Components

### Backend (Rust / Axum)

- **HTTP API** — RESTful endpoints for sessions, projects, cards, reports, git, config, auth
- **WebSocket Server** — real-time event streaming to all connected clients; resume-from-seq on reconnect
- **SQLite Storage** — normalized schema managed by Diesel ORM with embedded migrations
- **Claude CLI Manager** — spawns `claude` child processes with `--output-format stream-json`, parses newline-delimited JSON, manages process lifecycle
- **Worker Orchestrator** — assigns cards to worker sessions, manages pipeline steps, handles crash recovery, money-loop defense
- **MCP Server** — stdio subprocess per worker session, exposes tools like `complete_step`, `create_card`, `finish_card`, `wont_do_card`, `ask_user`, `write_report`
- **Services** — push notifications (VAPID/web-push), TLS cert management (self-signed or user-provided), mDNS advertising, status line delegation, keep-awake

### Frontend (React + Zustand + Vite)

- **SPA** embedded in the binary and served by Axum
- **Zustand store** split into slices (auth, sessions, ws, projects, ui, reports, git, config)
- **WebSocket client** with auth handshake, resume-from-seq, exponential backoff reconnect
- **Event log renderer** — projects raw event stream into display items (messages, tool uses, step headers)
- **Mobile-first** — touch accommodations, viewport keyboard management, responsive layout

## Runtime Data

All persistent data lives under a configurable data directory (default `~/.peckboard/`):

- `peckboard.db` — SQLite database (sessions, projects, cards, events, auth, push subscriptions)
- `certs/` — self-signed TLS certificate and key (generated on first run)
- `worker-mcp/` — per-session MCP config JSON files consumed by the Claude CLI
- `reports/` — markdown reports and binary attachments organized by date folder
- `attachments/` — per-session user-uploaded files

## Key Design Principles

1. **Single binary** — everything embedded at compile time. No external files needed to run.
2. **Event log is the source of truth** — anything a client needs after refresh or the backend needs after restart lives in the event log. In-memory state is a cache populated from the log.
3. **Append, don't mutate** — events are append-only. State changes are new events; readers derive current state by walking the tail.
4. **One session per card for life** — a card's worker session survives step transitions via `--resume`. No crash spawns a fresh session.
5. **Money-loop defense** — consecutive crash counting with a hard block threshold prevents runaway API spend.
6. **Normalized schema** — proper relational tables managed by Diesel ORM, not JSON blobs. Migrations are embedded and run automatically on startup.

## Network Architecture

- HTTP on configurable port (default 3333)
- HTTPS on configurable port (default 3334) with auto-generated self-signed cert
- WebSocket upgrades on the same ports
- MCP subprocess calls back to HTTP on loopback (bearer-token-gated, no TLS verification needed)
- mDNS advertising for LAN discovery (`<name>.local`)
- Server binds `0.0.0.0` — all routes require auth except login/status endpoints

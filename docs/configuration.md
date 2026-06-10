---
title: Configuration
nav_order: 6
---

# Configuration

PeckBoard is configured entirely through command-line flags — there is no config file. Everything it stores lives in a single data directory, so a backup is a copy of one folder. This page covers the flags, how accounts work, how models are chosen, and where data lives.

## Command-Line Flags

Started with no flags, the server listens on HTTP port `3344` and HTTPS port `3345`, binds to all interfaces, and keeps its data in `~/.peckboard`. Each of those can be changed:

```bash
peckboard --port 8080 --https-port 8443 --data-dir /var/lib/peckboard
```

| Flag | Default | Description |
| --- | --- | --- |
| `--port` | `3344` | HTTP port |
| `--https-port` | `3345` | HTTPS port |
| `--host` | `0.0.0.0` | Bind address |
| `--data-dir` | `~/.peckboard` | Data directory; also settable via the `PECKBOARD_DATA_DIR` environment variable |
| `--reset-password` | — | Generate a new password for a user, print it, and exit; add `--user <name>` when more than one user exists |
| `--mdns` | off | Advertise the server on the local network via mDNS; also `PECKBOARD_MDNS=1` |

HTTPS uses a self-signed certificate that PeckBoard generates into the data directory on first start and renews automatically, so browsers warn on first visit. Set `RUST_LOG` (for example `RUST_LOG=debug`) to change log verbosity.

## Authentication

On its first start PeckBoard creates one admin account, named `admin`, with a random password, and prints both to the terminal in a banner. The password is shown only that once — save it. If it is lost, generate a new one:

```bash
peckboard --reset-password
# prints admin:<new-password> and exits
```

Sign in with those credentials in the web UI. There is no self-service registration: an admin creates further accounts from the user management page (the people icon in the navigation rail, visible to admins only), choosing a username, password, and a role of *user* or *admin*. Any signed-in user can change their own password from the avatar menu in the bottom-left; an admin can reset another user's password, which also signs that user out everywhere.

<details markdown="1">
<summary>Scripted setups: choosing the first account's credentials</summary>

Two environment variables override the first-run defaults, useful when provisioning a server non-interactively:

```bash
PECKBOARD_BOOTSTRAP_USERNAME=alice \
PECKBOARD_BOOTSTRAP_PASSWORD='<choose-a-strong-password>' \
peckboard
```

They take effect only on a first start with an empty database. The credential banner's last line is a machine-readable `username:password`, so `peckboard | tail -1` captures it in scripts.

</details>

## Models

Every conversation in PeckBoard — a *session* — runs on a model. A model id names a provider and a model joined by a colon: `claude:claude-opus-4-7` drives the Claude Code CLI with Opus 4.7, while `mock:happy-path` runs one of the built-in mock models.

A *mock model* is a fake agent: instead of talking to Claude, it replays a fixed script of events — some text, a tool call, a completion — identically on every run. Mock models exist for tests and demos. They let you try the UI, record a reproducible demo, or run the end-to-end test suite without the Claude CLI installed and at no API cost. For real work, pick a Claude model.

You choose a model when creating a session; the dropdown groups Claude and Mock models under separate headers, with *Server default* at the top. An existing session can be switched from the model button in the chat toolbar. Projects have their own model setting, used for the worker sessions they spawn, and a card can override its project's choice.

<details markdown="1">
<summary>Available models</summary>

The Claude provider lists these models (use as `claude:<id>`, or bare — a bare id defaults to the Claude provider):

| Id | Display name |
| --- | --- |
| `claude-fable-5` | Claude Fable 5 |
| `claude-opus-4-8` | Claude Opus 4.8 |
| `claude-opus-4-7` | Claude Opus 4.7 |
| `claude-opus-4-6` | Claude Opus 4.6 |
| `claude-sonnet-4-6` | Claude Sonnet 4.6 |
| `claude-haiku-4-5` | Claude Haiku 4.5 |

If any of the environment variables `ANTHROPIC_DEFAULT_OPUS_MODEL`, `ANTHROPIC_DEFAULT_SONNET_MODEL`, or `ANTHROPIC_DEFAULT_HAIKU_MODEL` is set to an Amazon Bedrock model ARN, that model is added to the list as well.

The mock provider offers one model per scripted scenario (use as `mock:<id>`):

| Id | Scenario |
| --- | --- |
| `echo` | Echoes your message back as a text event |
| `happy-path` | Text, a tool call, more text, then a clean completion |
| `tool-use` | A single tool call, then completion |
| `markdown` | Text exercising markdown rendering |
| `ask` | Asks you a question and waits for the answer |
| `crash` | Text, then a simulated agent crash |
| `tool-orphan-crash` | A tool call that never finishes, then a crash |

</details>

## Data Storage

All state lives in the data directory — `~/.peckboard` unless `--data-dir` or `PECKBOARD_DATA_DIR` says otherwise. The database is a single SQLite file at `<data-dir>/peckboard.db`; users, sessions, projects, cards, and event history are all in it. No external database or service is involved, so backing up an install means stopping the server and copying the directory.

<details markdown="1">
<summary>What else is in the data directory</summary>

| Path | Contents |
| --- | --- |
| `peckboard.db` | The SQLite database |
| `jwt_secret` | Signing secret for login tokens, generated on first start |
| `certs/` | Self-signed TLS certificate and key |
| `vapid_keys.json` | Keys for web push notifications |
| `attachments/` | Files uploaded to sessions |
| `reports/` | Markdown reports written by workers and experts |
| `worker-mcp/` | Per-session configuration consumed by the Claude CLI |
| `plugins/` | Drop-in directory for WASM plugins, empty by default |

</details>

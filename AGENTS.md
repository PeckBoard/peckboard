# Peckboard — Claude working notes

Project layout, tooling commands, and the workflow Claude should follow
when changing this repo.

## Stack

- **Backend**: Rust 2024, Axum, SQLite via Diesel, single embedded binary.
- **Frontend**: React 19 + TypeScript + Vite + Zustand. Lives in `web/`.
  Built assets in `web/dist/` are embedded into the release binary at
  compile time via `rust-embed`.
- **Agents**: pluggable via the `AgentProvider` trait in
  `src/provider/agent.rs`. Built-ins are `ClaudeProvider`
  (`src/provider/claude/`) and `MockProvider` (`src/provider/mock/`).
  Sessions pick a backend through the `provider:model` prefix on the
  model id — e.g. `claude:claude-opus-4-7`, `mock:happy-path`.

## Commands

Run from the repo root unless noted.

| What | Command |
| --- | --- |
| Build (debug) | `cargo build` |
| Build release | `cargo build --release` |
| Rust unit + integration tests | `cargo test` |
| Rust lint | `cargo clippy --all-targets --no-deps` |
| Rust format | `cargo fmt` (or `--check` to verify) |
| Web install | `cd web && npm install` |
| Web build | `cd web && npm run build` |
| Web lint | `cd web && npm run lint` |
| Web format | `cd web && npm run format` (or `format:check`) |
| Playwright e2e | `cd web && npm run e2e` |
| Playwright install (one-time) | `cd web && npm run e2e:install` |

The Playwright `webServer` block boots `target/release/peckboard` with a
fresh `mktemp -d` data dir, so each run starts from a clean state.

## Definition of done

**After making code changes, run the full verification cycle and fix
anything it surfaces before reporting done:**

1. `cargo fmt --check` — format clean
2. `cargo clippy --all-targets --no-deps` — no errors
3. `cargo test` — all unit + integration tests pass
4. `cd web && npm run lint` — no errors
5. `cd web && npm run format:check` — prettier clean
6. `cd web && npm run e2e` — Playwright suite green

If a step fails because of something *unrelated* to the current change
(pre-existing backlog), call it out explicitly rather than silently
ignoring it.

## Tests for new features

When adding a feature, add tests **proportional to the change** — enough
to lock in behaviour, not enough to slow future refactors. Default to:

- **One** Rust unit/integration test per non-trivial backend module or
  behaviour (e.g. a new provider scenario, a new route's happy path).
  Reach for `Db::in_memory()` + the public API rather than mocking.
- **One** Playwright e2e test per new user-visible flow, using
  `mock:*` model ids so the test is deterministic and doesn't depend on
  the real `claude` CLI. Reuse the `authenticate` + `collectEventsUntil`
  helpers in `web/e2e/tests/mock-provider.spec.ts`.

**Don't** add tests for every internal helper, every error branch, or
every UI permutation. If the behaviour is already covered by an
existing test at a higher level, leave it. Tests that only restate the
implementation are pure cost.

**Every feature change must be verified end-to-end with new e2e
tests before reporting done. The e2e tests should cover every flow of
the feature** — the entry point, the happy path, the meaningful state
transitions, and any failure mode a user can actually trigger. Unit
and integration tests can pass while the feature is still broken
behind the HTTP / WS / UI layers; the e2e tests are how we confirm the
new feature actually works for a user.

"Cover every flow" is not the same as "cover every permutation": one
test per distinct user-visible behaviour, not one per code branch. If
the change is a pure backend refactor with no user-visible surface,
say so explicitly instead.

## Conventions

- Model ids carry a provider prefix (`claude:`, `mock:`). Bare model
  strings default to `claude:` for backward compat with stored
  sessions/cards.
- Use the shared `crate::provider::agent::emit_event` helper for any
  new provider — it persists the event, updates `last_activity`, and
  broadcasts to subscribers in one place.
- New providers go in `src/provider/<name>/` and register themselves
  via a `register_<name>_provider(&registry)` function called from
  `main.rs`.

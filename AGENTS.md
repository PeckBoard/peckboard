# Peckboard — Claude Working Notes

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

| What                          | Command                                        |
| ----------------------------- | ---------------------------------------------- |
| Build (debug)                 | `cargo build`                                  |
| Build release                 | `cargo build --release`                        |
| Rust unit + integration tests | `cargo test`                                   |
| Rust lint                     | `cargo clippy --all-targets --no-deps`         |
| Rust format                   | `cargo fmt` (or `--check` to verify)           |
| Web install                   | `cd web && npm install`                        |
| Web build                     | `cd web && npm run build`                      |
| Web lint                      | `cd web && npm run lint`                       |
| Web format                    | `cd web && npm run format` (or `format:check`) |
| Playwright e2e                | `cd web && npm run e2e`                        |
| Playwright install (one-time) | `cd web && npm run e2e:install`                |

The Playwright `webServer` block boots `target/release/peckboard` with a
fresh `mktemp -d` data dir, so each run starts from a clean state.

## Running the Binary While Iterating

**Always launch ad-hoc / scratch runs against a fresh tmp data dir,
not the user's real install** — unless the user explicitly asks you
to use theirs:

```bash
./target/release/peckboard --data-dir "$(mktemp -d)"
```

The user's default data dir holds their real cards, sessions, and DB.
An in-development binary may write data the released version can't
read, or apply a migration that corrupts it. Default to a tmp dir
every time.

**Never blanket-kill `peckboard` processes.** The user may have their
own instance running alongside yours. If you need to stop a server
you started, track its PID — capture `$!` after backgrounding it, or
match on the `--data-dir` path you launched it with — and kill only
that one. Do not run `pkill peckboard` / `killall peckboard` /
`fuser -k <port>`; those will take down the user's session.

## Definition of Done

**After making code changes, run the full verification cycle and fix
anything it surfaces before reporting done:**

1. `cargo fmt --check` — format clean
2. `cargo clippy --all-targets --no-deps` — no errors
3. `cargo test` — all unit + integration tests pass
4. `cd web && npm run lint` — no errors
5. `cd web && npm run format:check` — prettier clean
6. `cd web && npm run e2e` — Playwright suite green

If a step fails because of something _unrelated_ to the current change
(pre-existing backlog), call it out explicitly rather than silently
ignoring it.

## Tests for New Features

Add tests **proportional to the change** — enough to lock in
behaviour, not enough to slow future refactors:

- **One** Rust unit/integration test per non-trivial backend module
  or behaviour (e.g. a new provider scenario, a new route's happy
  path). Reach for `Db::in_memory()` + the public API rather than
  mocking.
- **One** Playwright e2e test per new user-visible flow, using
  `mock:*` model ids so the test is deterministic and doesn't depend
  on the real `claude` CLI. Reuse the `authenticate` +
  `collectEventsUntil` helpers in `web/e2e/tests/mock-provider.spec.ts`.

Skip tests that only restate the implementation — every internal
helper, every error branch, every UI permutation. If a higher-level
test already covers it, leave it.

**Every feature change must be verified end-to-end with new e2e tests
before reporting done** — entry point, happy path, meaningful state
transitions, and any failure mode a user can actually trigger.
Unit/integration tests can pass while the feature is broken behind
the HTTP/WS/UI layers; e2e is how we confirm it works for a user.
That means one test per distinct user-visible behaviour, not one per
code branch. If the change is a pure backend refactor with no
user-visible surface, say so explicitly.

## Migrations — READ THIS BEFORE ADDING ONE

**Peckboard runs on real user data. Data loss is not acceptable. A
migration that drops a column, drops a table, alters types, or runs
incorrectly on existing DBs is a P0 incident.** Migrations have
already silently corrupted live databases once on this project.

### Think Hard Before Adding One — but Use Real Schema When You Do

**Migrations are unavoidable; treat the decision to add one as
weighty, not the way you express it.** Don't dodge a migration by
stuffing typed data into a JSON blob — you lose every
schema-on-write protection (types, NOT NULL, FKs, UNIQUE, indexes)
the moment data goes into a `TEXT` column, and bugs that schema
would have caught at write time turn into silent corruption.

Before reaching for ALTER / CREATE, ask:

- **Is this state actually durable?** Per-browser prefs (theme,
  layout) and per-session ephemera belong in localStorage /
  sessionStorage / a Zustand store, not the DB.
- **Can it be derived at query time?** Aggregations and "summary"
  fields are often cheaper to compute than to store and keep in sync.
- **Is the data shape really this volatile?** If one feature needs
  five migrations, the schema is wrong — redesign so the columns
  generalise (e.g. an event log with `kind` + typed payload tables,
  instead of bolting a new flag onto a wide row each release).

Once it's real, durable, structured state, **add a proper column or
table.** Use the right type. Add the constraint. Add the index. JSON
is only appropriate for genuinely free-form data the application
never queries against (e.g. opaque provider-event payloads in
`events.data`).

### When You Must Add One

Treat it like a one-way door — every shipped migration runs forever
on old DBs and can never be deleted. So:

- Make the change as small as possible. Don't bundle unrelated columns.
- **Never DROP** in a forward migration without an explicit
  conversation about acceptable data loss. Leave the obsolete column
  unused; remove it (if ever) much later.
- **Never change a column's type** in-place. Add a new column,
  backfill, switch reads, leave the old column.
- Backfill rows in the same migration when the new column is
  `NOT NULL` — otherwise existing rows blow up at write time.
- Provide a working `down.sql`. You will need it locally even if you
  never run it in production.

### Hard Rules

1. **Version numbers MUST be globally unique.** Diesel keys applied
   migrations by the numeric prefix (`00000000000003` in
   `00000000000003_user_tabs`). Two directories with the same prefix
   make diesel mark one applied and silently skip the other.
   `build.rs` rejects duplicates, but you still need to pick a fresh
   number — pull `origin/main` first, or use a Unix timestamp prefix
   (`date +%s`) on parallel branches.

2. **`CREATE TABLE` / `CREATE INDEX` must include `IF NOT EXISTS`.**
   Cheap insurance against a duplicate-version migration getting
   re-run on a DB that already has the object.

3. **`ALTER TABLE … ADD COLUMN` cannot be made idempotent in SQLite.**
   If you add a column, also add a defensive check in
   `src/db/repair.rs::ensure_schema()` that adds it on startup if
   missing — the only way to heal DBs from older versions when a
   migration has gone wrong.

4. **Never edit a migration after it has been merged.** Add a new
   one. Diesel decides whether to run by version, not by content;
   editing an applied migration produces silent schema drift between
   fresh and existing DBs.

5. **Test with a non-empty DB.** Run the binary against an existing
   data dir before merging, not just `mktemp -d`. Most migration
   breakage only surfaces when the table already has rows.

### Required Workflow When Adding a Migration

```bash
mkdir migrations/$(date +%s)_what_this_does
# write up.sql + down.sql (both with IF NOT EXISTS where supported)
cargo build                       # build.rs rejects duplicate versions
cargo test --lib                  # in-memory migrations + schema tests
./target/release/peckboard --data-dir ~/.peckboard-test  # against a real DB
```

If `cargo build` fails with "duplicate migration version", rename the
new migration. Don't suppress the check.

### Backfilling a Botched Migration

If you discover a column / table that should exist but doesn't on some
DBs, **do not** rely on a new migration to add it (it'll fail on DBs
that already have the column). Add the check to
`src/db/repair.rs::ensure_schema()`; that runs after diesel migrations
and is required to be idempotent.

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
- **Markdown headings use Title Case** (AP-leaning). Capitalize
  principal words; always capitalize the first and last word. Keep
  articles, coordinating conjunctions, and short prepositions
  lowercase mid-title (`a`, `an`, `the`, `and`, `but`, `or`, `for`,
  `of`, `to`, `in`, `on`, `at`, `by`, `with`, `from`, etc.). Preserve
  identifier-shaped tokens verbatim: inline code, ALL-CAPS emphasis
  (`READ THIS BEFORE ADDING ONE`), acronyms (`HTTP`, `MCP`),
  mixed-case names (`ESLint`, `GitHub`), and digit+letter tokens
  (`e2e`, `OAuth2`).

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

## Migrations — READ THIS BEFORE ADDING ONE

**Peckboard runs on real user data. Data loss is not acceptable. A
migration that drops a column, drops a table, alters types, or runs
incorrectly on existing DBs is a P0 incident.** Migrations have
already silently corrupted live databases once on this project.

### Think hard before adding one — but use real schema when you do

**Migrations are unavoidable; treat the decision to add one as
weighty, not the way you express it.** Don't dodge a migration by
stuffing typed data into a JSON blob — schema-on-write protections
(types, NOT NULL, FKs, UNIQUE, indexes) exist for a reason and we
lose all of them inside a `TEXT` column. JSON-in-TEXT is technical
debt that compounds: every query needs string parsing, every
filter/index is impossible without a generated column, and bugs that
schema would have caught at write time turn into silent corruption.

Before reaching for ALTER / CREATE, ask:

- **Is this state actually durable?** If it's per-user-per-browser
  (theme, layout prefs) or per-session ephemeral, it belongs in
  localStorage / sessionStorage / a Zustand store, not the DB.
- **Can it be derived at query time?** Aggregations, denormalised
  copies, and "summary" fields are often cheaper to compute than to
  store and keep in sync.
- **Is the data shape really this volatile?** If you find yourself
  needing five migrations for one feature, the schema is wrong;
  step back and redesign so the columns generalise (e.g. an event
  log with `kind` + typed payload tables, instead of bolting a new
  flag onto a wide row each release).

Once you've decided it's real, durable, structured state, **add a
proper column or table.** Use the right type. Add the constraint.
Add the index. JSON is appropriate only for genuinely free-form
data the application never queries against (e.g. opaque
provider-event payloads in `events.data`, where the producer's
schema isn't ours to define).

### When you must add one

Treat it like a one-way door. Every migration shipped is permanent;
you can never delete the file because it has to keep running for old
DBs. So:

- Make the change as small as possible. Don't bundle unrelated columns.
- **Never DROP** in a forward migration without an explicit
  conversation about acceptable data loss. Prefer to leave the
  obsolete column unused and remove it (if ever) much later.
- **Never change a column's type** in-place. Add a new column,
  backfill, switch reads, leave the old column.
- Backfill rows in the same migration when the new column is
  `NOT NULL` — otherwise existing rows blow up at write time.
- Provide a working `down.sql`. You will need it locally even if you
  never run it in production.

### Hard rules

### Hard rules

1. **Version numbers MUST be globally unique.** Diesel keys applied
   migrations by the numeric prefix (`00000000000003` in
   `00000000000003_user_tabs`). Two directories with the same prefix —
   even with different names — make diesel mark the version applied
   after running one of them and silently skip the other. `build.rs`
   fails compilation if it detects duplicates, but you still need to
   pick a number nobody else is using:
   - If working with parallel branches/contributors, use a Unix
     timestamp prefix (`date +%s` → `1717891234_*`) instead of
     sequential numbers.
   - Pull `origin/main` before adding a migration and check the
     highest existing version number.

2. **`CREATE TABLE` / `CREATE INDEX` must include `IF NOT EXISTS`.**
   Cheap insurance against a duplicate-version migration getting
   re-run on a DB that already has the object.

3. **`ALTER TABLE … ADD COLUMN` cannot be made idempotent in SQLite.**
   If you need to add a column, also add a defensive check in
   `src/db/repair.rs::ensure_schema()` that adds the column on startup
   if missing. This is the only way to safely heal DBs from older
   versions when migrations have gone wrong.

4. **Never edit a migration after it has been merged.** Add a new one.
   Diesel decides whether to run by version, not by content; editing
   an already-applied migration produces silent schema drift between
   fresh and existing DBs.

5. **Test with a non-empty DB.** Run the binary against an existing
   data dir before merging, not just `mktemp -d`. Most migration
   breakage only surfaces when the table already has rows or the
   schema is mid-evolution.

### Required workflow when adding a migration

```bash
mkdir migrations/$(date +%s)_what_this_does
# write up.sql + down.sql (both with IF NOT EXISTS where supported)
cargo build                       # build.rs rejects duplicate versions
cargo test --lib                  # in-memory migrations + schema tests
./target/release/peckboard --data-dir ~/.peckboard-test  # against a real DB
```

If `cargo build` fails with "duplicate migration version", rename the
new migration. Don't suppress the check.

### Backfilling a botched migration

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

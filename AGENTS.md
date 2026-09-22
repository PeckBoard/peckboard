# Peckboard — Claude Working Notes

Project layout, tooling commands, and the workflow Claude should follow
when changing this repo.

## Stack

- **Backend**: Rust 2024, Axum, SQLite via Diesel, single embedded binary.
- **Frontend**: React 19 + TypeScript + Vite + Zustand. Lives in `web/`.
  Built assets in `web/dist/` are embedded into the release binary at
  compile time via `rust-embed`.
- **Agents**: pluggable via the `AgentProvider` trait in
  `src/provider/agent.rs`. First-party backends are trusted crate plugins
  (`peck-plugins/{claude,grok,cursor,kimi,codex,ollama,mock}`): each crate
  builds natively (rlib) and as WASM (cdylib), and boot registers a native
  `CrateAgentProvider` (`src/plugin/crates.rs` +
  `src/provider/crate_provider.rs`) around the crate's hooks. Third-party
  providers load as WASM plugins and register via `provider.register`.
  Sessions pick a backend through the `provider:model` prefix on the model
  id — e.g. `claude:claude-opus-4-7`, `mock:happy-path`. Tests that only
  need a catalog entry may register `src/provider/test_double.rs`
  (`NoopProvider`); never at boot.

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
| Playwright e2e (sharded)      | `scripts/e2e-shards.sh 4`                      |
| Playwright e2e (single)       | `cd web && npm run e2e`                        |
| Playwright e2e (impacted)     | `scripts/e2e-impacted.sh`                      |
| Rebuild the e2e impact map    | `scripts/e2e-impact-map.sh`                    |
| Refresh provider model seeds  | `scripts/refresh-provider-model-seeds.sh`      |
| Playwright install (one-time) | `cd web && npm run e2e:install`                |

First-party providers prefer live CLI/HTTP model discovery at runtime
(`provider.models`). When discovery fails they serve a last-good catalog
cached in the plugin data store, then the compile-time `seed_models()`
fallback. After a provider ships new models, refresh checked-in seeds on a
machine with that CLI authenticated:

```bash
scripts/refresh-provider-model-seeds.sh           # probe + print drift
scripts/refresh-provider-model-seeds.sh --write   # rewrite seed_models()
scripts/refresh-provider-model-seeds.sh --check   # CI: non-zero on drift
```

The Playwright `webServer` block boots `target/release/peckboard` with a
fresh `mktemp -d` data dir, so each run starts from a clean state.

### Why the Suite Is Sharded

Specs mutate shared server state, so the suite runs `workers: 1` and
cannot share one server. Serialising ~480 tests costs ~14 minutes.
`scripts/e2e-shards.sh N` instead runs N Playwright processes side by
side, each `workers: 1` against its **own** server, port block, and data
dir — so every isolation assumption the specs already make still holds,
and a full run takes ~4 minutes. Playwright splits by spec file, so a
given spec runs in exactly one shard and fixed-path artifacts can't
collide.

### Running Only the Impacted Specs

`scripts/e2e-impacted.sh` runs just the specs a change can affect, using
`web/e2e/impact-map.json` (source file → spec files). e2e specs never
_import_ app source — they drive a browser — so that map cannot be
derived statically; it is recorded from a real run by
`scripts/e2e-impact-map.sh` (V8 coverage traced through the bundle
sourcemap for `web/src/**`, plus the server's route log joined to each
test's wall-clock window for `src/**`).

**This is the default for iterating; the merge gate is the full suite**
(see Definition of Done). Selection rules:

- Only build inputs count as changes (`src/**`, `web/src/**`,
  `web/e2e/**`, `migrations/**`, the build manifests) — scratch files,
  logs, docs, and scripts can never force a run.
- A changed spec file selects exactly itself.
- Everything else goes through the map, and the selector falls back to
  the whole suite whenever it cannot prove a narrower set is safe — an
  unmapped source file, a shared primitive, a schema or build change, or
  no map on disk — and prints what it skipped and why.

Regenerate and commit the map after adding or substantially reworking
specs.

## Running the Binary While Iterating

**Always launch ad-hoc / scratch runs against a fresh tmp data dir
_and_ on a random high port, not the user's real install** — unless
the user explicitly asks you to use theirs:

```bash
PORT=$((20000 + RANDOM % 10000))
./target/release/peckboard \
  --data-dir "$(mktemp -d)" \
  --port "$PORT" \
  --https-port "$((PORT + 1))"
```

The user's default data dir holds their real cards, sessions, and DB.
An in-development binary may write data the released version can't
read, or apply a migration that corrupts it. Default to a tmp dir
every time.

The user's real instance is usually already bound to the default
ports (`3344` / `3345`). Picking a random high port avoids
`Address already in use` failures _and_ ensures any traffic you
generate while testing can't accidentally reach the user's live
session. Pair the random port with the tmp dir — never one without
the other.

**Never blanket-kill `peckboard` processes.** The user may have their
own instance running alongside yours. If you need to stop a server
you started, track its PID — capture `$!` after backgrounding it, or
match on the `--data-dir` path you launched it with — and kill only
that one. Do not run `pkill peckboard` / `killall peckboard` /
`fuser -k <port>`; those will take down the user's session.

## Definition of Done

**After making code changes, run the verification cycle and fix anything
it surfaces before reporting done — use the script, do not run the steps
by hand:**

```bash
scripts/verify.sh --impacted  # DEFAULT per change: full checks, e2e narrowed to what the change can reach
scripts/verify.sh --fast      # skip the release build + e2e (quick inner-loop check)
scripts/verify.sh             # the full suite — required before a commit / release
```

It runs, in order: `cargo fmt --check`, `cargo clippy --all-targets
--no-deps`, `cargo test`, `cd web && npm run lint`, `npm run
format:check`, then `cargo build --release` (the binary Playwright
boots) and the e2e suite via `scripts/e2e-shards.sh 4`. Every step runs
even if an earlier one fails, and it exits non-zero with a per-step
summary if anything failed.

**Use `--impacted` for every intermediate change** — running all ~480
specs per edit is the wall-clock sink, and the selector already falls
back to the whole suite whenever it cannot prove a narrower set is safe
(shared primitive, schema/build change, unmapped source file). A changed
spec file selects just itself. The narrowing only ever applies to step 8;
every Rust/web check always runs in full.

**Before a commit or release, run one full `scripts/verify.sh`** — the
map is recorded evidence, not proof, so the merge gate stays the whole
suite. Regenerate the map (`scripts/e2e-impact-map.sh`) after adding or
substantially reworking specs, and commit `web/e2e/impact-map.json`.

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

## Enforce Critical Invariants in the Type System

**When a function has a precondition that must be true for it to be
correct, make the precondition a parameter the caller can only obtain
by satisfying it.** Lint rules, code review, naming conventions, and
documentation all rely on humans noticing — the type system doesn't.
Use it whenever a "you must X before calling this" rule actually
matters for correctness.

The canonical example in this codebase is `SessionLock` in
`src/provider/manager.rs`. Per-session dispatch needs a mutex held
across the `is_running → spawn` decision, or two concurrent senders
both spawn agents. Earlier versions of the manager relied on every
caller remembering to `lock_session()` first — four code paths
forgot, and the resulting double-spawns took a long debugging session
to trace. The fix:

- `SessionLock` is a struct that owns the `OwnedMutexGuard<()>` AND
  carries the `session_id` it locked. It is `pub`, but its only
  constructors (`SessionManager::lock_session`, `try_lock_session`)
  acquire the mutex first.
- The dispatch method is `send_message_locked(&self, &SessionLock,
…)`. It uses `lock.session_id()` internally, so you can't pass a
  lock for "s1" and dispatch to "s2" by mistake.
- The old `pub send_message` is gone. Any future caller that tries
  to skip the lock fails to compile with `E0599: no method named
send_message`, not in code review.

**Reach for this pattern when** the invariant is load-bearing for
correctness (data race, security boundary, transaction scope, "this
must be true or we corrupt user data"), there's a small, well-defined
set of dispatch sites, and the proof token's constructor naturally
co-locates with the work that establishes the invariant. Concrete
forms:

- **Proof token**: a struct whose only constructors run the
  precondition check (the `SessionLock` shape).
- **Typestate**: distinct types for distinct states, with methods on
  each that move you to the next state (e.g. `Connection` →
  `AuthenticatedConnection`).
- **Borrowed witness**: take `&Foo` where the caller already holds a
  `Foo` that means "I did the check" — same idea as `SessionLock`
  but lighter-weight.

**Don't reach for this pattern when** the invariant is a soft
preference (style, performance hint, "usually X"), the constraint
spans a wildly heterogeneous set of call sites where forcing a
uniform shape adds more friction than it removes, or a simpler design
(making the dangerous method private, or just deleting the bypass
path) would work. Type-level enforcement has a real ergonomic cost
at call sites; spend that cost on the invariants that genuinely warrant it.

**When adding a new such invariant**, document the proof type next to
its definition with `// Proof token: bearer has done X. See
[caller] for an example.` so future readers know it's load-bearing
and not just bookkeeping.

## Component Reuse — Prefer Existing Components

**Before writing a new component, check whether one of the shared
primitives already does the job.** Repeatedly hand-rolling list rows,
dropdowns, headers, and context menus is how the app accumulates four
slightly different versions of the same widget that drift apart over
releases.

The frontend has a small set of shared primitives that every screen is
expected to use:

- **`Modal`** (`web/src/components/Modal.tsx`) — portal-rendered modal
  shell. Every dialog goes through this; don't render `.modal-backdrop`
  inline.
- **`List`** (`web/src/components/List.tsx`) — the list-of-things
  view (sessions / projects / repeating tasks / reports). Owns the row
  chrome, the 3-dot menu, right-click / long-press context menus,
  multi-select selection, and the floating bulk-action bar. New list
  views MUST use it.
- **`ListViewHeader`** (`web/src/components/ListViewHeader.tsx`) — the
  title + primary-action bar above a `List`. Use it for every top-level
  list page (Sessions, Projects, Repeating Tasks, Reports, Experts).
- **`Dropdown` + `MenuButton`** (`web/src/components/Dropdown.tsx`) —
  the only popup-menu primitive. Replaces every ad-hoc
  `list-view-dropdown` / `chat-toolbar-dropdown` / inline 3-dot menu.
  Supports dividers (`{ divider: true }`), danger items, submenus, and
  active markers. Don't roll a new menu shape — extend `MenuItem` if you
  truly need a new affordance.
- **`useContextMenu`** (`web/src/hooks/useContextMenu.tsx`) —
  right-click + long-press menu primitive. Shares the `MenuItem` shape
  with `Dropdown` so a row's right-click menu and its 3-dot menu can
  be built from the same list (`List` does this for you).
- **`ConfirmDialog`** (`web/src/components/ConfirmDialog.tsx`) — every
  destructive confirmation. Don't reach for `window.confirm`.
- **`FieldError`** (`web/src/components/FieldError.tsx`) — the inline
  validation message under a single input. Use it instead of appending to
  a form-wide banner: a banner that lists every constraint ends up naming
  fields the user filled in correctly. Pair it with a
  `.form-actions-reason` span stating why the primary action is disabled —
  a disabled button with no stated reason reads as broken.

**Naming and ordering of menu items must match across surfaces.** If a
session's chat-toolbar 3-dot menu says "Clear session", the tab
right-click menu and the sessions-list row menu must say the same.
"Clear session" / "Terminate agent" / "Delete" / etc. — same label,
same order, everywhere. The current canonical session menu order is:

```
Rename
─────────
Model >       (submenu, current value as hint)
Effort >      (submenu, current value as hint)
─────────
Clear session
Terminate agent
Delete        (danger)
```

The TabBar tab context menu adds "Close tab" above (it's a tab
property, not a session property), but the rest matches.

**Don't invent new dropdown shapes.** The model picker, effort picker,
workflow picker, and 3-dot overflow menu are all the same primitive
with different items. Replace any new variant with `MenuButton` / a
`MenuItem[]` list.

**Don't invent new card or row chrome.** `.list-view-row` and its
descendants are the shared row skin. If you need a new column on a row,
add it inside the `renderItem` slot, not as a new component.

**When you DO need something new**, add it next to the existing shared
primitives (`web/src/components/`) and document it here. A "shared
primitive" is a component used by ≥ 2 different views; until then, leave
it where it lives.

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
- Use the shared `crate::provider::agent::emit_event` helper from the
  host when a plugin-emitted event is persisted.
- First-party providers are trusted crate plugins under
  `peck-plugins/<id>/`, wired into boot via `CrateHooks` in
  `src/plugin/crates.rs`. Third-party providers are WASM plugins that
  register through the `provider.register` hook. Do not hand-roll an
  `AgentProvider` in `src/provider/` — implement it in a plugin crate.
- **Markdown headings use Title Case** (AP-leaning). Capitalize
  principal words; always capitalize the first and last word. Keep
  articles, coordinating conjunctions, and short prepositions
  lowercase mid-title (`a`, `an`, `the`, `and`, `but`, `or`, `for`,
  `of`, `to`, `in`, `on`, `at`, `by`, `with`, `from`, etc.). Preserve
  identifier-shaped tokens verbatim: inline code, ALL-CAPS emphasis
  (`READ THIS BEFORE ADDING ONE`), acronyms (`HTTP`, `MCP`),
  mixed-case names (`ESLint`, `GitHub`), and digit+letter tokens
  (`e2e`, `OAuth2`).

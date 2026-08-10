# app-manager

A Peckboard WASM plugin that lists, installs, and removes common
applications on Linux targets — the local Peckboard host and any
configured remote SSH hosts — via MCP tools. System apps come from the
distro package manager (or a vendor script); Python packages come from
pip, tracked as their own clearly-labelled namespace.

It ships both an MCP tool surface (catalog, target abstraction, `app_*`
tools) and an **App Manager dashboard page** reachable from the sidebar.

## Permissions

- `provide_mcp_tools` — the `app_*` tools below.
- `data_store` — target registry and install/remove job records.
- `process_exec_any` — run commands on the **local** Peckboard host. This is
  a broad permission: it lets the plugin run any bare executable on PATH as
  the Peckboard host user. In practice this plugin only ever runs its own
  catalog's static recipes (see `src/catalog.ts`) — user input is validated
  against the catalog before anything reaches a shell — but the permission
  grant itself is not narrower than that.
- `ssh` — run commands on configured **remote** targets.
- `ssh_keys` — resolve a remote target's configured vault key by id
  (`Auth::KeyRef`) without this plugin ever seeing key material, and populate
  the page's key dropdown from `peckboard_ssh_key_list` (metadata only).
- `user_authority` — serve the page's authenticated data routes under the
  signed-in user (`http.request.authed`).
- `contribute_sidebar` — the App Manager sidebar entry.
- `models_read` — the install picker's account+model catalog. Metadata only
  (ids, display names, tiers, account ids), already filtered server-side to
  thinking-capable models; never credentials or tokens.
- `session_write` — create the temporary AI install session (`is_temp`, in
  the shared `~/peckboard-installs/app-manager` folder core registers).
- `session_dispatch` — dispatch the install prompt at that session.
- `session_read` — poll the session's slim event tail (`{seq, kind, name}`,
  never payloads) to render install progress.

**Upgrading from 0.4.0 (or earlier) re-triggers the approval prompt.** The
permission set grew again (`models_read`, `session_write`,
`session_dispatch`, `session_read` for AI-session installs), so Peckboard
loads the new version inert until you approve it again in Settings →
Plugins.

## Dashboard Page

Sidebar → **App Manager** opens `/plugin-api/v1/app-manager`, served by
this plugin and framed in a sandboxed iframe (no `allow-same-origin`). It
talks only to its own authenticated routes, through the host's
postMessage fetch bridge:

| Route                                  | Purpose                                          |
| -------------------------------------- | ------------------------------------------------ |
| `GET /targets`                         | the target dropdown (local + configured remotes) |
| `GET /ssh-keys`                        | vault key metadata for the key dropdown          |
| `GET /apps?target=`                    | distro banner + one grid row per catalog app     |
| `GET /status?target=&app=`             | one app's live state + job progress              |
| `GET /install-options`                 | account+model picker options + stored default    |
| `POST /targets`, `POST /target-remove` | remote-target CRUD                               |
| `POST /install`, `POST /remove`        | start an install (session/script) / remove job   |
| `GET /deps?target=`                    | cached dependency graph, trees + reverse view    |
| `POST /deps-refresh`                   | re-resolve the graph from the package manager    |
| `GET /rdeps?target=&pkg=`              | system-wide reverse deps of one graph package    |

(all under `/api/plugin-ui/app-manager`.)

The page itself is a single HTML string (`src/page.ts`) that cannot import
anything, so every display decision — badge text, action label, job headline,
and the prose an error is rendered as — is made server-side in `src/view.ts`
and shipped as plain data. That is also what the vitest suite covers; the page
is pure DOM plumbing on top.

Notes on the shape of it:

- Target picker and SSH-key picker are `<select>` elements — never free text.
  The page never accepts or displays private key material; a target stores only
  the vault key's id.
- Installs never block the UI: `POST /install` returns a job id and the page
  polls `/status` every 2s, streaming the log tail with a running / succeeded /
  failed state.
- Removal goes through a confirmation that states plainly that it runs a
  package-manager command as root on the target.
- A target that isn't a usable Linux host renders as a refusal instead of an
  app grid; every error is a sentence, never raw JSON.

### Local Target and Folder Scope

`peckboard_exec_any` pins its cwd to the caller's folder, and a **global**
sidebar page has no project or session to resolve one from. Core therefore
falls back to the app data dir when the caller holds full user authority and
carried no folder scope (see `exec_impl` in `src/plugin/host.rs` in the core
repo). An MCP tool call still refuses — its per-folder floor is what keeps a
plugin tool inside the calling session's reach.

## Targets

- `local` is always available and needs no configuration.
- Remote targets are records `{id, hostname, port, username, key_id}` kept
  in this plugin's own `data_store` (plugin data stores are hard-namespaced
  per plugin, so this plugin cannot see ssh-fleet's hosts, and vice versa).
  Only a vault key reference (`key_id`) is ever stored — never a password or
  private key. Populate the key dropdown from the `peckboard_ssh_key_list`
  host function.
- No MCP tool adds or removes a remote target: that is the dashboard page's
  job, through its own `POST /targets` / `POST /target-remove` routes on top of
  the `src/targets.ts` store functions.

## Catalog

`src/catalog.ts` is a plain data table — one entry per app, each with a
`detect` command, a `version` probe, per-package-manager install/remove
recipes, and (for apps not in any distro's repos) a `vendor` install/remove
script. Adding an app is a pure data change. Entries with
`namespace: "pip"` are **Python packages, not system apps**: one pip
recipe used on every distro, a `pip_package` name, and pip-based
detect/version probes (see "The pip Namespace" below).

Distro detection reads `/etc/os-release` on the target and maps
`ID`/`ID_LIKE` to one of `apt` (debian/ubuntu), `dnf` (fedora/rhel), `pacman`
(arch), `zypper` (suse). An unrecognised or non-Linux target is refused with
a clear message — never a guessed command.

## Installs Are Detached Jobs — and Local Installs Run in an AI Session

`app_install`/`app_remove` don't block until completion — plugin calls are
synchronous and bounded by `call_timeout_secs`, and an `ollama`/`docker`
install can run for minutes.

**Local installs (`app_install` on the `local` target) run through a
TEMPORARY AI SESSION** instead of a detached script:

1. The user picks the **account and model** in the dashboard (a `<select>`
   fed by `peckboard_list_models` — thinking-capable models only, filtered
   server-side; the chosen id is validated against that same catalog before
   anything is created, and persisted as the default for next time).
2. The plugin takes the BEFORE package-DB snapshot, creates a temp session
   (`Install <app>`, `is_temp`, in `~/peckboard-installs/app-manager`) on
   that model, and dispatches an install prompt that mirrors the core
   install-session rules — including `sudo -A` so root steps raise the
   masked askpass dialog in the session tab.
3. `app_status` polls the session's **slim event tail** (`{seq, kind,
name}` — core never exposes event payloads to plugins), so the page
   shows tool-level activity plus an "Open install session" link. It is
   deliberately NOT a log; the real conversation lives in the session tab.
4. When the run ends (`agent-end` — emitted for completed and crashed runs
   alike), the plugin takes the AFTER snapshot and decides success by
   re-running the app's **detect probe** — never by trusting the agent's
   own account. A session that vanishes before its run ends (temp tab
   closed, cleared, killed) lands the job in a clear **failed** state with
   an "unknown" note — never a bogus empty delta recorded as success.

**Remote installs and every removal stay deterministic scripts.** An AI
session runs on the Peckboard host and has no path to a remote target's
SSH credentials; and removal is destructive, so a scripted `apt remove` is
preferred over an agent. Those paths keep the original shape:

1. The catalog recipe is wrapped and launched with `nohup sh -c '...' >
<logfile> 2>&1 &`, its PID captured.
2. A job record `{id, target_id, app_id, action, pid, logfile, status}` is
   written to the `data_store`; the tool call returns the job id
   immediately.
3. `app_status` polls: checks whether the pid is still alive and tails the
   logfile. The wrapped script also appends a `PECKBOARD_EXIT:<code>`
   sentinel line on completion, so `app_status` can tell success from
   failure without waiting on the process itself.

Session jobs reuse the same job records with `kind: "session"` plus the
session id, event cursor, and bounded activity lines (`src/jobs.ts`,
`src/installSession.ts`).

## Install Provenance

What an install _genuinely_ added is recorded by bracketing it with
package-database snapshots — never by parsing installer output or asking
an agent. Script installs take both snapshots inside the same detached
script; AI-session installs take them in plugin code around the session's
lifetime (before the prompt is dispatched, after the run ends). Either
way:

    snapshot(before) → install → snapshot(after) → delta = added packages

Snapshots dump `name + version` per line (`dpkg-query -W`, `rpm -qa --qf`,
`pacman -Q`) into `/tmp` files next to the job's logfile, through the same
target abstraction as everything else (`src/exec.ts`). When a poll first
observes the job's terminal state, the two files are read and deleted in
one exec and the delta becomes a record in the `installs` collection
(`src/provenance.ts`), keyed `<target_id>:<app_id>` — a re-install
supersedes, a successful remove deletes. `app_list` and the dashboard
surface it: the app row notes its package-DB version next to the probed
binary version, and the packages that arrived with it render as a
secondary "Installed with …" line, each with its version.

Honest edges, deliberately visible:

- **Vendor `curl | sh` installers** (claude, cursor-agent, ollama) never
  touch the package database. Their rows say so — "not tracked by the
  package manager" — instead of showing an empty list that would read as
  "no dependencies". Prerequisites such an installer does `apt-get
install` DO land in the delta and are listed normally.
- **A failed snapshot** (unsupported package manager, permission denied,
  truncated output) degrades to an explicit "unknown", never to a
  silently-empty delta.
- The record is provenance — "arrived during this job" — not a dependency
  graph: a shared library is attributed to whichever app's install pulled
  it in first. Real dependency edges must come from the package manager.

## Dependency Graph

Provenance answers "what arrived during this job"; the dependency graph
answers "what does this app require right now" — and the edges are
**queried from the package manager itself** (`apt-cache depends`,
`rpm -qR` + `--whatprovides`, `pacman -Qi`), never inferred from the
install delta. The two live in separate `data_store` collections
(`installs` vs `depgraphs`) and never overwrite each other.

It is a DAG, not a tree: install git and node and both depend on
`libssl3` — that node has two parents. The plugin honours that:

- A shared dependency appears under **every** app that requires it,
  flagged `shared`, instead of being attributed to whichever app's
  install pulled it in first.
- The remove confirmation states removal impact with **autoremove
  semantics**: only packages nothing else still depends on are listed as
  "would become unneeded"; a shared dependency another app needs is
  explicitly shown as kept, so the UI never contradicts what the package
  manager would actually do.

Cost control: resolution is seeded from the installed catalog apps'
packages plus the provenance delta set, expanded breadth-first **one
batched exec per level**, depth-limited (default 2, max 4, configurable
per refresh request) and capped at 600 nodes with a visible _truncated_
marker. The graph refreshes when an install/remove job settles and on
the explicit "Refresh dependencies" button — rendering only ever reads
the cached snapshot, which carries an `at` timestamp because dependency
sets drift with upgrades.

The dashboard grows a slim bar under the distro banner: resolution
state + refresh button, plus a reverse view — pick a library from the
dropdown and see which catalog apps require it, with an optional
system-wide `rdepends` query on demand (the package name is validated
against the stored graph before it goes anywhere near a shell). Each
installed app row gains a collapsed "Dependencies" toggle: name +
version + kind (app / library / binary) per node, shared nodes marked,
the app's own binaries listed under its root. The `app_deps` MCP tool
returns the same payload.

Honest limits, stated in the UI rather than papered over:

- **Vendor `curl | sh` installs** (claude, cursor-agent, ollama) never
  enter the package database, so they have **no dependency edges at
  all**. Their rows say "not tracked by the package manager" — never an
  empty tree that would read as "no dependencies".
- **pip/Python packages live in their own section.** They are a different
  namespace from distro packages, so they are never merged into the system
  graph's nodes/edges — see "The pip Namespace" below.
- `kind` is a display heuristic (catalog apps are "app"; `lib`-named
  packages and `.so` capabilities are "library"; everything else renders
  "binary"), and on rpm systems capabilities resolve to their first
  provider.

## The pip Namespace

pip packages are **not** dpkg/rpm/pacman packages: they live in pip's own
database, are invisible to the snapshot bracket above, and must never be
confused with system packages. The plugin treats them as a separate,
explicitly-labelled namespace:

- **Catalog**: `namespace: "pip"` entries (today: `graphifyy`, the package
  the graphify plugin's tools need) install with one pip recipe on every
  distro — `PIP_BREAK_SYSTEM_PACKAGES=1 python3 -m pip install --user
<pkg>` — into the **user site**: no root, nothing outside `$HOME`. The
  env var lifts PEP 668's externally-managed refusal on modern distros and
  is ignored by older pips. `python3` and `pip` themselves are ordinary
  system catalog entries (and `python3` deliberately has **no remove
  recipe** — removing the system Python can dismantle the OS).
- **Probes are pip's own**: presence via `pip show <pkg>`, versions via
  `pip list --format=freeze`, dependency edges via `pip show`'s
  `Requires:` / `Required-by:` lines. Never via the distro package DB.
- **Provenance**: a pip install records `method: "pip"` and
  `tracking: "pip"` (`package_tracking: "pip"` on the MCP surface) — the
  snapshot bracket is deliberately skipped, so an unrelated background
  distro change can never be attributed to a pip app.
- **Dependency view**: pip packages ride along on a dependency refresh as
  their own "Python packages (pip)" block (`pip_packages` in the
  `app_deps` payload), never merged into the system graph's nodes/edges.
  A host without pip just leaves the block empty.
- **UI**: pip rows and entries carry a distinct `pip` badge.

One honest limit: the plugin only tracks pip's **user/system site** for
the target's `python3 -m pip`. Virtualenvs are invisible — in particular,
the graphify plugin's legacy self-install into a folder-root
`.graphify-venv/` is neither seen nor managed here.

## sudo

Recipes that need root use `sudo -A`, matching the core convention (see
`src/service/askpass.rs` and `web/src/utils/installSession.ts`).

- **AI-session installs** (local): the agent runs `sudo -A` inside a real
  session, so the askpass bridge works — the password prompt appears as a
  masked dialog in the session tab (the dashboard flags it as "waiting for
  your answer" and links there).
- **Script installs/removals**: a plugin's own exec calls do not have the
  askpass bridge wired in, so `sudo -A` fails cleanly with sudo's own
  stderr (e.g. "a password is required") rather than hanging — that
  message shows up in the job's log tail via `app_status`.

## Build

```bash
./build.sh
# or: npm install && npm run build
```

Requires `extism-js` on PATH. Output: `dist/plugin.wasm`. Copy it to
`<dataDir>/plugins/app-manager.wasm` (the file stem is the plugin id)
and approve it in Settings → Plugins.

## Renamed from linux-app-manager

Through 0.2.0 this plugin shipped as `linux-app-manager`, and the wasm file
stem is the plugin id. If an older copy is still staged, **delete
`<dataDir>/plugins/linux-app-manager.wasm` when you stage
`app-manager.wasm`** — two staged copies declare the same `app_*` tool
names, and core silently drops whichever set loads second. Core migrates the
plugin's stored data (configured remote targets, job records) from the old
plugin id to `app-manager` automatically at startup, provided the new id has
no data yet.

## Test

```bash
npm test
```

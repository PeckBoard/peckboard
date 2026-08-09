# app-manager

A Peckboard WASM plugin that lists, installs, and removes common
applications on Linux targets — the local Peckboard host and any
configured remote SSH hosts — via MCP tools.

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

**Upgrading from 0.1.0 re-triggers the approval prompt.** The permission set
grew (`user_authority`, `contribute_sidebar`) and so did the hook list
(`http.request.before`, `http.request.authed`), so Peckboard loads the new
version inert until you approve it again in Settings → Plugins.

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
| `GET /status?target=&app=`             | one app's live state + job log tail              |
| `POST /targets`, `POST /target-remove` | remote-target CRUD                               |
| `POST /install`, `POST /remove`        | start a detached job                             |

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
script. Adding an app is a pure data change.

Distro detection reads `/etc/os-release` on the target and maps
`ID`/`ID_LIKE` to one of `apt` (debian/ubuntu), `dnf` (fedora/rhel), `pacman`
(arch), `zypper` (suse). An unrecognised or non-Linux target is refused with
a clear message — never a guessed command.

## Installs are detached jobs

`app_install`/`app_remove` don't block until completion — plugin calls are
synchronous and bounded by `call_timeout_secs`, and an `ollama`/`docker`
install can run for minutes. Instead:

1. The catalog recipe is wrapped and launched with `nohup sh -c '...' >
<logfile> 2>&1 &`, its PID captured.
2. A job record `{id, target_id, app_id, action, pid, logfile, status}` is
   written to the `data_store`; the tool call returns the job id
   immediately.
3. `app_status` polls: checks whether the pid is still alive and tails the
   logfile. The wrapped script also appends a `PECKBOARD_EXIT:<code>`
   sentinel line on completion, so `app_status` can tell success from
   failure without waiting on the process itself.

## sudo

Recipes that need root use `sudo -A`, matching the core convention (see
`src/service/askpass.rs` and `web/src/utils/installSession.ts`). A plugin's
own exec calls do not currently have the askpass bridge wired in, so
`sudo -A` fails cleanly with sudo's own stderr (e.g. "a password is
required") rather than hanging — that message shows up in the job's log
tail via `app_status`.

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

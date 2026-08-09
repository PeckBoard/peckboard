# linux-app-manager

A Peckboard WASM plugin that lists, installs, and removes common
applications on Linux targets — the local Peckboard host and any
configured remote SSH hosts — via MCP tools.

This is the plugin **core**: catalog, target abstraction, and MCP tools.
The web UI (target management, app grid, live install log) is a separate
card/plugin surface and is not implemented here.

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
  (`Auth::KeyRef`) without this plugin ever seeing key material.

## Targets

- `local` is always available and needs no configuration.
- Remote targets are records `{id, hostname, port, username, key_id}` kept
  in this plugin's own `data_store` (plugin data stores are hard-namespaced
  per plugin, so this plugin cannot see ssh-fleet's hosts, and vice versa).
  Only a vault key reference (`key_id`) is ever stored — never a password or
  private key. Populate the key dropdown from the `peckboard_ssh_key_list`
  host function.
- This phase does not expose a tool to add/remove remote targets — that's
  wired up by the UI-page card alongside its HTTP routes, on top of the
  `src/targets.ts` store functions already here.

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
`<dataDir>/plugins/linux-app-manager.wasm` (the file stem is the plugin id)
and approve it in Settings → Plugins.

## Test

```bash
npm test
```

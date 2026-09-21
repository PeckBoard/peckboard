---
title: App Manager
parent: Plugins
nav_order: 11
---

# App Manager

App Manager lists, installs, and removes common developer applications — git, Claude Code, cursor-agent, Ollama, Node.js, Docker, ripgrep, Python, pip — on the PeckBoard host and on any configured SSH host, from a dashboard with live job progress and a package dependency graph.

![The App Manager dashboard: the app grid with installed states and versions, a target picker, and the dependency bar]({{ "/assets/screenshots/plugins/app-manager.png" | relative_url }})

The catalog knows each app's detect probe, install recipe, and remove command per package manager; the distro is detected from `/etc/os-release` and mapped to `apt`, `dnf`, `pacman`, or `zypper`, and an unrecognised target is refused with a message rather than a guessed command. Removals and remote installs run as deterministic scripts with a captured log. Local installs instead run through a temporary AI session on a model you pick — the plugin snapshots the package database before and after, and decides success by re-running the app's detect probe, never by trusting the agent. Root prompts surface as the masked askpass dialog in the install session's tab.

Provenance is honest: installed-with lines come from the package-database delta around the install, and vendor `curl | sh` installers (Claude Code, cursor-agent, Ollama) say plainly that they never touch the package database instead of showing an empty list. The dependency graph is queried from the package manager itself, honours autoremove semantics for removal impact, and offers a reverse lookup — which apps require this library.

Apps the catalog does not know can be added by hand: on the local host the install session identifies the software under strict official-sources-only rules, and a research session fills in a manual row's blanks (what it is, official site, detect probe, commands). A command proposed by an agent is never run as-is — it lands as a suggestion that only becomes runnable when someone clicks _Use this command_.

| Tool                 | What it does                                                                         |
| -------------------- | ------------------------------------------------------------------------------------ |
| `app_targets`        | Lists configured targets: the local host plus any remote SSH hosts                   |
| `app_list`           | The catalog plus hand-added apps, with per-target installed state and version        |
| `app_status`         | One app on one target: installed, version, and any in-flight job's state and log     |
| `app_install`        | Starts an install and returns a job id; local installs need a thinking-capable model |
| `app_remove`         | Starts a removal — always a deterministic scripted command                           |
| `app_deps`           | The cached dependency graph: per-app trees, reverse lookup, and removal impact       |
| `app_record_details` | Records what a hand-added app is — blanks only; commands land as suggestions         |

Other plugins can hand off to the dashboard with a deep link (`?install=python3,pip&from=graphify`) that renders a request bar naming who asked and for what — it only prefills; a human still clicks Install.

![The App Manager request bar: another plugin asked for python3, pip, and graphifyy, each with its state and an Install button]({{ "/assets/screenshots/plugins/app-manager-request.png" | relative_url }})

<details markdown="1">
<summary>Hooks, permissions, and remote targets</summary>

Hooks: `mcp.tool.invoke`, `http.request.before`, `http.request.authed`. Permissions: `provide_mcp_tools`, `data_store`, `process_exec_any`, `ssh`, `ssh_keys`, `user_authority`, `contribute_sidebar`, `models_read`, `session_write`, `session_dispatch`, `session_read`.

`process_exec_any` deserves attention at approval time: it allows the plugin to run any executable on the host's PATH as the PeckBoard user. The plugin restricts itself in code to the catalog's static recipes plus a manual app's user-authored command, but the grant itself is not narrower.

Remote targets store only `{hostname, port, username, key_id}` — a vault key reference, never a password or private key. Adding or removing a target, and adding or forgetting a manual app, are dashboard-only actions; no MCP tool can do either. Upgrading from 0.4.0 or earlier re-triggers approval (the session and model permissions were added later). The plugin was called `linux-app-manager` through 0.2.0; stored data migrates automatically, but a stale `linux-app-manager.wasm` in the plugins directory must be deleted or the duplicate tool names collide.

</details>

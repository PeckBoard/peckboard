# Plugin System (Extism)

Peckboard embeds an Extism WASM runtime. Users drop `.wasm` plugin files into a plugins directory. Plugins can intercept, cancel, modify, or extend every operation in the system.

## Standard

**Every operation gets granular hooks.** This is a system-wide rule, not case-by-case. When a new feature is added, it ships with `*.before` (cancellable, modifiable), `*.after` (observe-only), and `*.failed` (failure reaction) hooks. No exceptions.

## Distribution

The Extism runtime is compiled into the Peckboard binary. Plugins are the only external files the system loads — `.wasm` files placed in `<dataDir>/plugins/`. This is the sole exception to the single-binary rule.

## Plugin Lifecycle

1. On startup, Peckboard scans `<dataDir>/plugins/` for `.wasm` files
2. Each plugin is loaded into its own isolated WASM sandbox
3. Plugins declare which hooks they want to handle via a manifest function
4. On shutdown, all plugin instances are torn down

## Hook Model

Every hookable operation follows the same pattern:

1. Peckboard reaches a hook point
2. All registered plugins for that hook are called in order with the hook payload
3. Each plugin returns a verdict: **allow** (pass through, optionally modified), **cancel** (abort the operation), or **skip** (this plugin has no opinion)
4. If any plugin cancels, the operation is aborted and the cancel reason is surfaced to the caller
5. If a plugin modifies the payload, the modified version is passed to the next plugin in the chain

## Hook Points

### Auth Hooks

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| auth.login.before | Before validating credentials | username, IP | Yes | No |
| auth.login.after | After successful login | user ID, token expiry | No | No |
| auth.login.failed | After failed login attempt | username, IP, attempt count | No | No |
| auth.register.before | Before user registration | username, email, role | Yes | Yes (role) |
| auth.register.after | After user registered | user record | No | No |
| auth.register.failed | After registration failure | username, reason | No | No |
| auth.password.change.before | Before password change | user ID | Yes | No |
| auth.password.change.after | After password changed | user ID | No | No |
| auth.password.change.failed | After password change failure | user ID, reason | No | No |
| auth.session.create.after | After auth session created | session ID, user ID, IP, user-agent | No | No |
| auth.session.revoke.before | Before revoking an auth session | session ID, user ID | Yes | No |
| auth.session.revoke.after | After auth session revoked | session ID, user ID | No | No |

### User Hooks

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| user.create.before | Before creating a user (admin action) | username, email, role | Yes | Yes (role) |
| user.create.after | After user created | user record | No | No |
| user.create.failed | After user creation failure | username, reason | No | No |
| user.update.before | Before updating a user | user ID, changed fields | Yes | Yes (fields) |
| user.update.after | After user updated | user record | No | No |
| user.update.failed | After user update failure | user ID, reason | No | No |
| user.delete.before | Before deleting a user | user ID | Yes | No |
| user.delete.after | After user deleted | user ID | No | No |
| user.delete.failed | After user deletion failure | user ID, reason | No | No |

### Folder Hooks

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| folder.create.before | Before creating a folder | name, path | Yes | Yes (name) |
| folder.create.after | After folder created | folder record | No | No |
| folder.create.failed | After folder creation failure | name, path, reason | No | No |
| folder.delete.before | Before deleting a folder | folder ID, session count | Yes | No |
| folder.delete.after | After folder deleted | folder ID | No | No |
| folder.delete.failed | After folder deletion failure | folder ID, reason | No | No |
| folder.missing | Session recovery found missing folder | session ID, folder ID | No | No |

### Session Hooks

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| session.create.before | Before creating a session | session fields, user ID | Yes | Yes (name, dir, model, effort) |
| session.create.after | After session created | session record | No | No |
| session.create.failed | After session creation failure | fields, reason | No | No |
| session.update.before | Before updating a session | session ID, changed fields | Yes | Yes (fields) |
| session.update.after | After session updated | session record | No | No |
| session.update.failed | After session update failure | session ID, reason | No | No |
| session.delete.before | Before deleting a session | session ID | Yes | No |
| session.delete.after | After session deleted | session ID | No | No |
| session.delete.failed | After session deletion failure | session ID, reason | No | No |
| session.clear.before | Before clearing a session | session ID | Yes | No |
| session.clear.after | After session cleared | session ID | No | No |
| session.message.before | Before sending a message to Claude | session ID, message text, attachments | Yes | Yes (message text) |
| session.message.after | After agent responds | session ID, response | No | No |
| session.message.failed | After message send failure | session ID, reason | No | No |
| session.read.after | After session marked as read | session ID, user ID | No | No |

### Card Hooks

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| card.create.before | Before creating a card | card fields | Yes | Yes (title, description, priority, workflow) |
| card.create.after | After card created | card record | No | No |
| card.create.failed | After card creation failure | fields, reason | No | No |
| card.update.before | Before updating a card | card ID, changed fields | Yes | Yes (fields) |
| card.update.after | After card updated | card record | No | No |
| card.update.failed | After card update failure | card ID, reason | No | No |
| card.delete.before | Before deleting a card | card ID | Yes | No |
| card.delete.after | After card deleted | card ID | No | No |
| card.delete.failed | After card deletion failure | card ID, reason | No | No |
| card.step.before | Before advancing a step | card ID, from step, to step | Yes | No |
| card.step.after | After step advanced | card ID, from step, to step | No | No |
| card.step.failed | After step advance failure | card ID, reason | No | No |
| card.done | Card reached done | card record | No | No |
| card.wont_do | Card reached wont-do | card record, reason | No | No |
| card.blocked.before | Before blocking a card | card ID, reason | Yes | Yes (reason) |
| card.blocked.after | After card blocked | card record, reason | No | No |
| card.unblocked.before | Before unblocking a card | card ID | Yes | No |
| card.unblocked.after | After card unblocked | card record | No | No |

### Worker Hooks

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| worker.spawn.before | Before spawning a worker | card ID, session ID, model, effort | Yes | Yes (model, effort) |
| worker.spawn.after | After worker spawned | session ID, PID | No | No |
| worker.spawn.failed | After spawn failure | card ID, reason | No | No |
| worker.prompt.before | Before sending prompt to Claude | session ID, card ID, prompt text | Yes | Yes (prompt text) |
| worker.prompt.after | After prompt sent | session ID, card ID | No | No |
| worker.done | Worker finished normally | session ID, card ID, intent | No | No |
| worker.error | Worker crashed | session ID, card ID, error reason | No | No |
| worker.recovery.before | Before recovery spawn | session ID, crash count | Yes | No |
| worker.recovery.after | After recovery spawn | session ID | No | No |
| worker.recovery.failed | After recovery denied (loop detected) | session ID, crash count | No | No |
| worker.stop.before | Before stopping a worker | session ID, card ID | Yes | No |
| worker.stop.after | After worker stopped | session ID, card ID | No | No |
| worker.restart.before | Before restarting a worker | card ID | Yes | No |
| worker.restart.after | After worker restarted | session ID, card ID | No | No |

### Project Hooks

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| project.create.before | Before creating a project | project fields | Yes | Yes (name, context, worker_count) |
| project.create.after | After project created | project record | No | No |
| project.create.failed | After project creation failure | fields, reason | No | No |
| project.update.before | Before updating a project | project ID, changed fields | Yes | Yes (fields) |
| project.update.after | After project updated | project record | No | No |
| project.update.failed | After project update failure | project ID, reason | No | No |
| project.delete.before | Before deleting a project | project ID | Yes | No |
| project.delete.after | After project deleted | project ID | No | No |
| project.delete.failed | After project deletion failure | project ID, reason | No | No |
| project.pause.before | Before pausing a project | project ID | Yes | No |
| project.pause.after | After project paused | project ID | No | No |
| project.resume.before | Before resuming a project | project ID | Yes | No |
| project.resume.after | After project resumed | project ID | No | No |

### MCP Hooks

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| mcp.config.write.before | Before writing per-session MCP config JSON | session ID, config object | Yes | Yes (config) |
| mcp.config.write.after | After MCP config written | session ID | No | No |
| mcp.config.delete.after | After MCP config cleaned up | session ID | No | No |
| mcp.token.issue.before | Before issuing an MCP bearer token | session ID, project ID, role | Yes | No |
| mcp.token.issue.after | After MCP token issued | session ID, token context | No | No |
| mcp.token.revoke.after | After MCP token revoked | session ID | No | No |
| mcp.tool.call.before | Before an MCP tool call is processed | session ID, tool name, args | Yes | Yes (args) |
| mcp.tool.call.after | After MCP tool call succeeded | session ID, tool name, result | No | No |
| mcp.tool.call.failed | After MCP tool call failed | session ID, tool name, reason | No | No |
| mcp.server.start.before | Before spawning MCP stdio subprocess | session ID | Yes | No |
| mcp.server.start.after | After MCP subprocess spawned | session ID, PID | No | No |
| mcp.server.start.failed | After MCP subprocess spawn failure | session ID, reason | No | No |
| mcp.server.stop.after | After MCP subprocess stopped | session ID | No | No |

### Event Hooks

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| event.append.before | Before appending to event log | session ID, kind, data | Yes | Yes (data) |
| event.append.after | After event appended | session ID, seq, kind, data | No | No |

### Report Hooks

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| report.write.before | Before writing a report | folder, file, markdown | Yes | Yes (markdown) |
| report.write.after | After report written | folder, file | No | No |
| report.write.failed | After report write failure | folder, file, reason | No | No |
| report.update.before | Before updating a report | folder, file, markdown | Yes | Yes (markdown) |
| report.update.after | After report updated | folder, file | No | No |
| report.update.failed | After report update failure | folder, file, reason | No | No |
| report.delete.before | Before deleting a report | folder, file | Yes | No |
| report.delete.after | After report deleted | folder, file | No | No |
| report.attach.before | Before attaching a file | folder, file, extension, size | Yes | No |
| report.attach.after | After file attached | folder, file | No | No |
| report.attach.failed | After attach failure | folder, file, reason | No | No |

### Attachment Hooks

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| attachment.upload.before | Before uploading session attachment | session ID, filename, size | Yes | No |
| attachment.upload.after | After attachment uploaded | session ID, attachment ID | No | No |
| attachment.upload.failed | After upload failure | session ID, reason | No | No |
| attachment.delete.before | Before deleting an attachment | session ID, attachment ID | Yes | No |
| attachment.delete.after | After attachment deleted | session ID, attachment ID | No | No |

### WebSocket Hooks

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| ws.connect.after | After WebSocket authenticated | user ID, IP | No | No |
| ws.disconnect.after | After WebSocket disconnected | user ID | No | No |
| ws.message.before | Before processing a WS frame | user ID, frame type, payload | Yes | Yes (payload) |

### Push Notification Hooks

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| push.send.before | Before sending a push notification | endpoint, title, body | Yes | Yes (title, body) |
| push.send.after | After push sent | endpoint | No | No |
| push.send.failed | After push failure | endpoint, reason | No | No |
| push.subscribe.after | After push subscription added | endpoint | No | No |
| push.unsubscribe.after | After push subscription removed | endpoint | No | No |

### Config Hooks

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| config.update.before | Before config change | changed fields | Yes | Yes (fields) |
| config.update.after | After config changed | changed fields | No | No |

### Announcement Hooks

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| announcement.create.before | Before creating an announcement | kind, title, message, detail | Yes | Yes (title, message, detail) |
| announcement.create.after | After announcement created | announcement record | No | No |
| announcement.dismiss.before | Before dismissing an announcement | announcement ID, user ID | Yes | No |
| announcement.dismiss.after | After announcement dismissed | announcement ID | No | No |

### Provider Hooks

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| provider.register.before | Before a provider is registered | provider id, display_name, models | Yes | Yes (models list) |
| provider.register.after | After provider registered | provider id | No | No |
| provider.register.failed | After provider registration failure | provider id, reason | No | No |
| provider.spawn.before | Before spawning an agent run | session ID, provider id, model, spawn config | Yes | Yes (spawn config) |
| provider.spawn.after | After agent spawned | session ID, provider id | No | No |
| provider.spawn.failed | After spawn failure | session ID, provider id, reason | No | No |
| provider.send.before | Before sending a message | session ID, provider id, message text, attachments | Yes | Yes (message text) |
| provider.send.after | After message sent | session ID, provider id | No | No |
| provider.event | Provider emitted an event | session ID, provider id, ProviderEvent | No | Yes (event data) |
| provider.interrupt.before | Before soft interrupt | session ID, provider id | Yes | No |
| provider.interrupt.after | After interrupt sent | session ID, provider id | No | No |
| provider.kill.before | Before hard kill | session ID, provider id | Yes | No |
| provider.kill.after | After process killed | session ID, provider id | No | No |
| provider.cleanup.after | After provider resources cleaned up | session ID, provider id | No | No |
| provider.models.list | When model list is requested | provider id, models list | No | Yes (models list) |

### Queued Message Hooks

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| queue.set.before | Before queuing a follow-up message | session ID, text | Yes | Yes (text) |
| queue.set.after | After message queued | session ID, text | No | No |
| queue.deliver.before | Before delivering a queued message to the agent | session ID, text | Yes | Yes (text) |
| queue.deliver.after | After queued message delivered | session ID | No | No |
| queue.delete.before | Before clearing a queued message | session ID | Yes | No |
| queue.delete.after | After queued message cleared | session ID | No | No |

### Wake Detection Hooks

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| wake.detected | Host woke from sleep | grace_window_ms, idle_sessions_count, active_workers_count | No | No |
| wake.grace.expired | Grace window ended | — | No | No |

### Idle Sweeper Hooks

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| idle.sweep.before | Before an idle sweep cycle runs | session count to evaluate | Yes | No |
| idle.kill.before | Before killing an idle session's process | session ID, idle_duration_ms | Yes | No |
| idle.kill.after | After idle process killed | session ID | No | No |

### Watchdog Hooks

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| watchdog.sweep.before | Before a watchdog sweep cycle runs | — | Yes | No |
| watchdog.orphan.detected | Orphan worker session found (no card claims it) | session ID | No | No |
| watchdog.orphan.teardown.before | Before tearing down an orphan | session ID | Yes | No |
| watchdog.orphan.teardown.after | After orphan torn down | session ID | No | No |
| watchdog.stale_ref.detected | Card claims a deleted session | card ID, session ID | No | No |
| watchdog.stale_ref.cleared | Stale ref cleared from card | card ID | No | No |
| watchdog.dead_worker.detected | Card claims a dead/silent worker | card ID, session ID, silent_duration_ms | No | No |
| watchdog.dead_worker.teardown.after | After dead worker torn down and slot refilled | card ID, session ID | No | No |
| watchdog.unclaimed.detected | Unclaimed pipeline card found with spare capacity | card ID, project ID | No | No |

### HTTP Route Hooks

Plugins declare which endpoints they want to intercept via their manifest. Only granted endpoints are called — a plugin cannot intercept routes it hasn't declared.

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| http.request.before | Before route handler executes | method, path, query, headers, body, user ID | Yes | Yes (body) |
| http.request.after | After route handler, before response sent | method, path, status code, response body | No | Yes (response body) |
| http.request.failed | After route handler error | method, path, status code, error | No | No |

**Manifest declaration:**

A plugin's `manifest` declares which endpoints it intercepts:

```json
{
  "hooks": ["http.request.before", "http.request.after"],
  "http_routes": [
    "GET /api/sessions",
    "GET /api/sessions/:id",
    "POST /api/projects/:id/cards"
  ]
}
```

- Routes use the same pattern syntax as the router (`:id` for path params)
- A plugin only receives hook calls for its declared routes
- `http.request.after` can modify the JSON response body — e.g. add fields, filter results, transform data
- `http.request.before` can cancel the request (return an error response) or modify the request body before the handler sees it
- Multiple plugins on the same route are called in load order

### Server Lifecycle Hooks

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| server.started | Server fully booted and listening | port, https_port, data_dir | No | No |
| server.shutdown.before | Graceful shutdown initiated | reason (signal, manual) | No | No |
| server.shutdown.after | All connections closed, about to exit | — | No | No |

### Plugin Hooks

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| plugin.load.after | After a plugin loaded | plugin name | No | No |
| plugin.load.failed | After plugin load failure | plugin name, reason | No | No |
| plugin.unload.after | After a plugin unloaded | plugin name | No | No |

## Plugin API (Host Functions)

Plugins can call back into Peckboard via host functions exposed to the WASM sandbox:

| Function | Description |
|----------|-------------|
| peckboard_create_card | Create a card on a project |
| peckboard_update_card | Update card fields |
| peckboard_delete_card | Delete a card |
| peckboard_list_cards | List cards with filters |
| peckboard_create_project | Create a project |
| peckboard_update_project | Update project fields |
| peckboard_list_projects | List projects |
| peckboard_send_message | Send a message to a session |
| peckboard_create_session | Create a new session |
| peckboard_append_event | Append an event to a session's log |
| peckboard_get_config | Read config values |
| peckboard_get_user | Get user info by ID |
| peckboard_list_users | List users |
| peckboard_log | Write to Peckboard's log (info/warn/error) |
| peckboard_emit_provider_event | Emit a ProviderEvent (Text, ToolStart, ToolEnd, Completed, Crashed, etc.) — for plugin providers |
| peckboard_provider_get_session | Get current session context (ID, folder, card, project) — for plugin providers |
| peckboard_provider_get_mcp_config | Get the MCP config path for the current session — for plugin providers |
| peckboard_register_provider | Register a new AI provider (id, display_name, models) — called during plugin init |

## Sandbox

Hardcoded defaults, not configurable. Plugins are fully locked down.

| Setting | Value | Behavior on violation |
|---------|-------|-----------------------|
| Filesystem | None | No access. All data goes through host functions |
| Network | None | No sockets, no HTTP. No `peckboard_http_request` (removed) |
| Memory | 128 MB per plugin | Plugin killed immediately |
| Execution timeout | 2 seconds per hook call | Hook call aborted, plugin skipped for this invocation |
| Plugin isolation | Full | Plugins cannot see, communicate with, or share state with other plugins |

## Security

- Each plugin runs in an isolated WASM sandbox — no filesystem, no network, no process access
- Host functions are the only way plugins interact with Peckboard
- Plugins cannot access other plugins' state
- A plugin that exceeds 128 MB memory is killed immediately
- A hook call that exceeds 2 seconds is aborted — the hook is skipped and the operation proceeds as if the plugin returned "skip"
- A plugin that panics is killed without affecting the rest of the system
- Killed plugins are logged but do not block the operation that triggered them

## Configuration

```
<dataDir>/
  plugins/
    my-plugin.wasm
    another-plugin.wasm
  config.json          # optional per-plugin config under "plugins" key
```

Config example:
```json
{
  "plugins": {
    "my-plugin": {
      "enabled": true,
      "config": { "webhook_url": "https://..." }
    }
  }
}
```

Per-plugin config is passed to the plugin's `init` function as a JSON string.

## Plugin Interface

Every plugin must export these functions:

| Export | Description |
|--------|-------------|
| `manifest` | Returns JSON declaring which hooks the plugin handles |
| `init` | Called once on load with plugin config. Returns ok/error |
| `handle` | Called for each hook. Receives hook name + JSON payload. Returns verdict + optional modified payload |
| `shutdown` | Called on teardown. Cleanup opportunity |

## Prompt Injection

The `worker.prompt.before` hook is the primary mechanism for plugins to inject instructions into AI interactions. A plugin can:

- Prepend context (e.g. coding standards, project-specific rules)
- Append instructions (e.g. "always write tests", "use this API pattern")
- Replace the prompt entirely (for advanced orchestration)
- Cancel the prompt to prevent the worker from running

The `session.message.before` hook serves the same purpose for interactive chat sessions.

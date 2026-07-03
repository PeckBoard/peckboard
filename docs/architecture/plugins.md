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

| Hook                        | When                            | Payload                             | Can cancel | Can modify |
| --------------------------- | ------------------------------- | ----------------------------------- | ---------- | ---------- |
| auth.login.before           | Before validating credentials   | username, IP                        | Yes        | No         |
| auth.login.after            | After successful login          | user ID, token expiry               | No         | No         |
| auth.login.failed           | After failed login attempt      | username, IP, attempt count         | No         | No         |
| auth.register.before        | Before user registration        | username, email, role               | Yes        | Yes (role) |
| auth.register.after         | After user registered           | user record                         | No         | No         |
| auth.register.failed        | After registration failure      | username, reason                    | No         | No         |
| auth.password.change.before | Before password change          | user ID                             | Yes        | No         |
| auth.password.change.after  | After password changed          | user ID                             | No         | No         |
| auth.password.change.failed | After password change failure   | user ID, reason                     | No         | No         |
| auth.session.create.after   | After auth session created      | session ID, user ID, IP, user-agent | No         | No         |
| auth.session.revoke.before  | Before revoking an auth session | session ID, user ID                 | Yes        | No         |
| auth.session.revoke.after   | After auth session revoked      | session ID, user ID                 | No         | No         |

### User Hooks

| Hook               | When                                  | Payload                 | Can cancel | Can modify   |
| ------------------ | ------------------------------------- | ----------------------- | ---------- | ------------ |
| user.create.before | Before creating a user (admin action) | username, email, role   | Yes        | Yes (role)   |
| user.create.after  | After user created                    | user record             | No         | No           |
| user.create.failed | After user creation failure           | username, reason        | No         | No           |
| user.update.before | Before updating a user                | user ID, changed fields | Yes        | Yes (fields) |
| user.update.after  | After user updated                    | user record             | No         | No           |
| user.update.failed | After user update failure             | user ID, reason         | No         | No           |
| user.delete.before | Before deleting a user                | user ID                 | Yes        | No           |
| user.delete.after  | After user deleted                    | user ID                 | No         | No           |
| user.delete.failed | After user deletion failure           | user ID, reason         | No         | No           |

### Folder Hooks

| Hook                 | When                                  | Payload                  | Can cancel | Can modify |
| -------------------- | ------------------------------------- | ------------------------ | ---------- | ---------- |
| folder.create.before | Before creating a folder              | name, path               | Yes        | Yes (name) |
| folder.create.after  | After folder created                  | folder record            | No         | No         |
| folder.create.failed | After folder creation failure         | name, path, reason       | No         | No         |
| folder.delete.before | Before deleting a folder              | folder ID, session count | Yes        | No         |
| folder.delete.after  | After folder deleted                  | folder ID                | No         | No         |
| folder.delete.failed | After folder deletion failure         | folder ID, reason        | No         | No         |
| folder.missing       | Session recovery found missing folder | session ID, folder ID    | No         | No         |

### Session Hooks

| Hook                   | When                                                               | Payload                               | Can cancel | Can modify                     |
| ---------------------- | ------------------------------------------------------------------ | ------------------------------------- | ---------- | ------------------------------ |
| session.create.before  | Before creating a session                                          | session fields, user ID               | Yes        | Yes (name, dir, model, effort) |
| session.create.after   | After session created                                              | session record                        | No         | No                             |
| session.create.failed  | After session creation failure                                     | fields, reason                        | No         | No                             |
| session.update.before  | Before updating a session                                          | session ID, changed fields            | Yes        | Yes (fields)                   |
| session.update.after   | After session updated                                              | session record                        | No         | No                             |
| session.update.failed  | After session update failure                                       | session ID, reason                    | No         | No                             |
| session.delete.before  | Before deleting a session                                          | session ID                            | Yes        | No                             |
| session.delete.after   | After session deleted                                              | session ID                            | No         | No                             |
| session.delete.failed  | After session deletion failure                                     | session ID, reason                    | No         | No                             |
| session.clear.before   | Before clearing a session                                          | session ID                            | Yes        | No                             |
| session.clear.after    | After session cleared                                              | session ID                            | No         | No                             |
| session.message.before | Before sending a message to Claude                                 | session ID, message text, attachments | Yes        | Yes (message text)             |
| session.message.after  | After agent responds                                               | session ID, response                  | No         | No                             |
| session.message.failed | After message send failure                                         | session ID, reason                    | No         | No                             |
| session.read.after     | After session marked as read                                       | session ID, user ID                   | No         | No                             |
| session.user.answer    | After a user answers a worker's `ask_user` question (notification) | asker_session_id, project_id, qa_text | No         | No                             |

> `session.user.answer` is a **notification**, not a transform: the operation has already happened, so the verdict is ignored and a plugin cannot cancel it — it can only react. Core fires it under a **user-authority** context (the answering user) via `PluginManager::dispatch_authed` — the same context `serve_http_authed` lands for authed routes — so the handler may act on the user's behalf, reaching the user's sessions with whatever host-fn permissions it holds (the experts plugin feeds the `{asker_session_id, project_id, qa_text}` Q&A to its question expert via `session_dispatch`). The fire site is `src/routes/sessions/events.rs`; core itself knows nothing about experts.

### Card Hooks

| Hook                  | When                        | Payload                     | Can cancel | Can modify                                   |
| --------------------- | --------------------------- | --------------------------- | ---------- | -------------------------------------------- |
| card.create.before    | Before creating a card      | card fields                 | Yes        | Yes (title, description, priority, workflow) |
| card.create.after     | After card created          | card record                 | No         | No                                           |
| card.create.failed    | After card creation failure | fields, reason              | No         | No                                           |
| card.update.before    | Before updating a card      | card ID, changed fields     | Yes        | Yes (fields)                                 |
| card.update.after     | After card updated          | card record                 | No         | No                                           |
| card.update.failed    | After card update failure   | card ID, reason             | No         | No                                           |
| card.delete.before    | Before deleting a card      | card ID                     | Yes        | No                                           |
| card.delete.after     | After card deleted          | card ID                     | No         | No                                           |
| card.delete.failed    | After card deletion failure | card ID, reason             | No         | No                                           |
| card.step.before      | Before advancing a step     | card ID, from step, to step | Yes        | No                                           |
| card.step.after       | After step advanced         | card ID, from step, to step | No         | No                                           |
| card.step.failed      | After step advance failure  | card ID, reason             | No         | No                                           |
| card.done             | Card reached done           | card record                 | No         | No                                           |
| card.wont_do          | Card reached wont-do        | card record, reason         | No         | No                                           |
| card.blocked.before   | Before blocking a card      | card ID, reason             | Yes        | Yes (reason)                                 |
| card.blocked.after    | After card blocked          | card record, reason         | No         | No                                           |
| card.unblocked.before | Before unblocking a card    | card ID                     | Yes        | No                                           |
| card.unblocked.after  | After card unblocked        | card record                 | No         | No                                           |

### Worker Hooks

| Hook                   | When                                  | Payload                            | Can cancel | Can modify          |
| ---------------------- | ------------------------------------- | ---------------------------------- | ---------- | ------------------- |
| worker.spawn.before    | Before spawning a worker              | card ID, session ID, model, effort | Yes        | Yes (model, effort) |
| worker.spawn.after     | After worker spawned                  | session ID, PID                    | No         | No                  |
| worker.spawn.failed    | After spawn failure                   | card ID, reason                    | No         | No                  |
| worker.prompt.before   | Before sending prompt to Claude       | session ID, card ID, prompt text   | Yes        | Yes (prompt text)   |
| worker.prompt.after    | After prompt sent                     | session ID, card ID                | No         | No                  |
| worker.done            | Worker finished normally              | session ID, card ID, intent        | No         | No                  |
| worker.error           | Worker crashed                        | session ID, card ID, error reason  | No         | No                  |
| worker.recovery.before | Before recovery spawn                 | session ID, crash count            | Yes        | No                  |
| worker.recovery.after  | After recovery spawn                  | session ID                         | No         | No                  |
| worker.recovery.failed | After recovery denied (loop detected) | session ID, crash count            | No         | No                  |
| worker.stop.before     | Before stopping a worker              | session ID, card ID                | Yes        | No                  |
| worker.stop.after      | After worker stopped                  | session ID, card ID                | No         | No                  |
| worker.restart.before  | Before restarting a worker            | card ID                            | Yes        | No                  |
| worker.restart.after   | After worker restarted                | session ID, card ID                | No         | No                  |

### Project Hooks

| Hook                  | When                           | Payload                    | Can cancel | Can modify                        |
| --------------------- | ------------------------------ | -------------------------- | ---------- | --------------------------------- |
| project.create.before | Before creating a project      | project fields             | Yes        | Yes (name, context, worker_count) |
| project.create.after  | After project created          | project record             | No         | No                                |
| project.create.failed | After project creation failure | fields, reason             | No         | No                                |
| project.update.before | Before updating a project      | project ID, changed fields | Yes        | Yes (fields)                      |
| project.update.after  | After project updated          | project record             | No         | No                                |
| project.update.failed | After project update failure   | project ID, reason         | No         | No                                |
| project.delete.before | Before deleting a project      | project ID                 | Yes        | No                                |
| project.delete.after  | After project deleted          | project ID                 | No         | No                                |
| project.delete.failed | After project deletion failure | project ID, reason         | No         | No                                |
| project.pause.before  | Before pausing a project       | project ID                 | Yes        | No                                |
| project.pause.after   | After project paused           | project ID                 | No         | No                                |
| project.resume.before | Before resuming a project      | project ID                 | Yes        | No                                |
| project.resume.after  | After project resumed          | project ID                 | No         | No                                |

### MCP Hooks

| Hook                    | When                                       | Payload                       | Can cancel | Can modify   |
| ----------------------- | ------------------------------------------ | ----------------------------- | ---------- | ------------ |
| mcp.config.write.before | Before writing per-session MCP config JSON | session ID, config object     | Yes        | Yes (config) |
| mcp.config.write.after  | After MCP config written                   | session ID                    | No         | No           |
| mcp.config.delete.after | After MCP config cleaned up                | session ID                    | No         | No           |
| mcp.token.issue.before  | Before issuing an MCP bearer token         | session ID, project ID, role  | Yes        | No           |
| mcp.token.issue.after   | After MCP token issued                     | session ID, token context     | No         | No           |
| mcp.token.revoke.after  | After MCP token revoked                    | session ID                    | No         | No           |
| mcp.tool.call.before    | Before an MCP tool call is processed       | session ID, tool name, args   | Yes        | Yes (args)   |
| mcp.tool.call.after     | After MCP tool call succeeded              | session ID, tool name, result | No         | No           |
| mcp.tool.call.failed    | After MCP tool call failed                 | session ID, tool name, reason | No         | No           |
| mcp.server.start.before | Before spawning MCP stdio subprocess       | session ID                    | Yes        | No           |
| mcp.server.start.after  | After MCP subprocess spawned               | session ID, PID               | No         | No           |
| mcp.server.start.failed | After MCP subprocess spawn failure         | session ID, reason            | No         | No           |
| mcp.server.stop.after   | After MCP subprocess stopped               | session ID                    | No         | No           |

### Event Hooks

| Hook                | When                          | Payload                     | Can cancel | Can modify |
| ------------------- | ----------------------------- | --------------------------- | ---------- | ---------- |
| event.append.before | Before appending to event log | session ID, kind, data      | Yes        | Yes (data) |
| event.append.after  | After event appended          | session ID, seq, kind, data | No         | No         |

### Report Hooks

| Hook                 | When                        | Payload                       | Can cancel | Can modify     |
| -------------------- | --------------------------- | ----------------------------- | ---------- | -------------- |
| report.write.before  | Before writing a report     | folder, file, markdown        | Yes        | Yes (markdown) |
| report.write.after   | After report written        | folder, file                  | No         | No             |
| report.write.failed  | After report write failure  | folder, file, reason          | No         | No             |
| report.update.before | Before updating a report    | folder, file, markdown        | Yes        | Yes (markdown) |
| report.update.after  | After report updated        | folder, file                  | No         | No             |
| report.update.failed | After report update failure | folder, file, reason          | No         | No             |
| report.delete.before | Before deleting a report    | folder, file                  | Yes        | No             |
| report.delete.after  | After report deleted        | folder, file                  | No         | No             |
| report.attach.before | Before attaching a file     | folder, file, extension, size | Yes        | No             |
| report.attach.after  | After file attached         | folder, file                  | No         | No             |
| report.attach.failed | After attach failure        | folder, file, reason          | No         | No             |

### Attachment Hooks

| Hook                     | When                                | Payload                    | Can cancel | Can modify |
| ------------------------ | ----------------------------------- | -------------------------- | ---------- | ---------- |
| attachment.upload.before | Before uploading session attachment | session ID, filename, size | Yes        | No         |
| attachment.upload.after  | After attachment uploaded           | session ID, attachment ID  | No         | No         |
| attachment.upload.failed | After upload failure                | session ID, reason         | No         | No         |
| attachment.delete.before | Before deleting an attachment       | session ID, attachment ID  | Yes        | No         |
| attachment.delete.after  | After attachment deleted            | session ID, attachment ID  | No         | No         |

### WebSocket Hooks

| Hook                | When                          | Payload                      | Can cancel | Can modify    |
| ------------------- | ----------------------------- | ---------------------------- | ---------- | ------------- |
| ws.connect.after    | After WebSocket authenticated | user ID, IP                  | No         | No            |
| ws.disconnect.after | After WebSocket disconnected  | user ID                      | No         | No            |
| ws.message.before   | Before processing a WS frame  | user ID, frame type, payload | Yes        | Yes (payload) |

### Push Notification Hooks

| Hook                   | When                               | Payload               | Can cancel | Can modify        |
| ---------------------- | ---------------------------------- | --------------------- | ---------- | ----------------- |
| push.send.before       | Before sending a push notification | endpoint, title, body | Yes        | Yes (title, body) |
| push.send.after        | After push sent                    | endpoint              | No         | No                |
| push.send.failed       | After push failure                 | endpoint, reason      | No         | No                |
| push.subscribe.after   | After push subscription added      | endpoint              | No         | No                |
| push.unsubscribe.after | After push subscription removed    | endpoint              | No         | No                |

### Config Hooks

| Hook                 | When                 | Payload        | Can cancel | Can modify   |
| -------------------- | -------------------- | -------------- | ---------- | ------------ |
| config.update.before | Before config change | changed fields | Yes        | Yes (fields) |
| config.update.after  | After config changed | changed fields | No         | No           |

### Announcement Hooks

| Hook                        | When                              | Payload                      | Can cancel | Can modify                   |
| --------------------------- | --------------------------------- | ---------------------------- | ---------- | ---------------------------- |
| announcement.create.before  | Before creating an announcement   | kind, title, message, detail | Yes        | Yes (title, message, detail) |
| announcement.create.after   | After announcement created        | announcement record          | No         | No                           |
| announcement.dismiss.before | Before dismissing an announcement | announcement ID, user ID     | Yes        | No                           |
| announcement.dismiss.after  | After announcement dismissed      | announcement ID              | No         | No                           |

### Provider Hooks

| Hook                      | When                                | Payload                                            | Can cancel | Can modify         |
| ------------------------- | ----------------------------------- | -------------------------------------------------- | ---------- | ------------------ |
| provider.register.before  | Before a provider is registered     | provider id, display_name, models                  | Yes        | Yes (models list)  |
| provider.register.after   | After provider registered           | provider id                                        | No         | No                 |
| provider.register.failed  | After provider registration failure | provider id, reason                                | No         | No                 |
| provider.spawn.before     | Before spawning an agent run        | session ID, provider id, model, spawn config       | Yes        | Yes (spawn config) |
| provider.spawn.after      | After agent spawned                 | session ID, provider id                            | No         | No                 |
| provider.spawn.failed     | After spawn failure                 | session ID, provider id, reason                    | No         | No                 |
| provider.send.before      | Before sending a message            | session ID, provider id, message text, attachments | Yes        | Yes (message text) |
| provider.send.after       | After message sent                  | session ID, provider id                            | No         | No                 |
| provider.event            | Provider emitted an event           | session ID, provider id, ProviderEvent             | No         | Yes (event data)   |
| provider.interrupt.before | Before soft interrupt               | session ID, provider id                            | Yes        | No                 |
| provider.interrupt.after  | After interrupt sent                | session ID, provider id                            | No         | No                 |
| provider.kill.before      | Before hard kill                    | session ID, provider id                            | Yes        | No                 |
| provider.kill.after       | After process killed                | session ID, provider id                            | No         | No                 |
| provider.cleanup.after    | After provider resources cleaned up | session ID, provider id                            | No         | No                 |
| provider.models.list      | When model list is requested        | provider id, models list                           | No         | Yes (models list)  |

### Queued Message Hooks

| Hook                 | When                                            | Payload          | Can cancel | Can modify |
| -------------------- | ----------------------------------------------- | ---------------- | ---------- | ---------- |
| queue.set.before     | Before queuing a follow-up message              | session ID, text | Yes        | Yes (text) |
| queue.set.after      | After message queued                            | session ID, text | No         | No         |
| queue.deliver.before | Before delivering a queued message to the agent | session ID, text | Yes        | Yes (text) |
| queue.deliver.after  | After queued message delivered                  | session ID       | No         | No         |
| queue.delete.before  | Before clearing a queued message                | session ID       | Yes        | No         |
| queue.delete.after   | After queued message cleared                    | session ID       | No         | No         |

### Wake Detection Hooks

| Hook               | When                 | Payload                                                    | Can cancel | Can modify |
| ------------------ | -------------------- | ---------------------------------------------------------- | ---------- | ---------- |
| wake.detected      | Host woke from sleep | grace_window_ms, idle_sessions_count, active_workers_count | No         | No         |
| wake.grace.expired | Grace window ended   | —                                                          | No         | No         |

### Idle Sweeper Hooks

| Hook              | When                                     | Payload                      | Can cancel | Can modify |
| ----------------- | ---------------------------------------- | ---------------------------- | ---------- | ---------- |
| idle.sweep.before | Before an idle sweep cycle runs          | session count to evaluate    | Yes        | No         |
| idle.kill.before  | Before killing an idle session's process | session ID, idle_duration_ms | Yes        | No         |
| idle.kill.after   | After idle process killed                | session ID                   | No         | No         |

### Watchdog Hooks

| Hook                                | When                                              | Payload                                 | Can cancel | Can modify |
| ----------------------------------- | ------------------------------------------------- | --------------------------------------- | ---------- | ---------- |
| watchdog.sweep.before               | Before a watchdog sweep cycle runs                | —                                       | Yes        | No         |
| watchdog.orphan.detected            | Orphan worker session found (no card claims it)   | session ID                              | No         | No         |
| watchdog.orphan.teardown.before     | Before tearing down an orphan                     | session ID                              | Yes        | No         |
| watchdog.orphan.teardown.after      | After orphan torn down                            | session ID                              | No         | No         |
| watchdog.stale_ref.detected         | Card claims a deleted session                     | card ID, session ID                     | No         | No         |
| watchdog.stale_ref.cleared          | Stale ref cleared from card                       | card ID                                 | No         | No         |
| watchdog.dead_worker.detected       | Card claims a dead/silent worker                  | card ID, session ID, silent_duration_ms | No         | No         |
| watchdog.dead_worker.teardown.after | After dead worker torn down and slot refilled     | card ID, session ID                     | No         | No         |
| watchdog.unclaimed.detected         | Unclaimed pipeline card found with spare capacity | card ID, project ID                     | No         | No         |

### HTTP Route Hooks (Plugin-Served Routes)

A plugin can **own and fully serve** a public HTTP route. This is distinct from the cancel/modify hooks above: instead of observing an operation core performs, the plugin receives the request and returns the complete HTTP response. Core does no authentication and has no knowledge of the route — the plugin owns auth (e.g. API keys) end to end.

| Hook                | When                                                                                                                | Payload                                                                    | Returns                                        |
| ------------------- | ------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------- | ---------------------------------------------- |
| http.request.before | A request arrives on `/plugin-api/*` matching a declared route                                                      | method, path, query, headers, body, params                                 | The full HTTP response (status, headers, body) |
| http.request.authed | A request arrives on `/api/plugin-ui/*` matching a declared `ui_routes` route (core has already run `require_auth`) | method, path, query, headers, body, params, **`user`** (the verified user) | The full HTTP response (status, headers, body) |

> There is intentionally **no** `http.request.after` / `http.request.failed` interception of core's own `/api/*` routes — plugin-served routes are a separate, additive surface, and existing `/api/*` auth is never weakened.

These are the two plugin-served HTTP surfaces, and they differ in who authenticates and under whose authority the handler runs:

- **`http.request.before` → `/plugin-api/*`** — the **public** surface (above). Core does no authentication and is **not** behind the `/api/*` auth middleware; the plugin owns auth (e.g. API keys) end to end.
- **`http.request.authed` → `/api/plugin-ui/*`** — the **authenticated** surface. Core guards it with `require_auth` (the same gate as every `/api/*` route) and only then dispatches to the plugin. The payload carries the trusted `user`, and core lands a **user-authority** context for the span of the call, so the plugin's handler may call the scoped host functions on the logged-in user's behalf (gated by the `user_authority` permission — see below). Use this for plugin-served app UI that reads or writes the user's own data.

**Mounting.** Core mounts a dedicated public prefix `/plugin-api/*` (see `src/routes/plugin_api.rs`) that is **not** behind the `/api/*` auth middleware. Every request under it is dispatched to `PluginManager::serve_http`, which finds the first loaded plugin whose `http_routes` match and asks it to serve the request. If no plugin claims the path, the request returns **404**.

**Manifest declaration.** A plugin declares the hook plus the routes it owns. Routes are `"<METHOD> <PATH>"`; `METHOD` may be `*` for any method; paths use `:param` segments (and an optional trailing `*name` catch-all) like the router. The manifest also carries the plugin's **required identity metadata** — `description`, `version`, and `repository` — shown on the plugin's card in Settings:

```json
{
  "description": "Public, API-key-authenticated HTTP surface for Peckboard.",
  "version": "0.2.0",
  "repository": "https://github.com/PeckBoard/api-plugin",
  "hooks": ["http.request.before"],
  "http_routes": [
    "GET /plugin-api/v1/cards",
    "GET /plugin-api/v1/cards/:id",
    "* /plugin-api/v1/*rest"
  ]
}
```

`description`, `version`, and `repository` are **required and must be non-empty** — a manifest missing any of them fails to load. They come from the plugin itself (not the registry), so the operator sees what a plugin is, which release is running, and where it came from, even for a plugin installed outside any registry.

**Request payload** (the hook `payload`, a `PluginHttpRequest`):

```json
{
  "method": "GET",
  "path": "/plugin-api/v1/cards/42",
  "query": "page=2&limit=10",
  "headers": {
    "authorization": "Bearer abc",
    "content-type": "application/json"
  },
  "body": "<raw request body, UTF-8>",
  "params": { "id": "42" }
}
```

- `headers` keys are lowercased; duplicate values are joined with `", "`.
- `body` is the raw request body decoded as UTF-8 (lossily).
- `params` holds the path params captured from the matched pattern.

**Response.** The plugin returns its response as the `payload` of a `Verdict::Allow` (a `PluginHttpResponse`):

```json
{
  "verdict": "allow",
  "payload": {
    "status": 200,
    "headers": { "content-type": "application/json" },
    "body": { "cards": [] }
  }
}
```

- `status` is optional and defaults to `200`.
- `headers` is optional. Header names are normalized to lowercase.
- `body` may be a **JSON string** (sent verbatim) or **any other JSON value** (serialized to JSON text, with `content-type: application/json` defaulted unless the plugin set one). A `null`/absent body is an empty body.
- The plugin returns its real status this way for **all** responses, including `401`/`403`/`404` (e.g. a bad or missing API key → return `{ "status": 401, ... }`).

**Verdict mapping** (`PluginManager::serve_http`):

- `Verdict::Allow { payload }` → that response is returned.
- `Verdict::Cancel { reason }` → a `500` JSON error `{ "error": reason }`. Reserve this for genuine plugin failure; use `Allow` with an explicit status for normal rejections.
- `Verdict::Skip`, an invalid verdict, or a plugin call failure → the next matching plugin (in load order) is tried.
- No plugin declares a matching route → **404**. A plugin claimed the route but none produced a usable response → **500**.

A plugin must list `http.request.before` in `hooks` (the allowlisted hook name) **and** declare its routes in `http_routes` to be consulted.

### Authenticated UI Routes (`/api/plugin-ui/*`)

Alongside the public `/plugin-api/*` surface, a plugin can serve **authenticated** routes under `/api/plugin-ui/*`. These run **behind core's `require_auth`** (the same gate as every `/api/*` route) and **on behalf of the logged-in user**: this is how a plugin serves real app UI that reads or writes the user's own data, rather than a self-authenticated public surface.

**Manifest declaration.** Authenticated routes are declared in the manifest's `ui_routes` (alongside `http_routes`), in the same `"<METHOD> <PATH>"` form (with `:param` segments). A plugin declaring any `ui_routes` MUST also hold the **`user_authority`** permission and list the **`http.request.authed`** hook (the dispatch path that serves them) — core rejects the plugin at load time otherwise.

```json
{
  "hooks": ["http.request.authed"],
  "permissions": ["user_authority"],
  "ui_routes": [
    "GET /api/plugin-ui/experts",
    "GET /api/plugin-ui/pm/decisions",
    "POST /api/plugin-ui/pm/answer",
    "PUT /api/plugin-ui/pm/decisions/:id"
  ]
}
```

**Mounting.** Core mounts `/api/plugin-ui/*` **behind** the `/api/*` auth middleware (see `src/routes/plugin_ui.rs`). Every request is authenticated first; only then is it dispatched to `PluginManager::serve_http_authed`, which finds the first active plugin whose `ui_routes` match and asks it to serve the request. If no plugin claims the path, the request returns **404**.

**Request payload.** Same `PluginHttpRequest` shape as the public surface, with one addition: a trusted `user` block carrying the `require_auth`-verified user (`{ "id": "<user-id>" }`). The plugin never sees or trusts a caller-supplied identity — core stamps it from the authenticated session.

**User authority.** For exactly the span of the `handle` call, core lands a trusted user-authority context in the plugin's host state (cleared the instant `handle` returns). Under it, the plugin's scoped host functions act with the **user's full app authority** — no folder/project scope floor — so the handler can read and write the user's own sessions and the plugin's document store. The response is returned exactly as for `serve_http` (an `Allow` payload is the HTTP response; a `Cancel` maps to a 500).

### UI Panels (Plugin-Contributed Pages)

A plugin can contribute a **UI panel** — a page the Peckboard web app surfaces as a link in the **user dropdown menu** (the avatar menu, alongside the built-in entries). This is generic plumbing: core never renders or interprets the page; it embeds the plugin's own `/plugin-api/*` page (built with the HTTP Route Hook above) in a sandboxed `<iframe>`.

**Manifest declaration.** Panels are declared in the manifest's `ui_panels` (alongside `hooks` / `http_routes`):

```json
{
  "description": "Public, API-key-authenticated HTTP surface for Peckboard.",
  "version": "0.2.0",
  "repository": "https://github.com/PeckBoard/api-plugin",
  "hooks": ["http.request.before"],
  "http_routes": ["GET /plugin-api/v1/admin"],
  "ui_panels": [
    { "id": "api-keys", "title": "API Keys", "path": "/plugin-api/v1/admin" }
  ]
}
```

- `id` is the plugin-local panel id (stable; used as a React key and in test ids).
- `title` is the human label shown on the menu link.
- `path` is the page the host embeds. It **must** be a same-origin, server-absolute path under the plugin-owned `/plugin-api/` prefix.

**Surfacing.** Loaded plugins' panels are aggregated into the existing `GET /api/plugins` catalog response under a top-level `ui_panels` array, each entry tagged with the declaring plugin: `{ "plugin", "id", "title", "path" }`. The web app fetches this once and renders one menu item per panel in the user dropdown menu, with a stable test id `user-menu-plugin-<plugin>-<id>` (e.g. `user-menu-plugin-api-api-keys`). (The Settings → Plugins area also lists the same panels under "Plugin Pages"; both surfaces are generic and render whatever any plugin declares.)

**Embedded page model.** Selecting a panel opens a modal (`data-testid="plugin-panel-modal"`) containing a sandboxed `<iframe>` (`data-testid="plugin-panel-frame"`) whose `src` is the panel `path`. The iframe is sandboxed with `allow-scripts allow-forms allow-popups` and **without** `allow-same-origin`, so the plugin-authored page runs with an opaque origin: it cannot reach the host app's session token in `localStorage`, and its `fetch` calls back to `/plugin-api/*` are cross-origin (the plugin answers CORS preflight and authenticates them with its own credentials). Forwarding the user's Peckboard session into the iframe is intentionally not done by this generic plumbing — a plugin page authenticates to `/plugin-api` with its own credentials (e.g. API keys).

**Security choke point.** `PluginManager::ui_panels` validates paths: a panel whose `path` escapes `/plugin-api/` (an external/`//`-protocol-relative URL, the authenticated `/api/*` surface, or a `..` traversal) is dropped with a warning and never reaches the browser.

**Plugin-defined security headers (`/plugin-api` browser-security carve-out).** Core's global `security_headers` middleware stamps `X-Frame-Options: DENY` + CSP `frame-ancestors 'none'` (and `default-src 'self'`) on every response, which would forbid framing the panel page at all and clobber the plugin page's own CSP. For `/plugin-api/*` only, core therefore **defers to plugin-defined security headers**: the security policy comes from the plugin itself. A plugin returns the headers for each response in its `http.request.before` verdict and the `/plugin-api` dispatch (`src/routes/plugin_api.rs`) applies them verbatim — this is the generic hook by which a plugin owns its own CSP and framing. So `src/security.rs`:

- skips `security_headers` for `/plugin-api/*` (core adds none of its own CSP/framing — the plugin's response headers stand), and
- skips `origin_check` (CSRF) for `/plugin-api/*` — it is bearer-key-authenticated with no ambient cookie credentials, so Origin/CSRF defense is unnecessary there and would 403 the opaque-origin iframe's `Origin: null` calls.

`/api/*` is completely untouched (still `DENY`/`'none'` + global origin check). A plugin serving an embeddable page declares its framing in that page's response, e.g. CSP `frame-ancestors 'self'` (and/or `X-Frame-Options: SAMEORIGIN`), so Peckboard can frame it same-origin while foreign origins cannot. Because the policy is per-response, the same plugin can serve a strict page CSP for its HTML and no page CSP for its JSON API responses. The `api` plugin's management page does exactly this (`frame-ancestors 'self'`).

### Server Lifecycle Hooks

| Hook                   | When                                  | Payload                    | Can cancel | Can modify |
| ---------------------- | ------------------------------------- | -------------------------- | ---------- | ---------- |
| server.started         | Server fully booted and listening     | port, https_port, data_dir | No         | No         |
| server.shutdown.before | Graceful shutdown initiated           | reason (signal, manual)    | No         | No         |
| server.shutdown.after  | All connections closed, about to exit | —                          | No         | No         |

### Plugin Hooks

| Hook                | When                      | Payload             | Can cancel | Can modify |
| ------------------- | ------------------------- | ------------------- | ---------- | ---------- |
| plugin.load.after   | After a plugin loaded     | plugin name         | No         | No         |
| plugin.load.failed  | After plugin load failure | plugin name, reason | No         | No         |
| plugin.unload.after | After a plugin unloaded   | plugin name         | No         | No         |

## Plugin API (Host Functions)

Plugins can call back into Peckboard via host functions exposed to the WASM
sandbox. Each takes a single JSON-string argument and returns a JSON string;
errors come back as `{"error": "..."}` rather than trapping. The functions are
generic and **not** API-key/scope aware — scope enforcement belongs to the
plugin that fronts them. Implemented functions are wired into every loaded
plugin in `src/plugin/host.rs` (`host_functions`); the rest are planned but
**not yet implemented**.

The `peckboard_*_plugin_setting(s)` functions are namespaced to the **calling
plugin**: each loaded plugin gets its own host-function set carrying its own
`plugin_id` (its `.wasm` file stem), so a plugin can only read and write rows
under its own id and cannot touch another plugin's stored state. They are
backed by the existing `plugin_settings` store (no new migration). Stored
values are returned to the owning plugin verbatim — it owns the data and needs
the real value (e.g. to verify an API key it created at runtime). Redaction of
secrets happens only at the separate `/api/plugins/:id/settings` HTTP surface,
which surfaces values to the browser; the host functions never log values.

| Function                                               | Description                                                                                                                                                                                                                                                                                                                                                                                                        | Status              |
| ------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------- |
| peckboard_list_projects                                | List projects. Request: `{}`. Response: `{"projects": [...]}`                                                                                                                                                                                                                                                                                                                                                      | ✅ Implemented      |
| peckboard_list_cards                                   | List cards, optionally filtered. Request: `{"project_id"?: string, "step"?: string}` (omit `project_id` for all projects). Response: `{"cards": [...]}`                                                                                                                                                                                                                                                            | ✅ Implemented      |
| peckboard_create_card                                  | Create a card on a project. Request: `{"project_id": string, "title": string, "description"?, "step"?, "priority"?, "workflow"?, "model"?, "effort"?, "blocked"?, "block_reason"?}`. Validates priority/workflow and that the project exists; inherits the project's workflow when none is given. Response: `{"card": {...}}`                                                                                      | ✅ Implemented      |
| peckboard_get_plugin_setting                           | Read one of the **calling plugin's own** stored settings, scoped to its `plugin_id`. Request: `{"key": string}`. Response: `{"value": <json or null>}` (value returned verbatim — the owner needs the real value; `null` when unset).                                                                                                                                                                              | ✅ Implemented      |
| peckboard_set_plugin_setting                           | Write one of the **calling plugin's own** stored settings, scoped to its `plugin_id`. Request: `{"key": string, "value": <json>}`; a `null`/omitted `value` deletes the key. Rejects oversized keys (>256 B) and values (>64 KB). Response: `{"ok": true}`.                                                                                                                                                        | ✅ Implemented      |
| peckboard_list_plugin_settings                         | List all of the **calling plugin's own** stored settings, scoped to its `plugin_id`. Request: `{}`. Response: `{"settings": {key: value, ...}}` (values verbatim).                                                                                                                                                                                                                                                 | ✅ Implemented      |
| peckboard_store_put / get / list / delete              | The plugin's own **document store** (`plugin_data`), scoped to its `plugin_id`. `put`: `{collection, key, data}` (data is arbitrary JSON ≤256 KB). `get`/`delete`: `{collection, key}`. `list`: `{collection}` → `{items:[{key,value}]}`. Requires the **`data_store`** permission.                                                                                                                                | ✅ Implemented      |
| peckboard_session_meta_set / get                       | Plugin-namespaced metadata on a session (`plugin_session_meta`), so a plugin can tag a session (e.g. "this is an expert") without core columns. `set`: `{session_id, data}` (gated by **`session_write`**); `get`: `{session_id}` → `{value}` (gated by **`session_read`**).                                                                                                                                       | ✅ Implemented      |
| peckboard_create_session                               | Create a generic session in the **caller's** folder + project (taken from the trusted invocation context, never plugin-supplied). `{name, id?, model?, effort?, is_expert?, expert_kind?}` → `{session}`. `is_expert`/`expert_kind` (default false/None) classify the session for usage attribution and listings; expert _knowledge_ state is still the plugin's own `session_meta`. Gated by **`session_write`**. | ✅ Implemented      |
| peckboard_get_session / list_sessions / update_session | Read/list/update the sessions **this plugin manages** (those carrying its `session_meta`) and the caller may see. `list_sessions` returns `{sessions:[{session, meta}]}`; `update_session` writes only generic fields (name/model/effort). Gated by **`session_read`** / **`session_write`**; scoped by ownership + visibility.                                                                                    | ✅ Implemented      |
| peckboard_append_event                                 | Persist one event onto a session the plugin manages + the caller may see. `{session_id, kind, data}` → `{ok:true}` (no broadcast). Gated by **`event_append`**.                                                                                                                                                                                                                                                    | ✅ Implemented      |
| peckboard_list_project_files / read_file               | Read the caller's **folder** (its working dir, from the trusted context). `list_project_files`: `{}` → `{files:[{path,size}], truncated}` (depth ≤8, build/hidden dirs skipped). `read_file`: `{path}` → `{content, truncated, size}` — relative paths only; `..`/absolute/symlink escapes refused. Gated by **`project_files_read`**.                                                                             | ✅ Implemented      |
| peckboard_dispatch_capture / resume_session            | **Fire-and-forget** agent dispatch on a session in the caller's scope (capture run, or deliver + resume — e.g. hand an expert a question, or an answer back to the asker). `{session_id, prompt}` / `{session_id, text}` → `{ok:true}`. Gated by **`session_dispatch`**; authorized by visibility. Needs the live host bound (set after startup); inert in headless managers.                                      | ✅ Implemented      |
| peckboard_update_card                                  | Update card fields                                                                                                                                                                                                                                                                                                                                                                                                 | Not yet implemented |
| peckboard_delete_card                                  | Delete a card                                                                                                                                                                                                                                                                                                                                                                                                      | Not yet implemented |
| peckboard_create_project                               | Create a project                                                                                                                                                                                                                                                                                                                                                                                                   | Not yet implemented |
| peckboard_update_project                               | Update project fields                                                                                                                                                                                                                                                                                                                                                                                              | Not yet implemented |
| peckboard_send_message                                 | Send a message to a session                                                                                                                                                                                                                                                                                                                                                                                        | Not yet implemented |
| peckboard_get_config                                   | Read config values                                                                                                                                                                                                                                                                                                                                                                                                 | Not yet implemented |
| peckboard_get_user                                     | Get user info by ID                                                                                                                                                                                                                                                                                                                                                                                                | Not yet implemented |
| peckboard_list_users                                   | List users                                                                                                                                                                                                                                                                                                                                                                                                         | Not yet implemented |
| peckboard_log                                          | Write to Peckboard's log (info/warn/error)                                                                                                                                                                                                                                                                                                                                                                         | Not yet implemented |
| peckboard_emit_provider_event                          | Emit a ProviderEvent (Text, ToolStart, ToolEnd, Completed, Crashed, etc.) — for plugin providers                                                                                                                                                                                                                                                                                                                   | Not yet implemented |
| peckboard_provider_get_session                         | Get current session context (ID, folder, card, project) — for plugin providers                                                                                                                                                                                                                                                                                                                                     | Not yet implemented |
| peckboard_provider_get_mcp_config                      | Get the MCP config path for the current session — for plugin providers                                                                                                                                                                                                                                                                                                                                             | Not yet implemented |
| peckboard_register_provider                            | Register a new AI provider (id, display_name, models) — called during plugin init                                                                                                                                                                                                                                                                                                                                  | Not yet implemented |

> Note: the original data-access host functions (projects/cards/settings) have
> no per-plugin gate — every loaded `.wasm` plugin can call them, including the
> `peckboard_create_card` write; anything in `<dataDir>/plugins/` is trusted to
> run in-process. Newer capability host functions (the `peckboard_store_*` and
> `peckboard_session_meta_*` family) ARE gated: each requires the plugin to
> declare the matching manifest `permission` (`data_store` / `session_read` /
> `session_write` / `event_append` / `project_files_read` / `session_dispatch`),
> which the operator approves alongside the plugin's hooks (see "Manifest" —
> `permissions`). The gate is enforced at call time in `src/plugin/host.rs`; a
> plugin without the permission gets an `{"error":...}`.
>
> A further permission, **`user_authority`**, doesn't gate one host function but
> lets a plugin act under the **authenticated user's full app authority** — no
> folder/project scope floor. Core enforces it **at plugin load** for any plugin
> declaring `ui_routes` (the `/api/plugin-ui/*` authed surface over
> `http.request.authed`). The matching **user-authority context** is what core
> lands for the span of an authed-route call _and_ of the `session.user.answer`
> notification (via `serve_http_authed` / `dispatch_authed`); under it the
> plugin's scoped host functions act on the logged-in user's behalf — reaching
> the user's own sessions with no scope floor — still gated by each function's
> own permission. (`provide_mcp_tools`, `contribute_sidebar`, and `broadcast`
> round out the allowlist, gating the matching manifest capability.) The full
> set is pinned in `ALLOWED_PERMISSIONS` (`src/plugin/manager.rs`); the context
> is set/cleared by `serve_http_authed` / `dispatch_authed` and read via
> `UserContext`/`authority` in `src/plugin/host.rs`.

### Scope: the trusted invocation context

The session / file / dispatch host functions act on **shared** core data, so a
permission alone is not enough — they must also stay inside the **caller's**
reach. When core dispatches `mcp.tool.invoke` to a plugin, it stamps the
verified caller scope (project + folder, derived from the MCP token and session
row — never from anything the plugin says) into the plugin's host state for
exactly the span of that `handle` call (`PluginManager::invoke_mcp_tool` →
`HostState.invocation`). The scoped functions read it and refuse outside an
invocation. Two checks apply:

- **Ownership** — `get_session` / `list_sessions` / `update_session` /
  `append_event` only reach a session this plugin **marked** with its own
  `session_meta`. A plugin cannot touch an arbitrary user session.
- **Visibility** — every scoped session/file/dispatch call is confined to the
  caller's folder, project, or a global (`project_id = NULL`) session. This is
  the same boundary core's MCP scope tokens enforce; it is what stops a
  plugin-supplied id from crossing a folder/project line. The live-dispatch
  functions (`dispatch_capture` / `resume_session`) use **visibility only** (not
  ownership), because delivering an expert's answer legitimately targets the
  _asking_ session — which the plugin does not own — exactly as core's own
  expert delivery does within the folder boundary.

`create_session` always lands its row in the caller's own folder/project, so a
plugin can't seed a session into someone else's scope either.

### The live host (agent dispatch)

`dispatch_capture` / `resume_session` need the running app (the
`SessionManager`), not just the `Db`. The plugin layer stays free of any
`AppState` coupling: it defines a small `LiveHost` trait, and `main.rs` binds an
`AppLiveHost` (holding a `Weak<AppState>` + a runtime handle) into the manager
**after** `AppState` is built — the `Weak` breaks the otherwise-cyclic
`AppState → PluginManager → LiveHost → AppState` ownership. The calls are
**fire-and-forget**: the host function schedules the agent run on the async
runtime and returns immediately, so a synchronous WASM `handle` call never
blocks on a run (respecting the 2 s call timeout). Headless/test managers leave
the live host unbound, so those functions cleanly return `live dispatch
unavailable`.

The **experts plugin** (`peck-plugins/experts`, written in **TypeScript** and
compiled with the Extism js-pdk) is the reference consumer of this surface, and
owns the **entire** experts feature — there is no experts logic left in core. It
provides the `spin_up_experts` / `list_experts` / `ask_expert` MCP tools,
partitions a project's files (`list_project_files`) into knowledge experts
(`create_session` + `session_meta_set`, which is what tags a session as an
"expert" — not a core column), fires their capture runs (`dispatch_capture`),
and delivers consultations (`resume_session`). It also owns the **question
expert** — fed each user answer via the `session.user.answer` notification — and
the **PM expert**: the `pm_record_decision` / `pm_check_decisions` /
`pm_escalate_to_user` MCP tools, with PM decisions and supersession grants kept
in the plugin's own document store (`peckboard_store_*`), not a core table.
Finally it serves the authenticated Experts and PM views (`ui_routes` over
`http.request.authed`, under `user_authority`) that the web app's React UI
calls.

## Sandbox

Hardcoded defaults, not configurable. Plugins are fully locked down.

| Setting           | Value                   | Behavior on violation                                                   |
| ----------------- | ----------------------- | ----------------------------------------------------------------------- |
| Filesystem        | None                    | No access. All data goes through host functions                         |
| Network           | None                    | No sockets, no HTTP. No `peckboard_http_request` (removed)              |
| Memory            | 128 MB per plugin       | Plugin killed immediately                                               |
| Execution timeout | 2 seconds per hook call | Hook call aborted, plugin skipped for this invocation                   |
| Plugin isolation  | Full                    | Plugins cannot see, communicate with, or share state with other plugins |

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

| Export     | Description                                                                                                               |
| ---------- | ------------------------------------------------------------------------------------------------------------------------- |
| `manifest` | Returns JSON declaring the plugin's required metadata (`description`, `version`, `repository`) and which hooks it handles |
| `init`     | Called once on load with plugin config. Returns ok/error                                                                  |
| `handle`   | Called for each hook. Receives hook name + JSON payload. Returns verdict + optional modified payload                      |
| `shutdown` | Called on teardown. Cleanup opportunity                                                                                   |

## Prompt Injection

The `worker.prompt.before` hook is the primary mechanism for plugins to inject instructions into AI interactions. A plugin can:

- Prepend context (e.g. coding standards, project-specific rules)
- Append instructions (e.g. "always write tests", "use this API pattern")
- Replace the prompt entirely (for advanced orchestration)
- Cancel the prompt to prevent the worker from running

The `session.message.before` hook serves the same purpose for interactive chat sessions.

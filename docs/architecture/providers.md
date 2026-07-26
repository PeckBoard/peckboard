# AI Provider System

Peckboard uses a provider factory pattern for AI integration. Claude CLI is the built-in provider. Plugins can register additional providers (e.g. OpenAI API, local models, custom orchestrators).

## Architecture

### Provider Trait

Every AI provider implements the `AgentProvider` trait
(`src/provider/agent.rs`). The provider owns the full lifecycle —
process/connection management, message sending, output parsing, tool handling,
and cleanup. All methods take `&self` and providers track per-session state
behind interior mutability, because the registry hands out
`Arc<dyn AgentProvider>`.

**Required methods:**

| Method                          | Description                                                                                                                                                                                        |
| ------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `id()`                          | Stable provider id — the prefix in `provider:model` (e.g. `claude`, `mock`)                                                                                                                        |
| `send_message(ctx)`             | Begin ONE turn for `ctx.session_id`. Returns as soon as the run is scheduled; streaming happens in a background task, which reports the outcome on `ctx.completion_tx`                             |
| `cancel(session_id)`            | Cancel any in-flight run for the session (typically a hard kill)                                                                                                                                   |
| `interrupt(session_id)`         | Stop the in-flight run. Implementations MUST actually terminate it; this differs from `cancel` only in that the caller also appends an `interrupt` event so the UI can tell a user interrupt apart |
| `write_stdin(session_id, text)` | Deliver text to the run's input channel (e.g. an answer to a `ControlRequest`). `true` if delivery was attempted                                                                                   |
| `is_running(session_id)`        | Whether a run is currently in flight                                                                                                                                                               |
| `cleanup()`                     | Drop stale per-session state (e.g. exited processes)                                                                                                                                               |
| `shutdown()`                    | Tear down all in-flight runs — called on graceful shutdown                                                                                                                                         |

**Provided methods** (defaults any provider may override):

| Method                             | Default | Description                                                                                                                                                                         |
| ---------------------------------- | ------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `dynamic_models()`                 | `None`  | Settings-derived catalog that overrides the static `ProviderInfo` list. Called only on the read-only catalog path (`/api/models`, the MCP `list_models` tool), so a DB read is fine |
| `model_price(model_id)`            | `None`  | Published `(input, output)` USD per million tokens. `None` means unknown — never free                                                                                               |
| `shutdown_after_turn(session_id)`  | no-op   | Graceful exit once the current turn finishes, guaranteeing no `Crashed {reason: "interrupted"}` on the way out. What the MCP terminal-step tools use instead of `cancel`            |
| `wait_for_termination(session_id)` | returns | Block until the background run has fully wound down, including any synthetic Crashed from the cancel path                                                                           |
| `supports_mid_stream_injection()`  | `false` | `true` ⇒ a second `send_message` mid-turn is absorbed by the same live run, so the SessionManager dispatches straight through instead of persisting to `queued_messages`            |

There is no `spawn` / `resume` / `kill` trait method: one `send_message`
covers both the first turn and a resume (the dispatcher puts the stored
`conversation_id` on `SendMessageContext`), and killing a run is `cancel` or
`interrupt`. Providers translate their native output into `ProviderEvent`s and
feed them through the shared `emit_event` helper, which owns the event-log
append, `usage_events` rows, `conversation_id` persistence, and the WS
broadcast.

### Unified Stream Format

Providers parse their native output format and emit a unified stream of `ProviderEvent` values. Peckboard maps these directly to event log entries. The provider is responsible for this translation — Peckboard never sees raw provider-specific output.

**ProviderEvent kinds:**

| Kind             | Data                                     | Description                              |
| ---------------- | ---------------------------------------- | ---------------------------------------- |
| `Started`        | `{ model, conversation_id?, metadata? }` | Agent initialized                        |
| `Text`           | `{ text }`                               | Streamed text chunk                      |
| `ToolStart`      | `{ tool_use_id, name, input }`           | Agent invoked a tool                     |
| `ToolEnd`        | `{ tool_use_id, output?, error? }`       | Tool finished                            |
| `Completed`      | `{ conversation_id? }`                   | Agent finished normally                  |
| `Crashed`        | `{ reason, exit_code?, stderr? }`        | Agent failed                             |
| `ControlRequest` | `{ request_id, request_type, payload }`  | Agent requesting permission / user input |

**Mapping to event log:**

| ProviderEvent    | Event Log Kind                        |
| ---------------- | ------------------------------------- |
| `Started`        | `agent-start`                         |
| `Text`           | `agent-text`                          |
| `ToolStart`      | `agent-tool-start`                    |
| `ToolEnd`        | `agent-tool-end`                      |
| `Completed`      | `agent-end{status: 'complete'}`       |
| `Crashed`        | `agent-end{status: 'crashed'}`        |
| `ControlRequest` | `question` (for AskUserQuestion type) |

### Provider Registry

The registry holds all available providers. The built-in Claude provider is always registered. Plugin providers register via the `provider.register` hook.

**Registry operations:**

| Operation                      | Description                                                                                                                                                            |
| ------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `list_providers()`             | All registered providers with the models captured at init (cheap — no provider calls; used by dispatch/fan-out paths)                                                  |
| `list_providers_with_models()` | Same, but each provider's **effective** catalog: its `dynamic_models()` override when it has one, else the static list. What `/api/models` and MCP `list_models` serve |
| `get_provider(id)`             | Look up a provider by ID                                                                                                                                               |
| `list_all_models()`            | Flat list of all effective models across all providers, prefixed with provider ID                                                                                      |

**Model ID format:** `provider:model` (e.g. `claude:opus`, `claude:sonnet`, `openai:gpt-4o`)

The model picker in the UI groups models by provider.

### Spawn Config

Resolved per dispatch and handed to `send_message` as
`SendMessageContext.config` (`SpawnConfig`):

| Field             | Description                                                            |
| ----------------- | ---------------------------------------------------------------------- |
| `model`           | Model ID (without provider prefix — the provider knows its own models) |
| `effort`          | Optional effort/reasoning level                                        |
| `working_dir`     | Absolute path to working directory                                     |
| `mcp_config_path` | Path to MCP config JSON (if the provider supports MCP)                 |
| `env`             | Additional environment variables                                       |
| `permission_mode` | How to handle permission prompts (bypass, prompt-user, auto-deny)      |
| `timeout_ms`      | Turn timeout                                                           |
| `metadata`        | Provider-specific config (opaque to Peckboard)                         |

## Built-in: Claude CLI Provider

Provider ID: `claude`

### Models

Discovered from:

1. Seeded aliases: `opus`, `sonnet`, `haiku`, `default`
2. Bedrock ARNs from environment (ANTHROPIC*DEFAULT*\*\_MODEL)
3. Model IDs seen in CLI transcripts

### Implementation

- Spawns `claude -p <msg> --output-format stream-json --verbose`
- Parses newline-delimited JSON from stdout
- Translates CLI events to `ProviderEvent` stream
- Writes `control_response` on stdin for permission prompts and question answers
- Supports `--resume <conversation_id>` for conversation continuity
- Supports `--mcp-config` for MCP tool exposure
- Supports `--effort` for reasoning budget control
- Supports `--permission-prompt-tool stdio` for interactive sessions
- Injects the Peckboard subagent rules into every Task/Agent subagent via a
  `SubagentStart` hook: `spawn_claude` writes a static
  `claude-subagent-context.json` next to the per-session MCP configs
  (`data_dir/worker-mcp/`), and `build_cli_args` folds a hook that `cat`s it
  into the single merged `--settings` value (shared with `autoCompactEnabled`
  — the CLI honours only the last `--settings` flag). Needed because
  `--append-system-prompt` reaches only the main loop, never subagents.

### CLI-Specific Behavior

- `system.init` event backfills `conversation_id` on the `Started` event
- Non-AskUserQuestion permission prompts auto-allowed (for workers)
- Soft interrupt writes a `control_request{subtype:'interrupt'}` on stdin
- Hard kill sends SIGTERM with timeout escalation to SIGKILL

## Plugin Providers

A WASM plugin can register an AI provider (v1 scope: **HTTP-API providers**,
OpenAI-compatible request/response or chunked HTTP consumed inside the call —
no subprocess CLIs, no host-side SSE plumbing). Core wraps it in a
`PluginProviderAdapter` (`src/provider/plugin_provider.rs`) that implements
`AgentProvider` and registers it in the `ProviderRegistry` like any native
provider, so the `SessionManager` dispatch path, `/api/models`, the MCP
`list_models` tool, and provider-visibility filtering all work unchanged.

A provider plugin:

1. Declares the `provider.register` + `provider.send` hooks and the
   `register_provider` permission (all three required, enforced at load).
   Optionally also `provider.models` and/or `provider.interrupt` (each
   requires `provider.register`, also enforced at load).
2. On `provider.register`, calls the `peckboard_register_provider` host
   function with `{id, display_name, models, effort_levels?, pricing?}`.
   Core validates (id `[a-z0-9_-]`, no collision with an existing provider)
   and registers the adapter; `pricing` backs `model_price` for cost ranking.
3. On `provider.send`, runs ONE full agent turn: the hook payload carries
   `{session_id, provider_id, spawn_config, message: {text, attachments},
conversation_id}`. The call runs on a dedicated blocking thread with the
   provider-send budget (default 300s, `--provider-send-timeout-secs` /
   `PECKBOARD_PROVIDER_SEND_TIMEOUT_SECS`) — deliberately above the normal
   2–180s hook clamp.
4. While the call is in flight, streams `ProviderEvent`s via
   `peckboard_emit_provider_event` and polls `peckboard_provider_should_stop`
   between chunks for cooperative interrupts.
5. Returns after emitting `Completed` (carrying the `conversation_id` to
   resume with next turn) or `Crashed`. On a trap, timeout, or a return with
   no terminal event, the adapter emits `Crashed` itself — the session never
   wedges.

### Plugin Provider Lifecycle

```
1. Plugin approved/loaded → core dispatches the provider.register hook
2. Plugin calls peckboard_register_provider { id, display_name, models, ... }
3. Registry adds the provider; its models appear in /api/models
4. User selects a model from the new provider
5. On message send (one provider.send dispatch per turn):
   a. Core dispatches provider.send with the resolved SpawnConfig + message
   b. Plugin drives its HTTP API inside the call
   c. Plugin emits ProviderEvent values via peckboard_emit_provider_event
   d. Core persists each event (event log, usage_events, conversation_id)
      and broadcasts it over WS — the same emit path native providers use
6. On interrupt/cancel: core sets a host-side stop flag; the plugin's next
   peckboard_provider_should_stop poll returns true; the per-call WASM
   timeout guarantees termination regardless. On interrupt specifically, core
   ALSO dispatches provider.interrupt if the plugin declares it — a cleanup
   signal, not a preemption (see Hooks below)
7. On a catalog read (/api/models, MCP list_models): core dispatches
   provider.models if the plugin declares it, and serves the returned list
   instead of the one captured at registration
8. On plugin deny/uninstall/replace: core unregisters the provider and flags
   its in-flight turns to stop
```

`write_stdin` (control responses) is unsupported: a plugin turn has no input
channel. Mid-stream injection is opt-in — a registration carrying
`supports_mid_stream_injection: true` promises its turn drains
`peckboard_provider_take_message`, and core then hands a mid-turn message to
the live turn instead of persisting it in `queued_messages`. Registrations
without the flag (every plugin written before it existed) keep the durable
queue.

### Plugin Provider Host Functions

All gated by the `register_provider` permission; see
`docs/architecture/plugins.md` for full request/response shapes.

| Function                          | Description                                                                                                                                                                  |
| --------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| peckboard_register_provider       | Register the provider (id, display_name, models, effort_levels, pricing) during a provider.register dispatch                                                                 |
| peckboard_emit_provider_event     | Emit a ProviderEvent (Started, Text, ToolStart, ToolEnd, Todo, Usage, Completed, Crashed, …) into the session whose turn this plugin is executing                            |
| peckboard_provider_should_stop    | Poll the cooperative-interrupt flag for the current turn                                                                                                                     |
| peckboard_provider_take_message   | Pop a user message core handed to the live turn mid-flight, or `{"message": null}`. Only ever non-empty for a provider registered with `supports_mid_stream_injection: true` |
| peckboard_provider_get_session    | Get trusted session context (ID, folder path, card, project, is_worker)                                                                                                      |
| peckboard_provider_get_mcp_config | Get the per-session MCP config path (`worker-mcp/<session_id>.json`)                                                                                                         |

## Hooks

Implemented provider hooks (dispatched per declaring plugin):

| Hook               | When                                                                     | Payload                                                         | Can cancel           | Can modify                                             |
| ------------------ | ------------------------------------------------------------------------ | --------------------------------------------------------------- | -------------------- | ------------------------------------------------------ |
| provider.register  | Plugin set loads/changes — core asks the plugin to register its provider | `{}`                                                            | No                   | No                                                     |
| provider.send      | One agent turn on the plugin's provider                                  | session ID, provider id, spawn config, message, conversation_id | Yes (fails the turn) | No                                                     |
| provider.models    | A catalog read (`/api/models`, MCP `list_models`) — optional             | `{provider_id}`                                                 | No                   | Yes (`Allow {models}` replaces the registered catalog) |
| provider.interrupt | The user interrupted a turn on this provider — optional                  | `{session_id, provider_id}`                                     | No                   | No (verdict ignored)                                   |

`provider.models` is dispatched with a `try_lock` on the plugin instance, so a
catalog read never stalls behind an in-flight `provider.send` turn; a busy
plugin simply serves its registered catalog. An empty or invalid list is
ignored the same way.

`provider.interrupt` does **not** stop the turn. Core sets the cooperative
stop flag first (that is what ends the turn, with the per-call WASM timeout as
the hard backstop), then fires this hook. Because extism gives a plugin ONE
instance, the dispatch queues on the same per-plugin mutex the in-flight
`provider.send` call holds and therefore lands once that call has returned:
use it to release what the turn owned OUTSIDE the wasm (an upstream request, a
remote session), not to abort it.

The hooks from earlier drafts — `provider.register.before/after/failed`,
`provider.spawn.*`, `provider.send.before/after`, `provider.event`,
`provider.kill.*`, `provider.cleanup.after` — are **not implemented**.

## Model Resolution (Updated)

Model IDs are now `provider:model` format. Resolution precedence is unchanged:

1. `card.model` (e.g. `claude:opus`)
2. Workflow step's `model`
3. `project.model`
4. Config `defaultProjectModel`
5. Config `defaultProvider` + that provider's default model

If a model string has no provider prefix, it's assumed to be the default provider.

## Config Changes

| Property            | Default | Description                                              |
| ------------------- | ------- | -------------------------------------------------------- |
| defaultProvider     | claude  | Provider used when no prefix specified                   |
| defaultSessionModel | (unset) | Default model for plain sessions (provider:model format) |
| defaultProjectModel | (unset) | Default model for workers (provider:model format)        |

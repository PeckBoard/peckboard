# AI Provider System

Peckboard uses a provider factory pattern for AI integration. Claude CLI is the built-in provider. Plugins can register additional providers (e.g. OpenAI API, local models, custom orchestrators).

## Architecture

### Provider Trait

Every AI provider implements a common interface. The provider owns the full lifecycle — process/connection management, message sending, output parsing, tool handling, and cleanup.

**Required capabilities:**

| Method | Description |
|--------|-------------|
| `id()` | Unique provider identifier (e.g. "claude", "openai") |
| `display_name()` | Human-readable name for the UI |
| `models()` | List available models for this provider (id, display name, capabilities) |
| `spawn(config)` | Start a new agent run. Returns a handle |
| `resume(config, conversation_id)` | Resume a previous conversation |
| `send(handle, message, attachments)` | Send a user message to a running agent |
| `interrupt(handle)` | Soft interrupt (abort current generation, keep process/connection alive) |
| `kill(handle)` | Hard kill (terminate process/connection) |
| `is_alive(handle)` | Check if the agent process/connection is still active |
| `cleanup(handle)` | Tear down resources (process, temp files, connections) |

### Unified Stream Format

Providers parse their native output format and emit a unified stream of `ProviderEvent` values. Peckboard maps these directly to event log entries. The provider is responsible for this translation — Peckboard never sees raw provider-specific output.

**ProviderEvent kinds:**

| Kind | Data | Description |
|------|------|-------------|
| `Started` | `{ model, conversation_id?, metadata? }` | Agent initialized |
| `Text` | `{ text }` | Streamed text chunk |
| `ToolStart` | `{ tool_use_id, name, input }` | Agent invoked a tool |
| `ToolEnd` | `{ tool_use_id, output?, error? }` | Tool finished |
| `Completed` | `{ conversation_id? }` | Agent finished normally |
| `Crashed` | `{ reason, exit_code?, stderr? }` | Agent failed |
| `ControlRequest` | `{ request_id, request_type, payload }` | Agent requesting permission / user input |

**Mapping to event log:**

| ProviderEvent | Event Log Kind |
|---------------|---------------|
| `Started` | `agent-start` |
| `Text` | `agent-text` |
| `ToolStart` | `agent-tool-start` |
| `ToolEnd` | `agent-tool-end` |
| `Completed` | `agent-end{status: 'complete'}` |
| `Crashed` | `agent-end{status: 'crashed'}` |
| `ControlRequest` | `question` (for AskUserQuestion type) |

### Provider Registry

The registry holds all available providers. The built-in Claude provider is always registered. Plugin providers register via the `provider.register` hook.

**Registry operations:**

| Operation | Description |
|-----------|-------------|
| `list_providers()` | All registered providers with their models |
| `get_provider(id)` | Look up a provider by ID |
| `list_all_models()` | Flat list of all models across all providers, prefixed with provider ID |

**Model ID format:** `provider:model` (e.g. `claude:opus`, `claude:sonnet`, `openai:gpt-4o`)

The model picker in the UI groups models by provider.

### Spawn Config

Passed to `spawn()` and `resume()`:

| Field | Description |
|-------|-------------|
| `model` | Model ID (without provider prefix — the provider knows its own models) |
| `effort` | Optional effort/reasoning level |
| `working_dir` | Absolute path to working directory |
| `mcp_config_path` | Path to MCP config JSON (if the provider supports MCP) |
| `env` | Additional environment variables |
| `permission_mode` | How to handle permission prompts (bypass, prompt-user, auto-deny) |
| `timeout_ms` | Turn timeout |
| `metadata` | Provider-specific config (opaque to Peckboard) |

## Built-in: Claude CLI Provider

Provider ID: `claude`

### Models

Discovered from:
1. Seeded aliases: `opus`, `sonnet`, `haiku`, `default`
2. Bedrock ARNs from environment (ANTHROPIC_DEFAULT_*_MODEL)
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
   timeout guarantees termination regardless
7. On plugin deny/uninstall/replace: core unregisters the provider and flags
   its in-flight turns to stop
```

Registered models do not support mid-stream injection: concurrent messages
go through the durable `queued_messages` path, and `write_stdin` (control
responses) is unsupported in v1.

### Plugin Provider Host Functions

All gated by the `register_provider` permission; see
`docs/architecture/plugins.md` for full request/response shapes.

| Function | Description |
|----------|-------------|
| peckboard_register_provider | Register the provider (id, display_name, models, effort_levels, pricing) during a provider.register dispatch |
| peckboard_emit_provider_event | Emit a ProviderEvent (Started, Text, ToolStart, ToolEnd, Todo, Usage, Completed, Crashed, …) into the session whose turn this plugin is executing |
| peckboard_provider_should_stop | Poll the cooperative-interrupt flag for the current turn |
| peckboard_provider_get_session | Get trusted session context (ID, folder path, card, project, is_worker) |
| peckboard_provider_get_mcp_config | Get the per-session MCP config path (`worker-mcp/<session_id>.json`) |

## Hooks

Implemented provider hooks (dispatched per declaring plugin):

| Hook | When | Payload | Can cancel | Can modify |
|------|------|---------|------------|------------|
| provider.register | Plugin set loads/changes — core asks the plugin to register its provider | `{}` | No | No |
| provider.send | One agent turn on the plugin's provider | session ID, provider id, spawn config, message, conversation_id | Yes (fails the turn) | No |

The hooks from earlier drafts — `provider.register.before/after/failed`,
`provider.spawn.*`, `provider.send.before/after`, `provider.event`,
`provider.interrupt.*`, `provider.kill.*`, `provider.cleanup.after`,
`provider.models.list` — are **not implemented**. Interrupts are a host-side
stop flag polled via `peckboard_provider_should_stop`, not a hook.
## Model Resolution (Updated)

Model IDs are now `provider:model` format. Resolution precedence is unchanged:

1. `card.model` (e.g. `claude:opus`)
2. Workflow step's `model`
3. `project.model`
4. Config `defaultProjectModel`
5. Config `defaultProvider` + that provider's default model

If a model string has no provider prefix, it's assumed to be the default provider.

## Config Changes

| Property | Default | Description |
|----------|---------|-------------|
| defaultProvider | claude | Provider used when no prefix specified |
| defaultSessionModel | (unset) | Default model for plain sessions (provider:model format) |
| defaultProjectModel | (unset) | Default model for workers (provider:model format) |

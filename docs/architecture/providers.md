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

Plugins register providers via the hook system. A plugin that wants to add an AI provider:

1. Handles `provider.register` hook — returns provider metadata (id, display_name, models)
2. Handles `provider.spawn` hook — called when Peckboard needs to start an agent run
3. Handles `provider.send` hook — called when a message needs to be sent
4. Handles `provider.interrupt` / `provider.kill` hooks — called for stop/abort
5. Emits `ProviderEvent` values back to Peckboard via a host function (`peckboard_emit_provider_event`)

### Plugin Provider Lifecycle

```
1. Plugin loads → peckboard calls provider.register hook
2. Plugin returns { id, display_name, models: [...] }
3. Registry adds the provider and its models
4. User selects a model from the new provider
5. On message send:
   a. Peckboard calls provider.spawn hook with SpawnConfig
   b. Plugin starts its AI connection/process
   c. Plugin emits ProviderEvent values via peckboard_emit_provider_event
   d. Peckboard maps each ProviderEvent to an event log entry
6. On interrupt/kill:
   a. Peckboard calls provider.interrupt or provider.kill hook
   b. Plugin handles cleanup
```

### Plugin Provider Host Functions

| Function | Description |
|----------|-------------|
| peckboard_emit_provider_event | Emit a ProviderEvent (Text, ToolStart, ToolEnd, Completed, Crashed, etc.) |
| peckboard_provider_get_session | Get current session context (ID, folder, card, project) |
| peckboard_provider_get_mcp_config | Get the MCP config path for this session |

## Hooks

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
| provider.models.list | When model list is requested | provider id, models list | No | Yes (models list — plugin can filter or add) |

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

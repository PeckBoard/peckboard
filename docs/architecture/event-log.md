# Event Log

The per-session event log is the **single source of truth** for everything that happens in a session. There is no separate transcript, no in-memory-only state that matters. If it's not in the event log, it didn't happen.

## Core Principle

As the Claude CLI streams output, each chunk is appended to the event log as its own event in real time. The frontend renders directly from the event log — there's no separate "messages" model. The UI is a live projection of the log.

This means:
- Refreshing the page rebuilds the full transcript from the log
- Reconnecting via WebSocket replays missed events from the log
- Crash recovery reads the log to determine what was happening
- Two clients viewing the same session see the same thing because they project the same log

## Event Structure

Every event has:

| Field | Type | Description |
|-------|------|-------------|
| id | TEXT | UUID |
| session_id | TEXT | Owning session |
| seq | integer | Monotonic per session |
| ts | integer | Milliseconds since epoch (server-stamped) |
| kind | string | Event kind discriminator |
| data | JSON | Shape depends on kind |

## Event Kinds

### `user`

User message submitted.

Data: `{ text: string, attachmentIds?: string[] }`

### `agent-start`

Agent subprocess spawned or resumed. This is the "the agent is now working" signal.

Data:
- `model` — resolved provider:id
- `effort` — resolved CLI flag value (optional)
- `conversationId` — filled once CLI emits `system.init` (initially null)
- `reason` — one of: 'initial', 'clear', 'model-swap', 'step-change', 'recovery'

This event kind supports in-place update (to backfill `conversationId` once the CLI reports it).

### `agent-text`

A streamed text chunk from the agent. Each chunk from the CLI's stream-json output becomes its own event.

Data: `{ text: string }`

The frontend concatenates consecutive `agent-text` events between `agent-start` and `agent-end` to render the full response. Streaming display shows text arriving in real time.

### `agent-tool-start`

The agent started using a tool.

Data:
- `toolUseId` — CLI-assigned tool use ID
- `name` — tool name (e.g. "Read", "Edit", "Bash", "Write")
- `input` — tool input parameters (object)

### `agent-tool-end`

The agent finished using a tool.

Data:
- `toolUseId` — correlates to `agent-tool-start`
- `output` — tool output (string or object, optional)
- `error` — error message if the tool failed (optional)

### `agent-end`

Agent run completed or crashed. This is the "the agent stopped" signal.

Data:
- `status` — 'complete' or 'crashed'
- `reason` — e.g. 'normal', 'exit-code', 'inactivity', 'deadline', 'operator-stop', 'server-shutdown', 'peckboard-crash'
- `exitCode` — number or null (optional)
- `stderr` — last stderr output (optional)
- `conversationId` — final CLI-assigned id (optional)

### `interrupt`

Soft interrupt (non-fatal abort of current API call).

Data: `{ reason: string }` — e.g. 'user-interrupt'

### `system`

Server-emitted informational event (e.g. report chip, folder missing notice).

Data: `{ text: string }`

### `question`

AskUserQuestion prompt surfaced from the Claude CLI.

Data:
- `requestId` — correlation id
- `toolUseId` — Claude-assigned tool_use id (optional)
- `questions` — array of `{ question, header, multiSelect, options }` objects
- `receivedAt` — milliseconds since epoch

### `question-resolved`

User answered or dismissed a question.

Data (union):
- `{ answers: Record<string, string> }` — user answered
- `{ rejected: true, message?: string }` — user dismissed

### `step-change`

Pipeline step advanced (worker sessions only).

Data: `{ from: string, to: string }`

### `session-read`

User marked the session as read (for unread indicators).

### `complete-step-requested`

Worker called `complete_step` MCP tool. Durable record of intent.

### `finish-requested`

Worker called `finish_card` MCP tool.

### `wont-do-requested`

Worker called `wont_do_card` MCP tool.

### `ask-user-requested`

Worker called `ask_user` MCP tool.

### `folder-missing`

Session recovery detected that the session's folder no longer exists.

Data: `{ folder_id: string }`

## Derived State

All session state is derived by walking the event log. No separate state tables, no in-memory caches that aren't rebuildable from the log.

### Agent Status

Derived from the tail of the log:
- **Idle** — latest relevant event is `agent-end` (or no agent events at all)
- **Working** — latest `agent-start` has no corresponding `agent-end`
- **Using a tool** — within an active agent run, latest `agent-tool-start` has no corresponding `agent-tool-end`
- **Crashed** — latest `agent-end` has `status: 'crashed'`
- **Awaiting question** — `question` event exists without a corresponding `question-resolved`

### Crash Detection

An agent run that ends with `agent-end{status:'crashed'}` is a crash. The system detects this and can:
- Auto-resume via `--resume` with a recovery prompt
- Count consecutive crashes via `detectRetryLoop` to prevent money loops
- Surface the crash reason to the user in the UI

### Resume Detection

On server restart, sessions with an `agent-start` as their last lifecycle event (no closing `agent-end`) are detected as dangling. A synthetic `agent-end{status:'crashed', reason:'peckboard-crash'}` is appended so the log is consistent, and recovery can proceed.

### Derived Functions

| Function | Walks | Stops at | Returns |
|----------|-------|----------|---------|
| `deriveAgentStatus` | Tail | — | idle / working / tool-active / crashed / questioning |
| `detectRetryLoop` | Tail (64 events) | `agent-end{complete}`, `step-change` | crash count, allow/deny verdict |
| `findResumeConversationId` | Tail | — | latest `agent-start.conversationId` |
| `deriveWorkerIntent` | Tail (128 events) | latest `step-change` | intent kind or null |
| `derivePendingQuestion` | Tail | — | unresolved `question` or null |
| `deriveIsUnread` | Tail | — | boolean (agent-end{complete} without session-read after it) |

### Frontend Display

The frontend renders the event log directly:
1. Walk events in order
2. `user` events render as user message bubbles
3. Consecutive `agent-text` events between `agent-start` and `agent-end` render as a single streaming assistant message
4. `agent-tool-start` / `agent-tool-end` pairs render as collapsible tool-use blocks within the assistant turn
5. `step-change` renders as a step header
6. `system` renders as an info notice
7. `question` / `question-resolved` renders as an interactive card or resolved answer

During live streaming, new `agent-text` events append to the current assistant message in real time. The user sees tokens arrive as they're produced.

## Rules

1. **Everything lives in the log.** If a client needs it after refresh or the backend needs it after restart, it's an event.
2. **Append-only.** One narrow exception: `agent-start` can be updated in-place to backfill `conversationId`.
3. **State is derived, not stored.** Agent status, crash counts, pending questions — all computed by walking the tail.
4. **Timestamps are server-stamped.** Callers don't set their own timestamps.
5. **Each CLI output chunk is its own event.** Not batched, not buffered. The log is the stream.
6. **Crashes are detectable from the log alone.** `agent-start` without `agent-end` = dangling. `agent-end{crashed}` = crash. No external state needed.

# Worker Lifecycle

## Claude CLI Spawning

The Claude CLI is spawned as a child process with these arguments:

- `claude -p <prompt> --output-format stream-json --verbose`
- `--model <provider:id>` — resolved via card > workflow step > project > system default
- `--effort <level>` — optional, same precedence chain; omitted if null
- `--mcp-config <path>` — per-session JSON file referencing the MCP stdio subprocess
- `--permission-prompt-tool stdio` — plain sessions only (workers omit this)
- `--resume <conversationId>` — if resumable conversation exists
- Environment: `PECKBOARD_TOKEN`, `PECKBOARD_BASE_URL`, plus any configured env vars

Output is newline-delimited JSON parsed into event kinds: agent-start, agent chunks, agent-end, control_request, interrupt.

## Lifecycle Phases

### Spawn

1. `checkAndSpawnWorkers` iterates pipeline steps right-to-left, then backlog, picking unassigned unblocked cards
2. Coalesced via per-project spawn lock (at most one queued follow-up)
3. For each card:
   - Resolve effective model + effort
   - Create Session record
   - Issue MCP token
   - Write MCP config JSON
   - Call sendMessage with initial worker prompt
   - Mark card `workerSessionId = session.id`

### Running

- One session per card for the card's entire life
- Step boundaries kill and respawn the CLI with `--resume` (reason='step-change')
- Session carries context across steps without creating a new record
- The session is viewable by clicking the card's 3-dot menu → "Session" on the kanban board
- The full transcript (all steps) is visible in the same chat view used for interactive sessions
- Card sessions are `is_worker = true` and do not appear in the main session list
- Changing the session's model/effort from the chat UI (`PATCH /api/sessions/:id`) takes effect immediately: the live CLI is hard-cancelled (an `interrupted` crash, excluded from auto-pause counting), the card's claim is released, and the orchestrator resumes the same session with `--resume` under the new settings. Cross-provider/account switches are refused with 409 — workers can't run the handover doc turn; set the card/project model to move future workers instead.

### Done

Worker signals completion via one of four MCP tools (logged as durable events before returning success):

1. **`complete_step`** — advance to next pipeline step (or terminal if none left)
2. **`finish_card`** — jump straight to 'done', skip remaining steps
3. **`wont_do_card`** — park in 'wont-do' column for human triage
4. **`ask_user`** — block card, surface question on kanban card

Fallback: message ending with "DONE" (works but less preferred)

On done event, `handleWorkerDone`:

- Derives intent from event log via `deriveWorkerIntent`
- Acts on intent (advance step, release context, move card)
- If no intent and no "DONE": schedules continue-retry with exponential backoff

### Error / Crash

On crash:

- `handleWorkerError` checks if reason is deterministic (e.g. `invalid-model`) — blocks immediately
- Otherwise schedules `sendRecoveryPrompt` (5s delay) on the SAME session
- Recovery prompt calls `detectRetryLoop` before spawning
- If crash count <= threshold: spawn recovery run
- If crash count > threshold: block card with reason

### Step Advancement

1. Worker calls `complete_step` → `complete-step-requested` event appended
2. `handleWorkerDone` derives intent, appends `step-change` event
3. `findNextStep` walks workflow for next step with non-empty instructions
4. If found: kill CLI, respawn with `--resume` and new step instructions
5. If not found: release context, move card to 'done'

**Known limitation:** model/effort are frozen at initial spawn (`session.spawn`). Per-step workflow overrides only take effect on the initial spawn, not on later step transitions.

## Money-Loop Defense

### detectRetryLoop

Pure function over the session's event log tail (bounded, default 64 events):

- Counts consecutive `agent-end{crashed}` events
- Stops counting at: `agent-end{complete}`, `step-change`, `agent-end{reason:'server-shutdown'}`
- User events do NOT halt the walk (every respawn writes a user event)
- Threshold: 3 consecutive crashes allowed (configurable). 4th blocks.

### Blocking

- Denied verdict: no `agent-start` appended, no child process started
- Card blocked with `blockReason` naming the last crash reason and count
- Operator unblock resets via session replacement (restarting creates a fresh session with empty tail)

### Spawn-Time Exceptions

- Spawn-phase errors (before CLI starts) block on first failure — no session tail to walk
- Spawn-phase timeout also blocks immediately
- Strictly safer than loop-forever

### Wake Grace Window

- 30-second window after detected sleep/wake
- `detectRetryLoop` with `inWakeGrace: true` forces `allow=true` regardless of crash count
- Crash count still tracked honestly (for logging), but verdict overridden
- Prevents sleep-induced SIGTERMs from burning the crash budget

## Continue-Retry Backoff

When the worker ends its turn without signaling intent (no MCP tool call, no "DONE"):

- Exponential backoff: 2s, 4s, 8s, 16s, 32s (base=2000ms, max=60000ms)
- Max 5 retries. 6th done-without-intent moves card to 'wont-do'
- Wake-grace carve-out: counter NOT incremented if done lands inside grace window

Retry nudge is a user message telling the worker to continue and call `complete_step` when done.

## Worker Intent Derivation

`deriveWorkerIntent` walks the session's event log tail (bounded, default 128 events):

- Searches newest→oldest, stops at most recent `step-change`
- Returns first `*-requested` event found: `complete-step-requested`, `finish-requested`, `wont-do-requested`, `ask-user-requested`
- Returns null if no intent found
- Survives crash + recovery: intent logged before crash persists for the next done event

## Worker Watchdog (sweepOrphanWorkers)

Runs every 60 seconds. Handles four cases:

1. **Orphan sessions** — worker session no card claims → tear down
2. **Stale refs** — card claims deleted session → clear ref
3. **Dead-but-claimed** — card claims session but process is dead and silent for 2x timeout → tear down and re-fill
4. **Unclaimed pipeline cards** — active project with spare capacity and unassigned cards → kick spawn

## Idle Process Sweeper

- Plain sessions only (workers are exempt)
- Kills child processes idle longer than `idleProcessTimeoutMs` (default 30 min)
- Session record + conversationId preserved; next message respawns via `--resume`
- First sweep after wake is skipped (stale timestamps from sleep)

## Worker Prompt Construction

### Initial Spawn

1. Project context (readonly, from project admin)
2. Card title + description
3. Workflow step instructions
4. Handoff context (if present from prior worker)
5. System instructions: how to split work, ask questions, signal completion

### Step Advance

1. Same project context + card (restated in case prior step compacted)
2. New step's instructions
3. Handoff context
4. Shorter than initial (no need to re-explain splitting/bailing)

The worker does NOT know about pipelines, workflows, or step names. From its perspective: here's a project, here's a card, here's what to do.

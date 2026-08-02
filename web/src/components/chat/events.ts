import type { Event } from '../../types/api'

// Stable empty array so subscribers don't see a new reference every render
// when there are no events yet for a given session.
export const EMPTY_EVENTS: Event[] = []

/** Option object from an AskUserQuestion, with optional description */
export interface QuestionOption {
  label: string
  description?: string
}

/** Structured question within a question event */
export interface QuestionItem {
  question: string
  header?: string
  multiSelect?: boolean
  options?: string[]
  optionObjects?: QuestionOption[]
}

/** One attachment carried by a user turn, as recorded on the `user` event. */
export interface MessageAttachment {
  filename: string
  mimeType: string
  /** Attachment id — lets the chat fetch the bytes for an inline preview. */
  id?: string
}

/** An image returned by a tool (e.g. a Playwright MCP screenshot). New
 *  events carry a blob reference `id` (served by
 *  `/api/sessions/:id/tool-images/:imageId`); legacy events persisted before
 *  the blob offload carry the base64 payload inline. */
export interface ToolImage {
  mimeType: string
  /** Inline base64 payload — legacy events only. */
  dataBase64?: string
  /** Blob reference id — fetched with auth from the tool-images route. */
  id?: string
}

/** Server-computed unified diff for an edit_file / write_file call,
 *  delivered as a `file-diff` side-channel event. */
export interface FileDiff {
  path: string
  diff: string
  added: number
  removed: number
  truncated?: boolean
  created?: boolean
}

/** A display item derived from one or more raw events. */
export type DisplayItem =
  | {
      type: 'user'
      text: string
      key: string
      ts: number
      attachments?: MessageAttachment[]
      /** Set when the pre-hatcher delivered this message: the user's
       *  original text (shown expandable under the enriched message). */
      preHatchOriginal?: string
      /** True when the delivered text was enriched (differs from the
       *  original); false when the pre-hatcher passed it through. */
      preHatchEnriched?: boolean
    }
  | {
      /** A message parked by the pre-hatcher: shown as the user's bubble
       *  with a live feed of the research session's actions until the
       *  final `user` event (or any later user turn) supersedes it. */
      type: 'pre-hatch'
      text: string
      key: string
      ts: number
      /** The temp research session gathering context — the UI subscribes to
       *  its events to show the live feed. Absent on legacy `pre-ignite`
       *  events persisted before the pre-hatcher rename. */
      tempSessionId?: string
      /** The model the research session runs on. */
      model?: string
      /** Set when a later `user` event superseded this parked message —
       *  filtered out of the rendered feed. */
      superseded?: boolean
    }
  | { type: 'assistant'; text: string; key: string; ts: number }
  | {
      type: 'tool'
      toolName: string
      input?: Record<string, unknown>
      /** Structured object from the mock/plugin providers, or the raw text
       *  (often JSON-in-a-string) echoed back through the Claude CLI. */
      output?: Record<string, unknown> | string
      error?: string
      images?: ToolImage[]
      isRunning: boolean
      /** Event ts (ms) of the tool start / end — drives the duration badge. */
      startTs?: number
      endTs?: number
      /** Unified diff attached from a `file-diff` event (edit/write tools). */
      diff?: FileDiff
      key: string
    }
  | { type: 'file-diff'; diff: FileDiff; ts: number; key: string }
  | { type: 'thinking'; text: string; key: string; ts: number }
  | {
      type: 'turn-usage'
      turnSeq: number | null
      /** One entry per model that billed in this turn. */
      slices: {
        model: string
        input: number
        output: number
        cacheRead: number
        cacheCreation: number
      }[]
      outputTokens: number
      /** Context-window occupancy reported for this turn (0 = unreported). */
      contextTokens: number
      /** Occupancy change vs the previous reporting turn; null when unknown. */
      contextDelta: number | null
      /** Wall-clock from the turn's start (user send / agent start) to the
       *  usage report; null when no anchor was seen. */
      durationMs: number | null
      key: string
      ts: number
    }
  | { type: 'status'; text: string; key: string; ts: number }
  | {
      type: 'system'
      text: string
      /** Raw event payload when the event carried no human-readable text —
       *  rendered behind a <details> instead of inline. */
      detail?: Record<string, unknown>
      /** Consecutive identical notices coalesced into one row (×N badge). */
      count?: number
      key: string
      reportFolder?: string
      reportFile?: string
      ts: number
    }
  | { type: 'step'; label: string; key: string }
  | {
      type: 'agent-start'
      model: string
      effort: string
      /** Attempt number when this start follows N consecutive failed turns
       *  (orchestrator respawn / user resend) — renders a "retry N" chip. */
      retry?: number
      ts: number
      key: string
    }
  | {
      type: 'agent-crashed'
      reason: string
      /** Machine classification off the `agent-end` payload (`errorKind`),
       *  e.g. 'auth_expired' — drives remediation hints. */
      errorKind?: string
      /** True for a process crash (`status: "crashed"`); false for a turn
       *  that completed with an error result (e.g. auth failure). */
      crashed?: boolean
      /** Exit code / stderr tail off the `agent-end` payload — expandable
       *  in the crash row so debugging doesn't require the API. */
      exitCode?: number
      stderr?: string
      ts: number
      key: string
    }
  | {
      type: 'handover-aborted'
      from: string
      compaction: boolean
      /** Why the doc turn failed (provider error, e.g. an expired login's
       *  401); null for a user-initiated cancel. Drives the failed-
       *  compaction prompt (log in again / change sessions). */
      reason: string | null
      ts: number
      key: string
    }
  | { type: 'handover-start'; to: string; compaction: boolean; ts: number; key: string }
  | {
      type: 'handover'
      from: string
      to: string
      doc: string
      compaction: boolean
      ts: number
      key: string
    }
  | { type: 'interrupt'; ts: number; key: string }
  | {
      type: 'question'
      questionId: string
      /** ControlRequest correlation id — answers echo it as `request_id`. */
      requestId?: string
      /** ControlRequest type for generic (non-AskUserQuestion) requests. */
      requestType?: string
      questions: QuestionItem[]
      key: string
    }
  | {
      type: 'question-resolved'
      questionId: string
      questions: QuestionItem[]
      answers: Record<string, unknown>
      key: string
    }
  | {
      /** Fallback for event kinds this build doesn't recognize (plugin
       *  providers, future backend kinds): rendered as a collapsed row
       *  instead of being silently dropped. */
      type: 'unknown'
      kind: string
      data: Record<string, unknown>
      ts: number
      key: string
    }

/** Flatten a display item to the text the in-transcript search matches
 *  against. Lives next to DisplayItem so a new variant gets added to both. */
export function displayItemSearchText(item: DisplayItem): string {
  switch (item.type) {
    case 'user':
      return item.preHatchOriginal ? `${item.text}\n${item.preHatchOriginal}` : item.text
    case 'pre-hatch':
    case 'assistant':
    case 'thinking':
    case 'status':
    case 'system':
      return item.text
    case 'tool': {
      const parts = [item.toolName]
      if (item.input) parts.push(JSON.stringify(item.input))
      if (typeof item.output === 'string') parts.push(item.output)
      else if (item.output) parts.push(JSON.stringify(item.output))
      if (item.error) parts.push(item.error)
      if (item.diff) parts.push(item.diff.path, item.diff.diff)
      return parts.join('\n')
    }
    case 'file-diff':
      return `${item.diff.path}\n${item.diff.diff}`
    case 'step':
      return item.label
    case 'agent-start':
      return item.model
    case 'agent-crashed':
      return item.stderr ? `${item.reason}\n${item.stderr}` : item.reason
    case 'handover':
      return item.doc
    case 'question':
    case 'question-resolved':
      return item.questions
        .map((q) => [q.header ?? '', q.question, ...(q.options ?? [])].join('\n'))
        .join('\n')
    default:
      return ''
  }
}

/** Derive agent status from events for the toolbar indicator. */
export type AgentStatus = 'idle' | 'working' | 'tool' | 'crashed' | 'error' | 'questioning'

export function deriveAgentStatus(events: Event[]): AgentStatus {
  for (let i = events.length - 1; i >= 0; i--) {
    const kind = events[i].kind
    if (kind === 'agent-end') {
      // A crashed turn ends the process too — reporting it as "Idle" told the
      // user everything was fine while an "Agent crashed" row sat right below
      // the pill. `status` is camelCase-free, but tolerate a snake_case
      // spelling in case a provider ever emits one. A Completed turn that
      // carries an `error` (the CLI's is_error result, e.g. an expired
      // login) is just as failed — 'error', not 'idle'.
      const data = events[i].data
      const status = (data.status as string) ?? (data.agent_status as string)
      if (status === 'crashed') return 'crashed'
      return typeof data.error === 'string' && data.error !== '' ? 'error' : 'idle'
    }
    if (kind === 'pre-hatch' || kind === 'pre-ignite') return 'working'
    if (kind === 'question') {
      // Check if resolved later
      const qId = events[i].id
      const resolved = events
        .slice(i + 1)
        .some(
          (e) =>
            e.kind === 'question-resolved' &&
            (e.data.question_id === qId || e.data.questionId === qId),
        )
      if (!resolved) return 'questioning'
    }
    if (kind === 'agent-tool-start') {
      // Check if ended later. Backend emits camelCase `toolUseId`; tolerate
      // the snake_case spelling too in case older events use it.
      const startData = events[i].data
      const toolUseId =
        (startData.toolUseId as string) ?? (startData.tool_use_id as string) ?? events[i].id
      const ended = events.slice(i + 1).some((e) => {
        if (e.kind !== 'agent-tool-end') return false
        const endId = (e.data.toolUseId as string) ?? (e.data.tool_use_id as string)
        return endId === toolUseId
      })
      if (!ended) return 'tool'
    }
    if (kind === 'agent-start') return 'working'
    // The user sent a message and the process hasn't reported in yet — that's
    // the same "working" the thinking indicator shows, and it clears a stale
    // `crashed` pill as soon as the next turn is dispatched.
    if (kind === 'user') return 'working'
  }
  return 'idle'
}

export function getStatusLabel(status: AgentStatus): string {
  switch (status) {
    case 'idle':
      return 'Idle'
    case 'working':
      return 'Working...'
    case 'tool':
      return 'Using tool...'
    case 'crashed':
      return 'Crashed'
    case 'error':
      return 'Error'
    case 'questioning':
      return 'Awaiting answer'
  }
}

export function getStatusDotClass(status: AgentStatus): string {
  switch (status) {
    case 'idle':
      return 'status-dot status-dot-idle'
    case 'working':
      return 'status-dot status-dot-working'
    case 'tool':
      return 'status-dot status-dot-tool'
    case 'crashed':
    case 'error':
      return 'status-dot status-dot-crashed'
    case 'questioning':
      return 'status-dot status-dot-questioning'
  }
}

/** Mark any tool blocks still flagged as running as ended (with a fallback
 * error message). Defends against the agent dying mid-tool, or any other
 * code path that drops the matching agent-tool-end. */
function closeOpenTools(
  items: DisplayItem[],
  openTools: Map<string, number>,
  reason: string,
  endTs?: number,
): void {
  for (const idx of openTools.values()) {
    const item = items[idx]
    if (item?.type === 'tool' && item.isRunning) {
      items[idx] = {
        ...item,
        isRunning: false,
        error: item.error ?? reason,
        endTs: item.endTs ?? endTs,
      }
    }
  }
  openTools.clear()
}

/**
 * Pull attachment metadata off a `user` event for the chat bubble's
 * "image attached" indicator. The backend records a rich `attachments`
 * array ({filename, mime_type}) for every provider; older events (and any
 * the FE sent before that field existed) only carry `attachmentIds`, so we
 * fall back to a bare placeholder per id. Returns undefined when the turn
 * had no attachments, so the bubble renders exactly as before.
 */
function readAttachments(ev: Event): MessageAttachment[] | undefined {
  const rich = ev.data.attachments
  if (Array.isArray(rich) && rich.length > 0) {
    return rich.map((a) => {
      const obj = (a ?? {}) as Record<string, unknown>
      return {
        filename: (obj.filename as string) ?? 'attachment',
        mimeType: (obj.mime_type as string) ?? (obj.mimeType as string) ?? '',
        id: typeof obj.id === 'string' ? obj.id : undefined,
      }
    })
  }
  const ids = ev.data.attachmentIds
  if (Array.isArray(ids) && ids.length > 0) {
    return ids.map((x) => ({
      filename: 'attachment',
      mimeType: '',
      id: typeof x === 'string' ? x : undefined,
    }))
  }
  return undefined
}

/**
 * Pull any images off an `agent-tool-end` event. Tools that return images
 * (Playwright MCP `browser_take_screenshot`, any image-returning MCP server)
 * carry them as `[{mimeType, id}]` blob references — or inline as
 * `[{mimeType, dataBase64}]` on legacy events, which must keep rendering.
 * Returns undefined when the tool returned no images, so the tool block
 * renders exactly as before.
 */
function readToolImages(ev: Event): ToolImage[] | undefined {
  const raw = ev.data.images
  if (!Array.isArray(raw) || raw.length === 0) return undefined
  const images: ToolImage[] = []
  for (const entry of raw) {
    const obj = (entry ?? {}) as Record<string, unknown>
    const dataBase64 = (obj.dataBase64 as string) ?? (obj.data_base64 as string)
    const id = typeof obj.id === 'string' && obj.id !== '' ? obj.id : undefined
    if (!dataBase64 && !id) continue
    images.push({
      mimeType: (obj.mimeType as string) ?? (obj.mime_type as string) ?? 'image/png',
      dataBase64: dataBase64 || undefined,
      id,
    })
  }
  return images.length > 0 ? images : undefined
}

/** Mutable state threaded through the incremental display fold. Owns the
 *  `items` array; consumers get a filtered copy from `foldResult` so live
 *  stream buffers and superseded pre-hatch rows never leak in. */
interface FoldState {
  items: DisplayItem[]
  /** Events consumed so far — the fold resumes from this index. */
  consumed: number
  firstEventId: string | null
  lastEventId: string | null
  assistantBuffer: string
  assistantKey: string
  assistantTs: number
  thinkingBuffer: string
  thinkingKey: string
  thinkingTs: number
  /** tool_use_id -> index in items, for tool blocks still running. */
  openTools: Map<string, number>
  /** Dedupe tool starts from streaming + snapshot. */
  seenToolIds: Set<string>
  pendingInterrupt: boolean
  /** Consecutive turns that ended in a failure (crash or error result) —
   *  the next agent-start renders as "retry N". */
  errorStreak: number
  /** ts of the latest `user` / `agent-start` — anchors the turn duration
   *  shown on the usage chip. */
  turnAnchorTs: number
  /** contextTokens from the previous usage report — drives the delta. */
  prevContextTokens: number
  /** question event id -> index in items, so a later `question-resolved`
   *  swaps the pending card in place. */
  questionItems: Map<string, number>
  /** Indices of pre-hatch items not yet superseded by a later `user`. */
  livePreHatch: number[]
}

function newFoldState(): FoldState {
  return {
    items: [],
    consumed: 0,
    firstEventId: null,
    lastEventId: null,
    assistantBuffer: '',
    assistantKey: '',
    assistantTs: 0,
    thinkingBuffer: '',
    thinkingKey: '',
    thinkingTs: 0,
    openTools: new Map(),
    seenToolIds: new Set(),
    pendingInterrupt: false,
    errorStreak: 0,
    turnAnchorTs: 0,
    prevContextTokens: 0,
    questionItems: new Map(),
    livePreHatch: [],
  }
}

function flushThinking(st: FoldState): void {
  if (st.thinkingBuffer) {
    st.items.push({
      type: 'thinking',
      text: st.thinkingBuffer,
      key: st.thinkingKey,
      ts: st.thinkingTs,
    })
    st.thinkingBuffer = ''
    st.thinkingKey = ''
    st.thinkingTs = 0
  }
}

function flushAssistant(st: FoldState): void {
  // Thinking precedes the text it produced — keep that order in the feed.
  flushThinking(st)
  if (st.assistantBuffer) {
    st.items.push({
      type: 'assistant',
      text: st.assistantBuffer,
      key: st.assistantKey,
      ts: st.assistantTs,
    })
    st.assistantBuffer = ''
    st.assistantKey = ''
    st.assistantTs = 0
  }
}

/** Event kinds that intentionally render nothing in the chat feed, so the
 *  unknown-kind fallback must not pick them up:
 *  - `todo` snapshots drive the TodoPanel;
 *  - `question-resolved` folds into its `question` card;
 *  - `session-read` is per-open unread-sync bookkeeping;
 *  - the `*-requested` worker-intent markers accompany an already-visible
 *    MCP tool block;
 *  - `worker-finding` feeds the worker-comms panel;
 *  - `plan-proposed` / `plan-deleted` drive the plan view;
 *  - `model-switch` is the autoswitch audit record (the next `agent-start`
 *    row already shows the new model). */
const HIDDEN_KINDS = new Set([
  'todo',
  'question-resolved',
  'session-read',
  'complete-step-requested',
  'finish-requested',
  'wont-do-requested',
  'ask-user-requested',
  'worker-finding',
  'plan-proposed',
  'plan-deleted',
  'model-switch',
])

/** Tool cards a `file-diff` event may attach to, by bare name (Peckboard
 *  MCP file tools, the native Claude file tools, and cursor's `edit` —
 *  which writes new files too, so it covers both). */
const EDIT_TOOL_BARE_NAMES = new Set([
  'edit_file',
  'write_file',
  'Edit',
  'Write',
  'MultiEdit',
  'edit',
  'write',
])

function foldEvent(st: FoldState, ev: Event): void {
  const items = st.items
  switch (ev.kind) {
    case 'user': {
      flushAssistant(st)
      st.turnAnchorTs = ev.ts
      // Any user turn supersedes earlier parked pre-hatch bubbles — the
      // enriched delivery, or the user typing past a dead pre-hatch.
      for (const idx of st.livePreHatch) {
        const it = items[idx]
        if (it.type === 'pre-hatch') items[idx] = { ...it, superseded: true }
      }
      st.livePreHatch.length = 0
      const text = (ev.data.text as string) ?? JSON.stringify(ev.data)
      // `pre_ignite` is the legacy spelling from before the pre-hatcher
      // rename — old transcripts keep rendering.
      const pi = (ev.data.pre_hatch ?? ev.data.pre_ignite) as Record<string, unknown> | undefined
      items.push({
        type: 'user',
        text,
        key: ev.id,
        ts: ev.ts,
        attachments: readAttachments(ev),
        preHatchOriginal: typeof pi?.original === 'string' ? pi.original : undefined,
        preHatchEnriched: pi?.enriched === true,
      })
      break
    }
    case 'agent-text': {
      flushThinking(st)
      const chunk = (ev.data.text as string) ?? ''
      if (!st.assistantKey) {
        st.assistantKey = ev.id
        st.assistantTs = ev.ts
      }
      st.assistantBuffer += chunk
      break
    }
    case 'agent-thinking': {
      // Close any streaming text bubble — thinking starts its own block.
      // (thinkingBuffer is empty whenever text was streaming, so the
      // flushThinking inside flushAssistant is a no-op here.)
      if (st.assistantBuffer) flushAssistant(st)
      const chunk = (ev.data.text as string) ?? ''
      if (!st.thinkingKey) {
        st.thinkingKey = ev.id
        st.thinkingTs = ev.ts
      }
      st.thinkingBuffer += chunk
      break
    }
    case 'agent-usage': {
      flushAssistant(st)
      const turnSeq = typeof ev.data.turnSeq === 'number' ? ev.data.turnSeq : null
      const slice = {
        model: (ev.data.model as string) ?? '',
        input: (ev.data.inputTokens as number) ?? 0,
        output: (ev.data.outputTokens as number) ?? 0,
        cacheRead: (ev.data.cacheReadTokens as number) ?? 0,
        cacheCreation: (ev.data.cacheCreationTokens as number) ?? 0,
      }
      const ctx = typeof ev.data.contextTokens === 'number' ? ev.data.contextTokens : 0
      const contextDelta = ctx > 0 && st.prevContextTokens > 0 ? ctx - st.prevContextTokens : null
      if (ctx > 0) st.prevContextTokens = ctx
      const durationMs =
        st.turnAnchorTs > 0 && ev.ts >= st.turnAnchorTs ? ev.ts - st.turnAnchorTs : null
      const last = items[items.length - 1]
      if (last?.type === 'turn-usage' && last.turnSeq !== null && last.turnSeq === turnSeq) {
        // Multi-model turn: fold this model's slice into the same chip.
        // Replace rather than mutate so the memoized row re-renders.
        items[items.length - 1] = {
          ...last,
          slices: [...last.slices, slice],
          outputTokens: last.outputTokens + slice.output,
          contextTokens: ctx > 0 ? ctx : last.contextTokens,
          contextDelta: contextDelta ?? last.contextDelta,
          durationMs: durationMs ?? last.durationMs,
        }
      } else {
        items.push({
          type: 'turn-usage',
          turnSeq,
          slices: [slice],
          outputTokens: slice.output,
          contextTokens: ctx,
          contextDelta,
          durationMs,
          key: ev.id,
          ts: ev.ts,
        })
      }
      break
    }
    case 'agent-tool-start': {
      flushAssistant(st)
      const toolName = (ev.data.name as string) ?? (ev.data.tool_name as string) ?? 'tool'
      const input = (ev.data.input as Record<string, unknown>) ?? undefined
      const toolUseId = (ev.data.toolUseId as string) ?? (ev.data.tool_use_id as string) ?? ev.id
      // Skip duplicate tool starts (CLI emits both streaming + snapshot events)
      if (st.seenToolIds.has(toolUseId)) break
      st.seenToolIds.add(toolUseId)
      const idx = items.length
      items.push({ type: 'tool', toolName, input, isRunning: true, key: ev.id, startTs: ev.ts })
      st.openTools.set(toolUseId, idx)
      break
    }
    case 'agent-tool-end': {
      flushAssistant(st)
      const toolUseId = (ev.data.toolUseId as string) ?? (ev.data.tool_use_id as string) ?? ''
      const images = readToolImages(ev)
      const idx = st.openTools.get(toolUseId)
      if (idx !== undefined) {
        const existing = items[idx] as Extract<DisplayItem, { type: 'tool' }>
        const errorText = ev.data.error as string | undefined
        const output = (ev.data.output as Record<string, unknown> | string) ?? undefined
        items[idx] = {
          ...existing,
          isRunning: false,
          output,
          error: errorText,
          images,
          endTs: ev.ts,
        }
        st.openTools.delete(toolUseId)
      } else {
        const toolName = (ev.data.name as string) ?? (ev.data.tool_name as string) ?? 'tool'
        const errorText = ev.data.error as string | undefined
        const output = (ev.data.output as Record<string, unknown> | string) ?? undefined
        items.push({
          type: 'tool',
          toolName,
          output,
          error: errorText,
          images,
          isRunning: false,
          key: ev.id,
        })
      }
      break
    }
    case 'agent-start': {
      flushAssistant(st)
      st.pendingInterrupt = false
      st.turnAnchorTs = ev.ts
      const model = (ev.data.model as string) ?? 'default'
      // Strip provider prefix for display
      const displayModel = model.replace(/^claude:/, '')
      const effort = (ev.data.effort as string) ?? ''
      items.push({
        type: 'agent-start',
        model: displayModel,
        effort,
        retry: st.errorStreak > 0 ? st.errorStreak : undefined,
        ts: ev.ts,
        key: ev.id,
      })
      break
    }
    case 'agent-end': {
      flushAssistant(st)
      closeOpenTools(items, st.openTools, 'agent ended before tool completed', ev.ts)
      const reason = (ev.data.reason as string) ?? 'unknown error'
      const wasInterrupted = st.pendingInterrupt && reason === 'interrupted'
      st.pendingInterrupt = false
      if (wasInterrupted) {
        st.errorStreak = 0
        break
      }
      const crashed = (ev.data.status as string) === 'crashed'
      // A Completed turn can still carry an error (the CLI's is_error
      // result, e.g. an expired login's 401) — render it as a failure,
      // not as the green ready line.
      const errorText =
        typeof ev.data.error === 'string' && ev.data.error !== '' ? ev.data.error : undefined
      if (crashed || errorText !== undefined) {
        const exitCode = typeof ev.data.exitCode === 'number' ? ev.data.exitCode : undefined
        const stderr =
          typeof ev.data.stderr === 'string' && ev.data.stderr.trim() !== ''
            ? ev.data.stderr
            : undefined
        const errorKind =
          typeof ev.data.errorKind === 'string' && ev.data.errorKind !== ''
            ? ev.data.errorKind
            : undefined
        st.errorStreak += 1
        items.push({
          type: 'agent-crashed',
          reason: crashed ? reason : (errorText ?? reason),
          errorKind,
          crashed,
          exitCode,
          stderr,
          key: ev.id,
          ts: ev.ts,
        })
      } else {
        st.errorStreak = 0
        items.push({
          type: 'status',
          text: 'Ready for your next message.',
          key: ev.id,
          ts: ev.ts,
        })
      }
      break
    }
    case 'interrupt': {
      flushAssistant(st)
      closeOpenTools(items, st.openTools, 'interrupted', ev.ts)
      items.push({ type: 'interrupt', ts: ev.ts, key: ev.id })
      st.pendingInterrupt = true
      break
    }
    case 'handover-start': {
      flushAssistant(st)
      const to = (ev.data.to as string) ?? 'the new model'
      const compaction = (ev.data.compaction as boolean) ?? false
      items.push({ type: 'handover-start', to, compaction, ts: ev.ts, key: ev.id })
      break
    }
    case 'handover': {
      flushAssistant(st)
      st.prevContextTokens = 0
      // A handover restarts the conversation — the next start is a fresh
      // attempt, not a retry of the failed streak.
      st.errorStreak = 0
      items.push({
        type: 'handover',
        from: (ev.data.from as string) ?? '',
        to: (ev.data.to as string) ?? '',
        doc: (ev.data.doc as string) ?? '',
        compaction: (ev.data.compaction as boolean) ?? false,
        ts: ev.ts,
        key: ev.id,
      })
      break
    }
    case 'handover-aborted': {
      flushAssistant(st)
      items.push({
        type: 'handover-aborted',
        from: (ev.data.from as string) ?? '',
        compaction: (ev.data.compaction as boolean) ?? false,
        reason: typeof ev.data.reason === 'string' && ev.data.reason !== '' ? ev.data.reason : null,
        ts: ev.ts,
        key: ev.id,
      })
      break
    }
    case 'system': {
      flushAssistant(st)
      const rawText =
        typeof ev.data.text === 'string'
          ? ev.data.text
          : typeof ev.data.message === 'string'
            ? ev.data.message
            : undefined
      // A plugin provider (or a future backend kind) can emit a `system`
      // event carrying neither field. Stringifying the payload dumps a raw
      // object blob into the feed next to an ℹ️ icon; label the row instead
      // and park the payload behind the same <details> treatment the
      // unrecognized-event row uses.
      const text = rawText ?? 'System notice'
      const detail = rawText === undefined ? ev.data : undefined
      const reportFolder = ev.data.reportFolder as string | undefined
      const reportFile = ev.data.reportFile as string | undefined
      const last = items[items.length - 1]
      if (
        last?.type === 'system' &&
        last.text === text &&
        !last.detail &&
        !detail &&
        !last.reportFolder &&
        !reportFolder
      ) {
        // Coalesce runs of identical notices — N heartbeats become one row.
        items[items.length - 1] = { ...last, count: (last.count ?? 1) + 1, ts: ev.ts }
      } else {
        items.push({
          type: 'system',
          text,
          detail,
          key: ev.id,
          reportFolder,
          reportFile,
          ts: ev.ts,
        })
      }
      break
    }
    case 'step-change': {
      flushAssistant(st)
      const label = (ev.data.step as string) ?? (ev.data.label as string) ?? 'Step'
      items.push({ type: 'step', label, key: ev.id })
      break
    }
    case 'file-diff': {
      flushAssistant(st)
      const d = ev.data as Record<string, unknown>
      if (typeof d.path !== 'string' || typeof d.diff !== 'string') break
      const diff: FileDiff = {
        path: d.path,
        diff: d.diff,
        added: typeof d.added === 'number' ? d.added : 0,
        removed: typeof d.removed === 'number' ? d.removed : 0,
        truncated: d.truncated === true,
        created: d.created === true,
      }
      // Attach to a matching edit/write tool card. Parallel multi-file
      // edits interleave, so the owner isn't necessarily the most recent
      // tool row — scan back across the whole open-tool window and match
      // by path; fall back to a standalone card when nothing matches.
      let attached = false
      const openIdxs = [...st.openTools.values()]
      const floor = openIdxs.length > 0 ? Math.min(...openIdxs) : items.length - 1
      for (let i = items.length - 1; i >= Math.max(0, floor); i--) {
        const it = items[i]
        if (it.type !== 'tool' || it.diff) continue
        const bare = it.toolName.replace(/^mcp__.+?__/, '')
        if (!EDIT_TOOL_BARE_NAMES.has(bare)) continue
        const inputPath = it.input?.path ?? it.input?.file_path
        if (inputPath === diff.path) {
          items[i] = { ...it, diff }
          attached = true
          break
        }
      }
      if (!attached) {
        items.push({ type: 'file-diff', diff, ts: ev.ts, key: ev.id })
      }
      break
    }
    case 'question': {
      flushAssistant(st)
      const requestId = typeof ev.data.requestId === 'string' ? ev.data.requestId : undefined
      const requestType = typeof ev.data.requestType === 'string' ? ev.data.requestType : undefined
      // Parse questions array from event data; generic ControlRequests
      // (no `questions`) carry their text in `payload` instead — render
      // its fields rather than the raw event JSON.
      let questions: QuestionItem[]
      if (Array.isArray(ev.data.questions)) {
        questions = (ev.data.questions as QuestionItem[]).map((q) => ({
          question: q.question ?? '',
          header: q.header,
          multiSelect: q.multiSelect,
          options: q.options,
          optionObjects: q.optionObjects,
        }))
      } else {
        const payload =
          ev.data.payload && typeof ev.data.payload === 'object' && !Array.isArray(ev.data.payload)
            ? (ev.data.payload as Record<string, unknown>)
            : undefined
        const direct =
          (typeof payload?.text === 'string' && payload.text) ||
          (typeof payload?.question === 'string' && payload.question) ||
          (typeof ev.data.text === 'string' && ev.data.text) ||
          (typeof ev.data.question === 'string' && ev.data.question) ||
          ''
        const text = direct
          ? direct
          : payload
            ? Object.entries(payload)
                .map(([k, v]) => `${k}: ${typeof v === 'string' ? v : JSON.stringify(v)}`)
                .join('\n')
            : JSON.stringify(ev.data)
        const header = requestType && requestType !== 'question' ? requestType : undefined
        questions = [{ question: text, header }]
      }
      st.questionItems.set(ev.id, items.length)
      if (requestId) st.questionItems.set(requestId, items.length)
      items.push({
        type: 'question',
        questionId: ev.id,
        requestId,
        requestType,
        questions,
        key: ev.id,
      })
      break
    }
    case 'question-resolved': {
      // Swap the pending question card in place. A resolution whose
      // question sits on an unloaded older page renders nothing — same
      // as the previous full-rebuild behaviour.
      const qId = (ev.data.question_id as string) ?? (ev.data.questionId as string) ?? ''
      const reqId = (ev.data.request_id as string) ?? (ev.data.requestId as string) ?? ''
      const idx = st.questionItems.get(qId) ?? (reqId ? st.questionItems.get(reqId) : undefined)
      if (idx === undefined) break
      const existing = items[idx]
      if (existing.type !== 'question') break
      items[idx] = {
        type: 'question-resolved',
        questionId: existing.questionId,
        questions: existing.questions,
        answers: (ev.data.answers as Record<string, unknown>) ?? {},
        key: existing.key,
      }
      break
    }
    // `pre-ignite` is the legacy event kind from before the pre-hatcher
    // rename — old transcripts render identically (minus the live feed).
    case 'pre-hatch':
    case 'pre-ignite': {
      flushAssistant(st)
      const text = (ev.data.text as string) ?? ''
      st.livePreHatch.push(items.length)
      items.push({
        type: 'pre-hatch',
        text,
        key: ev.id,
        ts: ev.ts,
        tempSessionId:
          typeof ev.data.temp_session_id === 'string' ? ev.data.temp_session_id : undefined,
        model: typeof ev.data.model === 'string' ? ev.data.model : undefined,
      })
      break
    }
    default: {
      if (HIDDEN_KINDS.has(ev.kind)) break
      // Unknown kinds (plugin providers, future backends) render a
      // collapsed fallback row instead of disappearing.
      flushAssistant(st)
      items.push({ type: 'unknown', kind: ev.kind, data: ev.data, ts: ev.ts, key: ev.id })
      break
    }
  }
}

function foldResult(st: FoldState): DisplayItem[] {
  const out: DisplayItem[] = []
  for (const it of st.items) {
    if (it.type === 'pre-hatch' && it.superseded) continue
    out.push(it)
  }
  // Live stream buffers render as trailing rows without being committed,
  // so the next chunk keeps growing the same bubble. Keys are the first
  // chunk's event id — stable across renders and across the final flush.
  if (st.thinkingBuffer) {
    out.push({ type: 'thinking', text: st.thinkingBuffer, key: st.thinkingKey, ts: st.thinkingTs })
  }
  if (st.assistantBuffer) {
    out.push({
      type: 'assistant',
      text: st.assistantBuffer,
      key: st.assistantKey,
      ts: st.assistantTs,
    })
  }
  return out
}

/**
 * Incremental display-item builder. Returns a function that folds a
 * session's event list into display items, reusing all work from the
 * previous call when the new list is an append-only extension (the common
 * case: one WS event — or one token chunk — at a time). Any other shape
 * (session switch, "Load older" prepend, snapshot merge) is detected via
 * the first/last consumed event ids and triggers a full rebuild.
 *
 * Item object identity is stable across calls unless the item actually
 * changed, so `React.memo` rows skip re-rendering untouched history.
 */
export function createDisplayItemsFolder(): (events: Event[]) => DisplayItem[] {
  let st = newFoldState()
  return (events: Event[]) => {
    const stale =
      st.consumed > 0 &&
      (events.length < st.consumed ||
        events[0]?.id !== st.firstEventId ||
        events[st.consumed - 1]?.id !== st.lastEventId)
    if (stale) st = newFoldState()
    for (let i = st.consumed; i < events.length; i++) {
      foldEvent(st, events[i])
    }
    st.consumed = events.length
    st.firstEventId = events.length > 0 ? events[0].id : null
    st.lastEventId = events.length > 0 ? events[events.length - 1].id : null
    return foldResult(st)
  }
}

/** One-shot build — a fresh fold over the full list. */
export function buildDisplayItems(events: Event[]): DisplayItem[] {
  return createDisplayItemsFolder()(events)
}
/**
 * The newest question still awaiting an answer, as the display item a
 * question card renders — or null when the last question was answered (or
 * dismissed, or there never was one).
 *
 * Folds that single event rather than re-parsing the payload, so an
 * AskUserQuestion, a generic control request and a bare `{text}` all read
 * the same here as they do in the chat feed. Used by surfaces that pin the
 * question outside the feed (the review screen shows it above the document).
 */
export function findOpenQuestion(
  events: Event[],
): Extract<DisplayItem, { type: 'question' }> | null {
  for (let i = events.length - 1; i >= 0; i--) {
    const ev = events[i]
    if (ev.kind !== 'question') continue
    const answered = events
      .slice(i + 1)
      .some(
        (e) =>
          e.kind === 'question-resolved' &&
          (e.data.question_id === ev.id || e.data.questionId === ev.id),
      )
    if (answered) return null
    const st = newFoldState()
    foldEvent(st, ev)
    const item = st.items[0]
    return item?.type === 'question' ? item : null
  }
  return null
}

export function formatTime(ts: number): string {
  if (!ts) return ''
  return new Date(ts).toLocaleTimeString([], {
    hour: 'numeric',
    minute: '2-digit',
    second: '2-digit',
  })
}

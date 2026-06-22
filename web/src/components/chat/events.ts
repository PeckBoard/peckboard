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
}

/** An image returned by a tool (e.g. a Playwright MCP screenshot), carried
 *  inline on the `agent-tool-end` event as base64. */
export interface ToolImage {
  mimeType: string
  dataBase64: string
}

/** A display item derived from one or more raw events. */
export type DisplayItem =
  | { type: 'user'; text: string; key: string; ts: number; attachments?: MessageAttachment[] }
  | { type: 'assistant'; text: string; key: string; ts: number }
  | {
      type: 'tool'
      toolName: string
      input?: Record<string, unknown>
      output?: Record<string, unknown>
      error?: string
      images?: ToolImage[]
      isRunning: boolean
      key: string
    }
  | { type: 'status'; text: string; key: string; ts: number }
  | {
      type: 'system'
      text: string
      key: string
      reportFolder?: string
      reportFile?: string
      ts: number
    }
  | { type: 'step'; label: string; key: string }
  | { type: 'agent-start'; model: string; effort: string; ts: number; key: string }
  | { type: 'agent-crashed'; reason: string; ts: number; key: string }
  | { type: 'interrupt'; ts: number; key: string }
  | { type: 'question'; questionId: string; questions: QuestionItem[]; key: string }
  | {
      type: 'question-resolved'
      questionId: string
      questions: QuestionItem[]
      answers: Record<string, unknown>
      key: string
    }

/** Derive agent status from events for the toolbar indicator. */
export type AgentStatus = 'idle' | 'working' | 'tool' | 'crashed' | 'questioning'

export function deriveAgentStatus(events: Event[]): AgentStatus {
  for (let i = events.length - 1; i >= 0; i--) {
    const kind = events[i].kind
    if (kind === 'agent-end') return 'idle'
    if (kind === 'agent-error' || kind === 'error') return 'crashed'
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
): void {
  for (const idx of openTools.values()) {
    const item = items[idx]
    if (item?.type === 'tool' && item.isRunning) {
      items[idx] = {
        ...item,
        isRunning: false,
        error: item.error ?? reason,
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
      }
    })
  }
  const ids = ev.data.attachmentIds
  if (Array.isArray(ids) && ids.length > 0) {
    return ids.map(() => ({ filename: 'attachment', mimeType: '' }))
  }
  return undefined
}

/**
 * Pull any images off an `agent-tool-end` event. Tools that return images
 * (Playwright MCP `browser_take_screenshot`, any image-returning MCP server)
 * carry them inline as `[{mimeType, dataBase64}]`. Returns undefined when the
 * tool returned no images, so the tool block renders exactly as before.
 */
function readToolImages(ev: Event): ToolImage[] | undefined {
  const raw = ev.data.images
  if (!Array.isArray(raw) || raw.length === 0) return undefined
  const images: ToolImage[] = []
  for (const entry of raw) {
    const obj = (entry ?? {}) as Record<string, unknown>
    const dataBase64 = (obj.dataBase64 as string) ?? (obj.data_base64 as string)
    if (!dataBase64) continue
    images.push({
      mimeType: (obj.mimeType as string) ?? (obj.mime_type as string) ?? 'image/png',
      dataBase64,
    })
  }
  return images.length > 0 ? images : undefined
}

export function buildDisplayItems(events: Event[]): DisplayItem[] {
  const items: DisplayItem[] = []
  let assistantBuffer = ''
  let assistantKey = ''
  let assistantTs = 0

  const flushAssistant = () => {
    if (assistantBuffer) {
      items.push({ type: 'assistant', text: assistantBuffer, key: assistantKey, ts: assistantTs })
      assistantBuffer = ''
      assistantKey = ''
      assistantTs = 0
    }
  }

  // Collect resolved question ids
  const resolvedQuestions = new Set<string>()
  for (const ev of events) {
    if (ev.kind === 'question-resolved') {
      const qId = (ev.data.question_id as string) ?? (ev.data.questionId as string) ?? ''
      if (qId) resolvedQuestions.add(qId)
    }
  }

  // Track open tools by their tool_use_id
  const openTools = new Map<string, number>() // tool_use_id -> index in items
  const seenToolIds = new Set<string>() // dedupe tool starts from streaming + snapshot

  // When the user hits the Interrupt button we get two events back-to-back:
  // an `interrupt` (from the HTTP route) and an `agent-end` with status
  // crashed / reason "interrupted" (from the provider's stream loop winding
  // down). The dedicated interrupt notice already tells the user what
  // happened, so suppress the paired crash banner — without this the UI
  // looks like the agent broke instead of acknowledging the user's action.
  let pendingInterrupt = false

  for (const ev of events) {
    switch (ev.kind) {
      case 'user': {
        flushAssistant()
        const text = (ev.data.text as string) ?? JSON.stringify(ev.data)
        items.push({ type: 'user', text, key: ev.id, ts: ev.ts, attachments: readAttachments(ev) })
        break
      }
      case 'agent-text': {
        const chunk = (ev.data.text as string) ?? ''
        if (!assistantKey) {
          assistantKey = ev.id
          assistantTs = ev.ts
        }
        assistantBuffer += chunk
        break
      }
      case 'agent-tool-start': {
        flushAssistant()
        const toolName = (ev.data.name as string) ?? (ev.data.tool_name as string) ?? 'tool'
        const input = (ev.data.input as Record<string, unknown>) ?? undefined
        const toolUseId = (ev.data.toolUseId as string) ?? (ev.data.tool_use_id as string) ?? ev.id
        // Skip duplicate tool starts (CLI emits both streaming + snapshot events)
        if (seenToolIds.has(toolUseId)) break
        seenToolIds.add(toolUseId)
        const idx = items.length
        items.push({ type: 'tool', toolName, input, isRunning: true, key: ev.id })
        openTools.set(toolUseId, idx)
        break
      }
      case 'agent-tool-end': {
        flushAssistant()
        const toolUseId = (ev.data.toolUseId as string) ?? (ev.data.tool_use_id as string) ?? ''
        const images = readToolImages(ev)
        const idx = openTools.get(toolUseId)
        if (idx !== undefined) {
          const existing = items[idx] as Extract<DisplayItem, { type: 'tool' }>
          const errorText = ev.data.error as string | undefined
          const output = (ev.data.output as Record<string, unknown>) ?? undefined
          items[idx] = { ...existing, isRunning: false, output, error: errorText, images }
          openTools.delete(toolUseId)
        } else {
          const toolName = (ev.data.name as string) ?? (ev.data.tool_name as string) ?? 'tool'
          const errorText = ev.data.error as string | undefined
          const output = (ev.data.output as Record<string, unknown>) ?? undefined
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
        flushAssistant()
        pendingInterrupt = false
        const model = (ev.data.model as string) ?? 'default'
        // Strip provider prefix for display
        const displayModel = model.replace(/^claude:/, '')
        const effort = (ev.data.effort as string) ?? ''
        items.push({ type: 'agent-start', model: displayModel, effort, ts: ev.ts, key: ev.id })
        break
      }
      case 'agent-end': {
        flushAssistant()
        closeOpenTools(items, openTools, 'agent ended before tool completed')
        const reason = (ev.data.reason as string) ?? 'unknown error'
        const wasInterrupted = pendingInterrupt && reason === 'interrupted'
        pendingInterrupt = false
        if (wasInterrupted) {
          break
        }
        if ((ev.data.status as string) === 'crashed') {
          items.push({
            type: 'agent-crashed',
            reason,
            key: ev.id,
            ts: ev.ts,
          })
        } else {
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
        flushAssistant()
        closeOpenTools(items, openTools, 'interrupted')
        items.push({ type: 'interrupt', ts: ev.ts, key: ev.id })
        pendingInterrupt = true
        break
      }
      case 'system': {
        flushAssistant()
        const text =
          (ev.data.text as string) ?? (ev.data.message as string) ?? JSON.stringify(ev.data)
        const reportFolder = ev.data.reportFolder as string | undefined
        const reportFile = ev.data.reportFile as string | undefined
        items.push({ type: 'system', text, key: ev.id, reportFolder, reportFile, ts: ev.ts })
        break
      }
      case 'step-change': {
        flushAssistant()
        const label = (ev.data.step as string) ?? (ev.data.label as string) ?? 'Step'
        items.push({ type: 'step', label, key: ev.id })
        break
      }
      case 'question': {
        flushAssistant()
        // Parse questions array from event data, falling back to simple text
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
          const text =
            (ev.data.text as string) ?? (ev.data.question as string) ?? JSON.stringify(ev.data)
          questions = [{ question: text }]
        }

        if (resolvedQuestions.has(ev.id)) {
          // Find the matching resolved event to get answers
          const resolvedEv = events.find(
            (e) =>
              e.kind === 'question-resolved' &&
              ((e.data.question_id as string) === ev.id || (e.data.questionId as string) === ev.id),
          )
          const answers = (resolvedEv?.data.answers as Record<string, unknown>) ?? {}
          items.push({
            type: 'question-resolved',
            questionId: ev.id,
            questions,
            answers,
            key: ev.id,
          })
        } else {
          items.push({ type: 'question', questionId: ev.id, questions, key: ev.id })
        }
        break
      }
      default: {
        // Unknown event kinds: skip or render as system
        break
      }
    }
  }

  flushAssistant()
  return items
}

export function formatTime(ts: number): string {
  if (!ts) return ''
  return new Date(ts).toLocaleTimeString([], {
    hour: 'numeric',
    minute: '2-digit',
    second: '2-digit',
  })
}

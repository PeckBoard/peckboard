import { useEffect, useMemo, useState } from 'react'
import type { Event } from '../../types/api'
import { useSessionsStore } from '../../store/sessions'
import { useWsStore } from '../../store/ws'
import { EMPTY_EVENTS } from './events'

/** The first of these input keys present on a tool call becomes the detail
 *  text of its action line — the argument a human would want to see. */
const DETAIL_KEYS = [
  'path',
  'query',
  'name',
  'pattern',
  'url',
  'question',
  'command',
  'prompt',
] as const

/** Cap on rendered action lines; older ones collapse into a "+N earlier". */
const MAX_VISIBLE = 5

interface ActionLine {
  key: string
  label: string
  detail: string
  running: boolean
}

function detailFor(input: Record<string, unknown> | undefined): string {
  if (!input) return ''
  for (const k of DETAIL_KEYS) {
    const v = input[k]
    if (typeof v === 'string' && v.trim() !== '') return v
  }
  return ''
}

function truncate(s: string, max: number): string {
  return s.length <= max ? s : s.slice(0, max) + '…'
}

/** One line per tool call of the research session, in order, with a running
 *  flag for the still-open one. */
function deriveActionLines(events: Event[]): ActionLine[] {
  const lines: ActionLine[] = []
  const openByToolId = new Map<string, number>()
  for (const ev of events) {
    if (ev.kind === 'agent-tool-start') {
      const toolUseId = (ev.data.toolUseId as string) ?? (ev.data.tool_use_id as string) ?? ev.id
      // The CLI emits both streaming and snapshot starts for one call.
      if (openByToolId.has(toolUseId)) continue
      openByToolId.set(toolUseId, lines.length)
      lines.push({
        key: ev.id,
        label: (ev.data.name as string) ?? (ev.data.tool_name as string) ?? 'tool',
        detail: truncate(detailFor(ev.data.input as Record<string, unknown> | undefined), 80),
        running: true,
      })
    } else if (ev.kind === 'agent-tool-end') {
      const toolUseId = (ev.data.toolUseId as string) ?? (ev.data.tool_use_id as string) ?? ''
      const idx = openByToolId.get(toolUseId)
      if (idx !== undefined) lines[idx] = { ...lines[idx], running: false }
    } else if (ev.kind === 'agent-end') {
      for (let i = 0; i < lines.length; i++) {
        if (lines[i].running) lines[i] = { ...lines[i], running: false }
      }
    }
  }
  return lines
}

interface Props {
  /** The temp research session whose actions stream into the bubble.
   *  Absent on legacy `pre-ignite` events — the header line still shows. */
  tempSessionId?: string
  /** The model the research session runs on. */
  model?: string
  /** The chat session the pre-hatch is parked on — enables the cancel
   *  button (kill the research, send the original message untouched). */
  sessionId?: string
}

/**
 * Live activity feed inside the parked `pre-hatch` user bubble: subscribes
 * to the temp research session's events and lists its tool calls as they
 * happen, so the user sees exactly what the cheaper model is doing while
 * their message waits.
 */
export default function PreHatchActivity({ tempSessionId, model, sessionId }: Props) {
  const events = useSessionsStore((s) =>
    tempSessionId ? (s.eventsBySession[tempSessionId] ?? EMPTY_EVENTS) : EMPTY_EVENTS,
  )
  const fetchEvents = useSessionsStore((s) => s.fetchEvents)
  const appendEvent = useSessionsStore((s) => s.appendEvent)
  const subscribe = useWsStore((s) => s.subscribe)
  const unsubscribe = useWsStore((s) => s.unsubscribe)
  const addEventListener = useWsStore((s) => s.addEventListener)
  const removeEventListener = useWsStore((s) => s.removeEventListener)
  const cancelPreHatch = useSessionsStore((s) => s.cancelPreHatch)
  const [cancelling, setCancelling] = useState(false)

  useEffect(() => {
    if (!tempSessionId) return
    fetchEvents(tempSessionId)
    subscribe(tempSessionId)
    const listener = (event: Event) => {
      if (event.session_id === tempSessionId) appendEvent(event)
    }
    addEventListener(listener)
    return () => {
      removeEventListener(listener)
      unsubscribe(tempSessionId)
    }
  }, [
    tempSessionId,
    fetchEvents,
    subscribe,
    unsubscribe,
    addEventListener,
    removeEventListener,
    appendEvent,
  ])

  const handleCancel = async () => {
    if (!sessionId || cancelling) return
    setCancelling(true)
    try {
      await cancelPreHatch(sessionId)
      // The delivered `user` event arrives over WS and replaces the parked
      // bubble; stay disabled until it does.
    } catch (e) {
      console.warn('pre-hatch cancel failed', e)
      setCancelling(false)
    }
  }

  const lines = useMemo(() => deriveActionLines(events), [events])
  const hidden = Math.max(0, lines.length - MAX_VISIBLE)
  const visible = lines.slice(-MAX_VISIBLE)

  return (
    <div className="chat-prehatch-indicator" data-testid="chat-prehatch-activity">
      <div className="chat-prehatch-status">
        <span className="chat-prehatch-spinner" aria-hidden />
        Pre-hatching{model ? ` on ${model}` : ''} — a cheaper model is gathering context…
        {sessionId && (
          <button
            type="button"
            className="chat-prehatch-cancel"
            data-testid="chat-prehatch-cancel"
            onClick={handleCancel}
            disabled={cancelling}
            title="Stop the research and send your original message now"
          >
            {cancelling ? 'Cancelling…' : 'Cancel — send original'}
          </button>
        )}
      </div>
      {tempSessionId && (
        <div className="chat-prehatch-actions" data-testid="chat-prehatch-actions">
          {lines.length === 0 && (
            <div className="chat-prehatch-action">starting research session…</div>
          )}
          {hidden > 0 && (
            <div className="chat-prehatch-action chat-prehatch-more">
              +{hidden} earlier action{hidden === 1 ? '' : 's'}
            </div>
          )}
          {visible.map((l) => (
            <div
              key={l.key}
              className={`chat-prehatch-action${l.running ? ' chat-prehatch-action-running' : ''}`}
            >
              <span className="chat-prehatch-action-name">{l.label}</span>
              {l.detail && <span className="chat-prehatch-action-detail">{l.detail}</span>}
            </div>
          ))}
        </div>
      )}
    </div>
  )
}

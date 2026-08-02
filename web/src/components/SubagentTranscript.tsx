import { useCallback, useEffect, useRef, useState } from 'react'
import type { Event } from '../types/api'
import { authedFetch } from '../store/auth'
import { useTabsStore } from '../store/tabs'
import { useWsStore } from '../store/ws'
import { useUiStore } from '../store/ui'
import { buildDisplayItems } from './chat/events'
import { getCommandLine, getSummary, getToolLabel } from './chat/toolDisplay'

function mergeEvents(existing: Event[], incoming: Event[]): Event[] {
  const bySeq = new Map(existing.map((e) => [e.seq, e]))
  for (const e of incoming) bySeq.set(e.seq, e)
  return [...bySeq.values()].sort((a, b) => a.seq - b.seq)
}

/** Collapsible condensed transcript of a spawn_subagent child session,
 *  attached under the spawn tool card. Fetches once on expand for backfill,
 *  then stays live via the same per-session WS subscription ChatView uses.
 *  "Open session" jumps to the full chat. */
export default function SubagentTranscript({ sessionId }: { sessionId: string }) {
  const [expanded, setExpanded] = useState(false)
  const [events, setEvents] = useState<Event[] | null>(null)
  const [error, setError] = useState(false)
  const connected = useUiStore((s) => s.connected)
  const subscribe = useWsStore((s) => s.subscribe)
  const unsubscribe = useWsStore((s) => s.unsubscribe)
  const addEventListener = useWsStore((s) => s.addEventListener)
  const removeEventListener = useWsStore((s) => s.removeEventListener)

  const load = useCallback(async () => {
    try {
      const res = await authedFetch(`/api/sessions/${sessionId}/events?limit=200`)
      if (!res.ok) throw new Error(String(res.status))
      const fetched = (await res.json()) as Event[]
      setEvents((prev) => (prev ? mergeEvents(prev, fetched) : fetched))
      setError(false)
    } catch {
      setError(true)
    }
  }, [sessionId])

  const finished = events?.some((e) => e.kind === 'agent-end') ?? false

  useEffect(() => {
    if (!expanded || finished) return
    // Backfill goes through a timer so the effect body itself never triggers
    // a synchronous setState cascade (react-hooks/set-state-in-effect).
    const kick = setTimeout(() => void load(), 0)
    subscribe(sessionId)
    const listener = (event: Event) => {
      if (event.session_id !== sessionId) return
      setEvents((prev) => mergeEvents(prev ?? [], [event]))
    }
    addEventListener(listener)
    return () => {
      clearTimeout(kick)
      removeEventListener(listener)
      unsubscribe(sessionId)
    }
  }, [
    expanded,
    finished,
    sessionId,
    load,
    subscribe,
    unsubscribe,
    addEventListener,
    removeEventListener,
  ])

  // Socket dropped mid-transcript and came back: the WS store replays from
  // its own last-seq cache, not this component's, so a manual backfill on
  // the false→true edge is the only way to close a gap opened while
  // disconnected. Guarded so it doesn't also refire on the initial mount,
  // which the subscribe effect above already backfills.
  const wasConnected = useRef(connected)
  useEffect(() => {
    const reconnected = !wasConnected.current && connected
    wasConnected.current = connected
    if (!expanded || finished || !reconnected) return
    const id = setTimeout(() => void load(), 0)
    return () => clearTimeout(id)
  }, [connected, expanded, finished, load])

  const items = events ? buildDisplayItems(events) : []

  return (
    <div className="subagent-block" data-testid="subagent-transcript">
      <div className="subagent-header">
        <button type="button" className="subagent-toggle" onClick={() => setExpanded((v) => !v)}>
          <span className={`tool-chevron ${expanded ? 'open' : ''}`} aria-hidden="true">
            &#9654;
          </span>
          <span className="subagent-title">Subagent transcript</span>
          {expanded && events !== null && !finished && <span className="tool-spinner" />}
        </button>
        <a
          className="subagent-open"
          href={`/sessions/${sessionId}`}
          onClick={(e) => {
            // Plain left click stays in the SPA; modified clicks keep the
            // browser's open-in-new-tab behaviour via the real href.
            if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return
            e.preventDefault()
            const path = `/sessions/${sessionId}`
            if (window.location.pathname !== path) {
              window.history.pushState(null, '', path)
            }
            window.dispatchEvent(new PopStateEvent('popstate'))
            void useTabsStore.getState().openTab('session', sessionId)
          }}
        >
          Open session ↗
        </a>
      </div>
      {expanded && (
        <div className="subagent-body">
          {error && (
            <div className="subagent-row subagent-row-error">Failed to load transcript.</div>
          )}
          {!error && events === null && <div className="subagent-row">Loading…</div>}
          {!connected && !finished && events !== null && (
            <div className="subagent-row subagent-row-status">
              Live updates paused — reconnecting…{' '}
              <button type="button" className="tool-mini-btn" onClick={() => void load()}>
                Refresh
              </button>
            </div>
          )}
          {items.map((it) => {
            switch (it.type) {
              case 'assistant':
                return (
                  <div key={it.key} className="subagent-row subagent-row-text">
                    {it.text.length > 300 ? it.text.slice(0, 297) + '…' : it.text}
                  </div>
                )
              case 'tool': {
                const label = getCommandLine(it.toolName, it.input) || getToolLabel(it.toolName)
                const summary = getSummary(it.toolName, it.input)
                return (
                  <div key={it.key} className="subagent-row subagent-row-tool">
                    {label}
                    {summary ? ` — ${summary}` : ''}
                    {it.error ? ' ⚠' : ''}
                  </div>
                )
              }
              case 'agent-crashed':
                return (
                  <div key={it.key} className="subagent-row subagent-row-error">
                    Crashed: {it.reason}
                  </div>
                )
              case 'status':
                return (
                  <div key={it.key} className="subagent-row subagent-row-status">
                    {it.text}
                  </div>
                )
              default:
                return null
            }
          })}
        </div>
      )}
    </div>
  )
}

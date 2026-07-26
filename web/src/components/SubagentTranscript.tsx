import { useCallback, useEffect, useState } from 'react'
import type { Event } from '../types/api'
import { authedFetch } from '../store/auth'
import { buildDisplayItems } from './chat/events'
import { getCommandLine, getSummary, getToolLabel } from './chat/toolDisplay'

/** Collapsible condensed transcript of a spawn_subagent child session,
 *  attached under the spawn tool card. Fetches lazily on first expand and
 *  refreshes every few seconds while open, since the child is usually still
 *  running when the card appears. "Open session" jumps to the full chat. */
export default function SubagentTranscript({ sessionId }: { sessionId: string }) {
  const [expanded, setExpanded] = useState(false)
  const [events, setEvents] = useState<Event[] | null>(null)
  const [error, setError] = useState(false)

  const load = useCallback(async () => {
    try {
      const res = await authedFetch(`/api/sessions/${sessionId}/events?limit=200`)
      if (!res.ok) throw new Error(String(res.status))
      setEvents((await res.json()) as Event[])
      setError(false)
    } catch {
      setError(true)
    }
  }, [sessionId])

  const finished = events?.some((e) => e.kind === 'agent-end') ?? false

  useEffect(() => {
    if (!expanded || finished) return
    void load()
    const id = setInterval(() => void load(), 5000)
    return () => clearInterval(id)
  }, [expanded, finished, load])

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
        <a className="subagent-open" href={`/sessions/${sessionId}`}>
          Open session ↗
        </a>
      </div>
      {expanded && (
        <div className="subagent-body">
          {error && (
            <div className="subagent-row subagent-row-error">Failed to load transcript.</div>
          )}
          {!error && events === null && <div className="subagent-row">Loading…</div>}
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

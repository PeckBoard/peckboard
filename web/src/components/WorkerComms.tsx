import { useCallback, useEffect, useRef, useState } from 'react'
import { authedFetch } from '../store/auth'
import { useWsStore } from '../store/ws'
import type { Event } from '../types/api'

interface WorkerInfo {
  session_id: string
  name: string
  card_title: string | null
  step: string | null
}

interface CommMessage {
  id: string
  ts: number
  from_session: string
  from_name: string
  to_session: string | null // null = broadcast
  to_name: string | null
  type: 'message' | 'finding' | 'auto-notify' | 'notification'
  text: string
}

interface WorkerCommsProps {
  projectId: string
  onClose: () => void
}

function formatTime(ts: number): string {
  if (!ts) return ''
  return new Date(ts).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit', second: '2-digit' })
}

function msgTypeIcon(type: string): string {
  switch (type) {
    case 'finding': return '\u{1F4A1}'
    case 'auto-notify': return '\u{1F4C1}'
    case 'notification': return '\u{1F514}'
    default: return '\u{1F4AC}'
  }
}

function msgTypeLabel(type: string): string {
  switch (type) {
    case 'finding': return 'Finding'
    case 'auto-notify': return 'File Change'
    case 'notification': return 'Notification'
    default: return 'Message'
  }
}

export default function WorkerComms({ projectId, onClose }: WorkerCommsProps) {
  const [workers, setWorkers] = useState<WorkerInfo[]>([])
  const [messages, setMessages] = useState<CommMessage[]>([])
  const [loading, setLoading] = useState(true)
  const scrollRef = useRef<HTMLDivElement>(null)
  const addEventListener = useWsStore((s) => s.addEventListener)
  const removeEventListener = useWsStore((s) => s.removeEventListener)

  const fetchComms = useCallback(async () => {
    setLoading(true)
    try {
      // Get worker sessions
      const sessRes = await authedFetch(`/api/projects/${projectId}`)
      const projData = sessRes.ok ? await sessRes.json() : null
      const cards = projData?.cards ?? []

      // Build worker map from cards that have worker sessions
      const workerMap: Record<string, WorkerInfo> = {}
      for (const c of cards) {
        for (const sid of [c.worker_session_id, c.last_worker_session_id]) {
          if (sid && !workerMap[sid]) {
            workerMap[sid] = {
              session_id: sid,
              name: `worker: ${c.title}`,
              card_title: c.title,
              step: c.step,
            }
          }
        }
      }
      setWorkers(Object.values(workerMap))

      // Scan worker sessions for communication events
      const comms: CommMessage[] = []
      for (const w of Object.values(workerMap)) {
        try {
          const evRes = await authedFetch(`/api/sessions/${w.session_id}/events`)
          if (!evRes.ok) continue
          const events: Event[] = await evRes.json()
          for (const e of events) {
            if (e.kind !== 'user') continue
            const source = (e.data.source as string) ?? ''
            if (!source.startsWith('worker-')) continue

            const text = (e.data.text as string) ?? ''
            let fromName = 'Unknown'
            let fromSession = ''
            let toSession: string | null = w.session_id
            let toName: string | null = w.card_title ?? w.name

            // Parse sender from the message text
            const fromMatch = text.match(/\[(?:Worker message from|Shared finding from worker on|Auto\] Worker on) "([^"]+)"/)
            if (fromMatch) fromName = fromMatch[1]

            const sessionMatch = text.match(/From session: ([a-f0-9-]+)/)
            if (sessionMatch) fromSession = sessionMatch[1]

            if (source === 'worker-auto-notify') {
              const autoMatch = text.match(/Worker on "([^"]+)" modified/)
              if (autoMatch) fromName = autoMatch[1]
              fromSession = '' // broadcast
            }

            let type: CommMessage['type'] = 'message'
            if (source === 'worker-finding') type = 'finding'
            else if (source === 'worker-auto-notify') type = 'auto-notify'
            else if (source === 'worker-notification') type = 'notification'

            // Truncate text for display
            const displayText = text.length > 300 ? text.slice(0, 297) + '...' : text

            comms.push({
              id: e.id,
              ts: e.ts,
              from_session: fromSession,
              from_name: fromName,
              to_session: toSession,
              to_name: toName,
              type,
              text: displayText,
            })
          }
        } catch { /* skip */ }
      }

      // Sort by time
      comms.sort((a, b) => a.ts - b.ts)
      setMessages(comms)
    } finally {
      setLoading(false)
    }
  }, [projectId])

  useEffect(() => {
    fetchComms()
  }, [fetchComms])

  // Auto-refresh on WebSocket events
  useEffect(() => {
    const listener = (event: Event) => {
      if (event.kind === 'user' && typeof event.data.source === 'string' && event.data.source.startsWith('worker-')) {
        fetchComms()
      }
    }
    addEventListener(listener)
    return () => removeEventListener(listener)
  }, [addEventListener, removeEventListener, fetchComms])

  // Listen for worker-stdin-deliver broadcasts for live updates
  useEffect(() => {
    const handler = () => fetchComms()
    window.addEventListener('peckboard:card-update', handler)
    return () => window.removeEventListener('peckboard:card-update', handler)
  }, [fetchComms])

  // Scroll to bottom on new messages
  useEffect(() => {
    if (scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight
    }
  }, [messages])

  // Build a color map for workers
  const colors = ['#3b82f6', '#10b981', '#f59e0b', '#ef4444', '#8b5cf6', '#ec4899', '#06b6d4', '#84cc16']
  const workerColors: Record<string, string> = {}
  workers.forEach((w, i) => {
    workerColors[w.session_id] = colors[i % colors.length]
    workerColors[w.card_title ?? w.name] = colors[i % colors.length]
  })

  const getColor = (name: string, sessionId: string): string => {
    return workerColors[sessionId] || workerColors[name] || '#6b7280'
  }

  return (
    <div className="worker-comms">
      <div className="worker-comms-header">
        <button className="btn-secondary" onClick={onClose}>&larr; Back to Board</button>
        <h2>Worker Communications</h2>
        <span className="worker-comms-count">{messages.length} messages</span>
      </div>

      {/* Worker legend */}
      <div className="worker-comms-legend">
        {workers.map((w) => (
          <div key={w.session_id} className="worker-comms-legend-item">
            <span className="worker-comms-dot" style={{ background: getColor(w.card_title ?? w.name, w.session_id) }} />
            <span className="worker-comms-legend-name">{w.card_title ?? w.name}</span>
            {w.step && <span className="worker-comms-legend-step">{w.step}</span>}
          </div>
        ))}
      </div>

      {/* Messages timeline */}
      <div className="worker-comms-timeline" ref={scrollRef}>
        {loading && <div className="chat-loading"><div className="loading-spinner" /></div>}
        {!loading && messages.length === 0 && (
          <div className="worker-comms-empty">No inter-worker communications yet.</div>
        )}
        {messages.map((msg) => (
          <div key={msg.id} className="worker-comms-msg">
            <div className="worker-comms-msg-header">
              <span className="worker-comms-msg-icon">{msgTypeIcon(msg.type)}</span>
              <span className="worker-comms-msg-from" style={{ color: getColor(msg.from_name, msg.from_session) }}>
                {msg.from_name}
              </span>
              <span className="worker-comms-msg-arrow">&rarr;</span>
              <span className="worker-comms-msg-to" style={{ color: msg.to_session ? getColor(msg.to_name ?? '', msg.to_session) : undefined }}>
                {msg.to_name ?? 'All Workers'}
              </span>
              <span className="worker-comms-msg-type">{msgTypeLabel(msg.type)}</span>
              <span className="worker-comms-msg-time">{formatTime(msg.ts)}</span>
            </div>
            <div className="worker-comms-msg-body">{msg.text}</div>
          </div>
        ))}
      </div>
    </div>
  )
}

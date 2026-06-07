import { useEffect, useRef } from 'react'
import { useWsStore } from '../store/ws'
import { useWorkerCommsStore } from '../store/workerComms'
import type { Event } from '../types/api'

interface WorkerCommsProps {
  projectId: string
  onClose: () => void
}

function formatTime(ts: number): string {
  if (!ts) return ''
  return new Date(ts).toLocaleTimeString([], {
    hour: 'numeric',
    minute: '2-digit',
    second: '2-digit',
  })
}

function msgTypeIcon(type: string): string {
  switch (type) {
    case 'finding':
      return '\u{1F4A1}'
    case 'auto-notify':
      return '\u{1F4C1}'
    case 'notification':
      return '\u{1F514}'
    default:
      return '\u{1F4AC}'
  }
}

function msgTypeLabel(type: string): string {
  switch (type) {
    case 'finding':
      return 'Finding'
    case 'auto-notify':
      return 'File Change'
    case 'notification':
      return 'Notification'
    default:
      return 'Message'
  }
}

export default function WorkerComms({ projectId, onClose }: WorkerCommsProps) {
  const workers = useWorkerCommsStore((s) => s.workersByProject[projectId] ?? [])
  const messages = useWorkerCommsStore((s) => s.messagesByProject[projectId] ?? [])
  const loading = useWorkerCommsStore((s) => s.loadingByProject[projectId] ?? true)
  const fetchComms = useWorkerCommsStore((s) => s.fetchComms)
  const scrollRef = useRef<HTMLDivElement>(null)
  const addEventListener = useWsStore((s) => s.addEventListener)
  const removeEventListener = useWsStore((s) => s.removeEventListener)

  useEffect(() => {
    fetchComms(projectId)
  }, [fetchComms, projectId])

  // Auto-refresh on WebSocket events
  useEffect(() => {
    const listener = (event: Event) => {
      if (
        event.kind === 'user' &&
        typeof event.data.source === 'string' &&
        event.data.source.startsWith('worker-')
      ) {
        fetchComms(projectId)
      }
    }
    addEventListener(listener)
    return () => removeEventListener(listener)
  }, [addEventListener, removeEventListener, fetchComms, projectId])

  // Listen for worker-stdin-deliver broadcasts for live updates
  useEffect(() => {
    const handler = () => fetchComms(projectId)
    window.addEventListener('peckboard:card-update', handler)
    return () => window.removeEventListener('peckboard:card-update', handler)
  }, [fetchComms, projectId])

  // Scroll to bottom on new messages
  useEffect(() => {
    if (scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight
    }
  }, [messages])

  // Build a color map for workers
  const colors = [
    '#3b82f6',
    '#10b981',
    '#f59e0b',
    '#ef4444',
    '#8b5cf6',
    '#ec4899',
    '#06b6d4',
    '#84cc16',
  ]
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
        <button className="btn-secondary" onClick={onClose}>
          &larr; Back to Board
        </button>
        <h2>Worker Communications</h2>
        <span className="worker-comms-count">{messages.length} messages</span>
      </div>

      {/* Worker legend */}
      <div className="worker-comms-legend">
        {workers.map((w) => (
          <div key={w.session_id} className="worker-comms-legend-item">
            <span
              className="worker-comms-dot"
              style={{ background: getColor(w.card_title ?? w.name, w.session_id) }}
            />
            <span className="worker-comms-legend-name">{w.card_title ?? w.name}</span>
            {w.step && <span className="worker-comms-legend-step">{w.step}</span>}
          </div>
        ))}
      </div>

      {/* Messages timeline */}
      <div className="worker-comms-timeline" ref={scrollRef}>
        {loading && (
          <div className="chat-loading">
            <div className="loading-spinner" />
          </div>
        )}
        {!loading && messages.length === 0 && (
          <div className="worker-comms-empty">No inter-worker communications yet.</div>
        )}
        {messages.map((msg) => (
          <div key={msg.id} className="worker-comms-msg">
            <div className="worker-comms-msg-header">
              <span className="worker-comms-msg-icon">{msgTypeIcon(msg.type)}</span>
              <span
                className="worker-comms-msg-from"
                style={{ color: getColor(msg.from_name, msg.from_session) }}
              >
                {msg.from_name}
              </span>
              <span className="worker-comms-msg-arrow">&rarr;</span>
              <span
                className="worker-comms-msg-to"
                style={{
                  color: msg.to_session ? getColor(msg.to_name ?? '', msg.to_session) : undefined,
                }}
              >
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

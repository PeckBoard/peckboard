import { useEffect, useMemo, useState } from 'react'
import { authedFetch } from '../store/auth'
import { useWsStore } from '../store/ws'
import { useSessionsStore } from '../store/sessions'
import type { Event, Session } from '../types/api'
import { latestTodoSnapshot, parseTodoItems, type TodoItem } from '../types/todo'
import TodoPanel from './TodoPanel'

interface SessionTodosViewProps {
  sessionId: string
  onBack: () => void
}

/**
 * Dedicated, linkable view of a single session's todos. Mirrors the chat
 * surface so /sessions/{id}/todos can be opened directly — fetches the
 * snapshot at load time and keeps it live via the same WS broadcast path
 * the chat view uses. Latest `todo` event wins; the load-time fetch is the
 * fallback for events that landed before this client subscribed.
 */
export default function SessionTodosView({ sessionId, onBack }: SessionTodosViewProps) {
  const events = useSessionsStore((s) => s.eventsBySession[sessionId])
  const fetchEvents = useSessionsStore((s) => s.fetchEvents)
  const appendEvent = useSessionsStore((s) => s.appendEvent)
  const subscribe = useWsStore((s) => s.subscribe)
  const unsubscribe = useWsStore((s) => s.unsubscribe)
  const addEventListener = useWsStore((s) => s.addEventListener)
  const removeEventListener = useWsStore((s) => s.removeEventListener)

  const [sessionDetail, setSessionDetail] = useState<Session | null>(null)
  const [loadedTodos, setLoadedTodos] = useState<TodoItem[]>([])

  useEffect(() => {
    let cancelled = false
    authedFetch(`/api/sessions/${sessionId}`)
      .then((res) => (res.ok ? res.json() : null))
      .then((data: Session | null) => {
        if (!cancelled && data) setSessionDetail(data)
      })
      .catch(() => {})
    return () => {
      cancelled = true
    }
  }, [sessionId])

  useEffect(() => {
    let cancelled = false
    authedFetch(`/api/sessions/${sessionId}/todos`)
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        if (!cancelled) setLoadedTodos(parseTodoItems(data?.todos))
      })
      .catch(() => {})
    return () => {
      cancelled = true
    }
  }, [sessionId])

  // Same `session-cleared` reset as ChatView — the load-time fetch
  // above is keyed on sessionId, so without this a clear from the chat
  // toolbar leaves a stale snapshot showing in this standalone view.
  useEffect(() => {
    const onCleared = (e: CustomEvent<{ sessionId: string }>) => {
      if (e.detail?.sessionId === sessionId) setLoadedTodos([])
    }
    window.addEventListener('peckboard:session-cleared', onCleared as EventListener)
    return () => {
      window.removeEventListener('peckboard:session-cleared', onCleared as EventListener)
    }
  }, [sessionId])

  // Make sure the events store has this session's history populated so live
  // `todo` events can override the load-time snapshot.
  useEffect(() => {
    fetchEvents(sessionId)
  }, [sessionId, fetchEvents])

  useEffect(() => {
    subscribe(sessionId)
    const listener = (event: Event) => {
      if (event.session_id === sessionId) appendEvent(event)
    }
    addEventListener(listener)
    return () => {
      removeEventListener(listener)
      unsubscribe(sessionId)
    }
  }, [sessionId, subscribe, unsubscribe, addEventListener, removeEventListener, appendEvent])

  const eventTodos = useMemo(() => latestTodoSnapshot(events ?? []), [events])
  const todos = eventTodos ?? loadedTodos

  return (
    <div className="chat-container" data-testid="session-todos-view">
      <div className="chat-toolbar">
        <button
          className="chat-toolbar-back"
          onClick={onBack}
          type="button"
          aria-label="Back to chat"
          data-testid="session-todos-back"
        >
          <svg
            width="16"
            height="16"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <polyline points="15 18 9 12 15 6" />
          </svg>
        </button>
        <span className="chat-toolbar-name">{sessionDetail?.name ?? 'Session'} — Tasks</span>
      </div>

      <div className="session-todos-body">
        {todos.length === 0 ? (
          <div className="session-todos-empty" data-testid="session-todos-empty">
            <p>No tasks yet</p>
            <span>
              This session hasn't reported any todos. Tasks will appear here once the agent uses
              TodoWrite.
            </span>
          </div>
        ) : (
          <div className="session-todos-wrapper">
            <TodoPanel todos={todos} />
          </div>
        )}
      </div>
    </div>
  )
}

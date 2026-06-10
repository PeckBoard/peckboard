import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import rehypeHighlight from 'rehype-highlight'
import SafeMarkdown from './SafeMarkdown'
import type { Event, Session } from '../types/api'
import { authedFetch } from '../store/auth'
import { useWsStore } from '../store/ws'
import { useSessionsStore, type PendingUserMessage } from '../store/sessions'
import InputBar from './InputBar'
import ToolUseBlock from './ToolUseBlock'
import ConfirmDialog from './ConfirmDialog'
import TodoPanel from './TodoPanel'
import { parseTodoItems, latestTodoSnapshot, type TodoItem } from '../types/todo'
import {
  EMPTY_EVENTS,
  buildDisplayItems,
  deriveAgentStatus,
  formatTime,
  getStatusDotClass,
  getStatusLabel,
  type QuestionItem,
} from './chat/events'
import 'highlight.js/styles/github-dark.css'

// Stable empty array so the memoized `todos` keeps referential equality
// when there are no todos (avoids re-renders of TodoPanel and a
// fresh-array warning from React fast refresh).
const EMPTY_TODOS: TodoItem[] = []
const EMPTY_PENDING_MESSAGES: PendingUserMessage[] = []

interface ChatViewProps {
  sessionId: string
  onOpenTodos?: () => void
}

function ResolvedQuestionCard({
  questions,
  answers,
}: {
  questions: QuestionItem[]
  answers: Record<string, unknown>
}) {
  return (
    <div className="question-card question-resolved">
      <div className="question-card-title-bar">
        <span className="question-card-icon">&#x2611;&#xFE0F;</span>
        <span className="question-card-title-text">Question answered</span>
      </div>
      {questions.map((q, idx) => {
        const answer = String(
          answers[idx] ?? answers[String(idx)] ?? answers[q.question] ?? '(no answer)',
        )
        return (
          <div key={idx} className="question-item">
            {q.header && <div className="question-header">{q.header}</div>}
            <div className="question-card-text">{q.question}</div>
            <div className="question-answer-display">{answer}</div>
          </div>
        )
      })}
    </div>
  )
}

function QuestionCard({
  sessionId,
  questionId,
  questions,
}: {
  sessionId: string
  questionId: string
  questions: QuestionItem[]
}) {
  const [answers, setAnswers] = useState<Record<number, string>>({})
  const [submitting, setSubmitting] = useState(false)

  const setAnswer = (idx: number, value: string) => {
    setAnswers((prev) => ({ ...prev, [idx]: value }))
  }

  const toggleMulti = (idx: number, option: string) => {
    setAnswers((prev) => {
      const current = prev[idx] ?? ''
      const selected = current ? current.split(',') : []
      const next = selected.includes(option)
        ? selected.filter((s) => s !== option)
        : [...selected, option]
      return { ...prev, [idx]: next.join(',') }
    })
  }

  const hasAnswers = questions.some((_, idx) => (answers[idx] ?? '').trim().length > 0)

  const handleSubmit = async () => {
    if (!hasAnswers || submitting) return
    setSubmitting(true)
    try {
      const answerMap: Record<string, string> = {}
      questions.forEach((_, idx) => {
        const val = (answers[idx] ?? '').trim()
        if (val) answerMap[String(idx)] = val
      })
      await authedFetch(`/api/sessions/${sessionId}/events`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          kind: 'question-resolved',
          data: { question_id: questionId, answers: answerMap },
        }),
      })
    } finally {
      setSubmitting(false)
    }
  }

  const handleDismiss = async () => {
    if (submitting) return
    setSubmitting(true)
    try {
      await authedFetch(`/api/sessions/${sessionId}/events`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          kind: 'question-resolved',
          data: { question_id: questionId, rejected: true },
        }),
      })
    } finally {
      setSubmitting(false)
    }
  }

  return (
    <div className="question-card question-active">
      <div className="question-card-title-bar">
        <span className="question-card-icon">&#x2753;</span>
        <span className="question-card-title-text">Input needed</span>
      </div>
      {questions.map((q, idx) => (
        <div key={idx} className="question-item">
          {q.header && <div className="question-header">{q.header}</div>}
          <div className="question-card-text">{q.question}</div>
          {q.options && q.options.length > 0 ? (
            <div className="question-options">
              {q.options.map((opt, optIdx) => {
                const optObj = q.optionObjects?.[optIdx]
                return (
                  <label key={opt} className="question-option-label">
                    {q.multiSelect ? (
                      <input
                        type="checkbox"
                        checked={(answers[idx] ?? '').split(',').includes(opt)}
                        onChange={() => toggleMulti(idx, opt)}
                        disabled={submitting}
                      />
                    ) : (
                      <input
                        type="radio"
                        name={`question-${questionId}-${idx}`}
                        checked={answers[idx] === opt}
                        onChange={() => setAnswer(idx, opt)}
                        disabled={submitting}
                      />
                    )}
                    <span className="question-option-text">
                      <span className="question-option-label-text">{opt}</span>
                      {optObj?.description && (
                        <span className="question-option-desc">{optObj.description}</span>
                      )}
                    </span>
                  </label>
                )
              })}
            </div>
          ) : (
            <input
              className="question-input"
              type="text"
              placeholder="Type your answer..."
              value={answers[idx] ?? ''}
              onChange={(e) => setAnswer(idx, e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' && questions.length === 1) handleSubmit()
              }}
              disabled={submitting}
            />
          )}
        </div>
      ))}
      <div className="question-actions">
        <button className="btn-primary" onClick={handleSubmit} disabled={!hasAnswers || submitting}>
          Submit
        </button>
        <button className="btn-secondary" onClick={handleDismiss} disabled={submitting}>
          Dismiss
        </button>
      </div>
    </div>
  )
}

interface ModelInfo {
  id: string
  display_name: string
}

export default function ChatView({ sessionId, onOpenTodos }: ChatViewProps) {
  const events = useSessionsStore((s) => s.eventsBySession[sessionId] ?? EMPTY_EVENTS)
  const loading = useSessionsStore((s) => s.loadingEventsBySession[sessionId] ?? true)
  const fetchEvents = useSessionsStore((s) => s.fetchEvents)
  const fetchOlderEvents = useSessionsStore((s) => s.fetchOlderEvents)
  const loadingOlderEvents = useSessionsStore(
    (s) => s.loadingOlderEventsBySession[sessionId] ?? false,
  )
  const hasMoreOlderEvents = useSessionsStore(
    (s) => s.hasMoreOlderEventsBySession[sessionId] ?? false,
  )
  const appendEvent = useSessionsStore((s) => s.appendEvent)
  const pendingUserMessages = useSessionsStore(
    (s) => s.pendingUserMessages[sessionId] ?? EMPTY_PENDING_MESSAGES,
  )
  const prunePendingUserMessages = useSessionsStore((s) => s.prunePendingUserMessages)
  const [sessionDetail, setSessionDetail] = useState<Session | null>(null)
  const [menuOpen, setMenuOpen] = useState(false)
  const [confirmAction, setConfirmAction] = useState<{
    title: string
    message: string
    onConfirm: () => void
  } | null>(null)
  const [modelDropdownOpen, setModelDropdownOpen] = useState(false)
  const [availableModels, setAvailableModels] = useState<ModelInfo[]>([])
  const [loadedTodos, setLoadedTodos] = useState<TodoItem[]>([])
  const scrollRef = useRef<HTMLDivElement>(null)
  const menuRef = useRef<HTMLDivElement>(null)
  const modelRef = useRef<HTMLDivElement>(null)
  const userScrolledUp = useRef(false)
  /** Saved scroll-height immediately before a "Load older" fetch so
   *  we can restore the user's viewport position after the new rows
   *  splice in at the top. Without this the entire conversation
   *  shifts down by the height of the loaded page and the user loses
   *  their reading position. `null` whenever no restore is pending. */
  const pendingOlderScrollRestore = useRef<number | null>(null)

  const subscribe = useWsStore((s) => s.subscribe)
  const unsubscribe = useWsStore((s) => s.unsubscribe)
  const addEventListener = useWsStore((s) => s.addEventListener)
  const removeEventListener = useWsStore((s) => s.removeEventListener)
  const renameSession = useSessionsStore((s) => s.renameSession)
  const clearSession = useSessionsStore((s) => s.clearSession)
  const deleteSession = useSessionsStore((s) => s.deleteSession)
  const interruptSession = useSessionsStore((s) => s.interruptSession)
  const terminateAgent = useSessionsStore((s) => s.terminateAgent)

  // Fetch session detail on mount
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

  // Fetch the current todo snapshot on load so a freshly opened session shows
  // existing todos before any live `todo` event arrives over the WS.
  useEffect(() => {
    let cancelled = false
    authedFetch(`/api/sessions/${sessionId}/todos`)
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        // Always set (the endpoint returns `{ todos: [] }` for a fresh
        // session), so switching sessions clears any prior snapshot.
        if (!cancelled) setLoadedTodos(parseTodoItems(data?.todos))
      })
      .catch(() => {})
    return () => {
      cancelled = true
    }
  }, [sessionId])

  // Listen for the server's `session-cleared` broadcast and drop the
  // cached snapshot. Without this the panel keeps rendering pre-clear
  // todos until the user navigates away — the load-time fetch above
  // only runs on sessionId change, not on a same-session wipe.
  useEffect(() => {
    const onCleared = (e: CustomEvent<{ sessionId: string }>) => {
      if (e.detail?.sessionId === sessionId) setLoadedTodos([])
    }
    window.addEventListener('peckboard:session-cleared', onCleared as EventListener)
    return () => {
      window.removeEventListener('peckboard:session-cleared', onCleared as EventListener)
    }
  }, [sessionId])

  // Close menu on outside click
  useEffect(() => {
    if (!menuOpen) return
    const handleClick = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        setMenuOpen(false)
      }
    }
    document.addEventListener('mousedown', handleClick)
    return () => document.removeEventListener('mousedown', handleClick)
  }, [menuOpen])

  // Close model dropdown on outside click
  useEffect(() => {
    if (!modelDropdownOpen) return
    const handleClick = (e: MouseEvent) => {
      if (modelRef.current && !modelRef.current.contains(e.target as Node)) {
        setModelDropdownOpen(false)
      }
    }
    document.addEventListener('mousedown', handleClick)
    return () => document.removeEventListener('mousedown', handleClick)
  }, [modelDropdownOpen])

  // Fetch available models when model dropdown opens
  useEffect(() => {
    if (!modelDropdownOpen || availableModels.length > 0) return
    authedFetch('/api/models')
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        if (data && Array.isArray(data.models)) {
          setAvailableModels(data.models as ModelInfo[])
        }
      })
      .catch(() => {})
  }, [modelDropdownOpen, availableModels.length])

  // Fetch initial events
  useEffect(() => {
    userScrolledUp.current = false
    fetchEvents(sessionId)
  }, [sessionId, fetchEvents])

  // Subscribe to WS events for this session
  useEffect(() => {
    subscribe(sessionId)

    const listener = (event: Event) => {
      if (event.session_id === sessionId) {
        appendEvent(event)
      }
    }

    addEventListener(listener)

    return () => {
      removeEventListener(listener)
      unsubscribe(sessionId)
    }
  }, [sessionId, subscribe, unsubscribe, addEventListener, removeEventListener, appendEvent])

  // Sweep orphaned optimistic bubbles every 10s. Normally the matching
  // WS `user` event arrives within a few hundred ms and clears the
  // pending entry; if the POST succeeded but the broadcast was lost
  // (server crash mid-flight, etc.) the bubble would otherwise stick
  // around with no way to clear it. 60s is generous — anything older
  // than that is almost certainly orphaned.
  useEffect(() => {
    const tick = window.setInterval(() => {
      prunePendingUserMessages(60_000)
    }, 10_000)
    return () => window.clearInterval(tick)
  }, [prunePendingUserMessages])

  // Scroll handling
  const handleScroll = useCallback(() => {
    const el = scrollRef.current
    if (!el) return
    const threshold = 60
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < threshold
    userScrolledUp.current = !atBottom
  }, [])

  useEffect(() => {
    const el = scrollRef.current
    if (!el) return
    // If a "Load older" fetch is in flight, restore the user's
    // scroll-from-bottom so the older rows splice in above without
    // shifting their viewport. Stomp the saved value so we don't
    // re-apply on the next render.
    //
    // BUT only if the user is still scrolled up. If they scrolled
    // all the way to the bottom while the fetch was in flight (the
    // agent just emitted something, or they hit End), respect that
    // — fall through to the auto-scroll branch and snap to the new
    // bottom. Restoring an older saved position over an active
    // scroll-to-bottom would yank them away from text they just
    // chose to read.
    if (pendingOlderScrollRestore.current !== null) {
      const savedHeight = pendingOlderScrollRestore.current
      pendingOlderScrollRestore.current = null
      if (userScrolledUp.current) {
        el.scrollTop = el.scrollHeight - savedHeight
        return
      }
      // Falls through to auto-scroll-to-bottom.
    }
    if (!userScrolledUp.current) {
      el.scrollTop = el.scrollHeight
    }
  }, [events])

  const handleLoadOlder = useCallback(() => {
    const el = scrollRef.current
    if (el) {
      // Capture the current "distance from top of content" so the
      // useEffect above can restore it after the new rows render.
      pendingOlderScrollRestore.current = el.scrollHeight - el.scrollTop
    }
    void fetchOlderEvents(sessionId)
  }, [fetchOlderEvents, sessionId])

  const displayItems = buildDisplayItems(events)

  // Live `todo` events are authoritative once any arrive; before then, fall
  // back to the snapshot fetched at load time. After a clear (events loaded
  // but empty) the snapshot must also go away — without the explicit empty
  // check the panel would keep rendering `loadedTodos` from the pre-clear
  // mount and never disappear.
  const todos = useMemo(() => {
    const snap = latestTodoSnapshot(events)
    if (snap) return snap
    if (!loading && events.length === 0) return EMPTY_TODOS
    return loadedTodos
  }, [events, loadedTodos, loading])

  // Determine if agent is working (includes waiting for CLI to start after user sends)
  const agentWorking = (() => {
    for (let i = events.length - 1; i >= 0; i--) {
      const kind = events[i].kind
      if (kind === 'agent-start') return true
      if (kind === 'agent-end') return false
      // User sent a message but CLI hasn't started yet — still "working"
      if (kind === 'user') return true
    }
    return false
  })()

  const agentStatus = deriveAgentStatus(events)

  // Always show the thinking indicator while the agent is working —
  // even when text or tool blocks are streaming above it. The indicator
  // is the user's only persistent signal that the session is still busy.
  const showThinking = agentWorking

  // Toolbar actions
  const handleRename = async () => {
    setMenuOpen(false)
    const currentName = sessionDetail?.name ?? ''
    const newName = window.prompt('Rename session:', currentName)
    if (newName && newName !== currentName) {
      await renameSession(sessionId, newName)
      setSessionDetail((prev) => (prev ? { ...prev, name: newName } : prev))
    }
  }

  const handleClear = () => {
    setMenuOpen(false)
    setConfirmAction({
      title: 'Clear session',
      message: 'Clear all messages in this session?',
      onConfirm: async () => {
        setConfirmAction(null)
        await clearSession(sessionId)
        fetchEvents(sessionId)
      },
    })
  }

  const handleTerminateAgent = () => {
    setMenuOpen(false)
    setConfirmAction({
      title: 'Terminate agent',
      message:
        'Terminate the agent process? Any in-flight turn will be interrupted. The next message will start a fresh process (picking up any new skills or config).',
      onConfirm: async () => {
        setConfirmAction(null)
        await terminateAgent(sessionId)
      },
    })
  }

  const handleDelete = () => {
    setMenuOpen(false)
    setConfirmAction({
      title: 'Delete session',
      message: 'Delete this session and all its events?',
      onConfirm: async () => {
        setConfirmAction(null)
        await deleteSession(sessionId)
      },
    })
  }

  const handleModelChange = async (modelId: string) => {
    setModelDropdownOpen(false)
    try {
      const res = await authedFetch(`/api/sessions/${sessionId}`, {
        method: 'PATCH',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ model: modelId }),
      })
      if (res.ok) {
        const updated: Session = await res.json()
        setSessionDetail(updated)
      }
    } catch {
      /* ignore */
    }
  }

  if (loading) {
    return (
      <div className="chat-container">
        <div className="chat-loading">Loading events...</div>
      </div>
    )
  }

  return (
    <div className="chat-container">
      {/* Toolbar */}
      <div className="chat-toolbar">
        <span className="chat-toolbar-name">{sessionDetail?.name ?? 'Session'}</span>
        <div className="chat-toolbar-model-wrapper" ref={modelRef}>
          <button
            className="chat-toolbar-model"
            onClick={() => setModelDropdownOpen(!modelDropdownOpen)}
            type="button"
          >
            {sessionDetail?.model ?? 'default'}
          </button>
          {modelDropdownOpen && (
            <div className="chat-toolbar-dropdown chat-model-dropdown">
              {availableModels.length === 0 && (
                <div className="chat-model-loading">Loading models...</div>
              )}
              {availableModels.map((m) => (
                <button
                  key={m.id}
                  className={m.id === sessionDetail?.model ? 'active' : ''}
                  onClick={() => handleModelChange(m.id)}
                >
                  {m.display_name}
                </button>
              ))}
            </div>
          )}
        </div>
        <span className="chat-toolbar-status">
          <span className={getStatusDotClass(agentStatus)} />
          {getStatusLabel(agentStatus)}
        </span>
        {onOpenTodos && (
          <button
            className="chat-toolbar-tasks"
            onClick={onOpenTodos}
            type="button"
            title="Tasks"
            data-testid="chat-toolbar-tasks"
          >
            <svg
              width="14"
              height="14"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
              aria-hidden="true"
            >
              <polyline points="9 11 12 14 22 4" />
              <path d="M21 12v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11" />
            </svg>
            <span>Tasks</span>
            {todos.length > 0 && (
              <span className="chat-toolbar-tasks-count">
                {todos.filter((t) => t.status === 'done').length}/{todos.length}
              </span>
            )}
          </button>
        )}
        <div className="chat-toolbar-menu-wrapper" ref={menuRef}>
          <button
            className="chat-toolbar-menu"
            onClick={() => setMenuOpen(!menuOpen)}
            type="button"
          >
            <svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor">
              <circle cx="8" cy="3" r="1.5" />
              <circle cx="8" cy="8" r="1.5" />
              <circle cx="8" cy="13" r="1.5" />
            </svg>
          </button>
          {menuOpen && (
            <div className="chat-toolbar-dropdown">
              <button onClick={handleRename}>Rename</button>
              <button onClick={handleClear}>Clear</button>
              <button onClick={handleTerminateAgent} data-testid="chat-toolbar-terminate">
                Terminate agent
              </button>
              {/* Worker sessions are owned by their card; the backend
                  refuses DELETE /api/sessions/:id for them. Hide the
                  button rather than render a control that always 409s. */}
              {!sessionDetail?.is_worker && (
                <button className="danger" onClick={handleDelete}>
                  Delete
                </button>
              )}
            </div>
          )}
        </div>
      </div>

      <TodoPanel todos={todos} />

      <div className="chat-messages" ref={scrollRef} onScroll={handleScroll}>
        {/* "Load older" button: shown at the top once the initial
            fetch returned a full page (more history likely exists)
            and hidden once a short page proves the user has reached
            the start of the conversation. The store debounces with
            `loadingOlderEvents` so a rapid double-click loads at most
            one extra page. */}
        {hasMoreOlderEvents && displayItems.length > 0 && (
          <div className="chat-load-older">
            <button
              className="chat-load-older-btn"
              data-testid="chat-load-older"
              onClick={handleLoadOlder}
              disabled={loadingOlderEvents}
            >
              {loadingOlderEvents ? 'Loading…' : 'Load older messages'}
            </button>
          </div>
        )}
        {displayItems.length === 0 && (
          <div className="chat-empty">No messages yet. Send one below.</div>
        )}
        {displayItems.map((item) => {
          switch (item.type) {
            case 'user':
              return (
                <div key={item.key} className="chat-row chat-row-user">
                  <div className="chat-bubble chat-bubble-user">
                    {item.text}
                    <div className="chat-time chat-time-user">{formatTime(item.ts)}</div>
                  </div>
                </div>
              )
            case 'assistant':
              return (
                <div key={item.key} className="chat-row chat-row-assistant">
                  <div className="chat-bubble chat-bubble-assistant">
                    <SafeMarkdown className="chat-markdown" rehypePlugins={[rehypeHighlight]}>
                      {item.text}
                    </SafeMarkdown>
                    <div className="chat-time">{formatTime(item.ts)}</div>
                  </div>
                </div>
              )
            case 'agent-start':
              return (
                <div key={item.key} className="chat-row chat-row-system">
                  <div className="chat-agent-start">
                    <span className="chat-agent-start-label">Agent started</span>
                    <span className="chat-agent-start-detail">
                      {item.model}
                      {item.effort ? `, ${item.effort}` : ''}
                    </span>
                    <span className="chat-agent-start-time">{formatTime(item.ts)}</span>
                  </div>
                </div>
              )
            case 'interrupt':
              return (
                <div key={item.key} className="chat-row chat-row-system">
                  <div className="chat-agent-start">
                    <span className="chat-agent-start-label">Agent interrupted</span>
                    <span className="chat-agent-start-time">{formatTime(item.ts)}</span>
                  </div>
                </div>
              )
            case 'agent-crashed':
              // Plain row, no bubble/icon — mirrors `agent-start` and
              // `interrupt` so all agent lifecycle notices read the
              // same. The reason ("process exited with code 1",
              // "interrupted", etc.) sits in the detail chip; stderr
              // is intentionally not surfaced here, it's still in the
              // event payload for debugging via the API.
              return (
                <div key={item.key} className="chat-row chat-row-system">
                  <div className="chat-agent-start">
                    <span className="chat-agent-start-label">Agent crashed</span>
                    <span className="chat-agent-start-detail">{item.reason}</span>
                    <span className="chat-agent-start-time">{formatTime(item.ts)}</span>
                  </div>
                </div>
              )
            case 'tool':
              return (
                <div key={item.key} className="chat-row chat-row-tool">
                  <ToolUseBlock
                    toolName={item.toolName}
                    input={item.input}
                    output={item.output}
                    error={item.error}
                    isRunning={item.isRunning}
                  />
                </div>
              )
            case 'status':
              return (
                <div key={item.key} className="chat-row chat-row-system">
                  <div className="chat-ready-notice">
                    <span>{item.text}</span>
                    <span className="chat-ready-time">{formatTime(item.ts)}</span>
                  </div>
                </div>
              )
            case 'system':
              return (
                <div key={item.key} className="chat-row chat-row-system">
                  {item.reportFolder && item.reportFile ? (
                    <button
                      className="chat-report-chip"
                      onClick={() => {
                        window.history.pushState({}, '', '/reports')
                        window.dispatchEvent(new PopStateEvent('popstate'))
                      }}
                    >
                      <span className="chat-report-chip-icon">{'\u{1F4C4}'}</span>
                      <span className="chat-report-chip-body">
                        <span className="chat-report-chip-title">{item.text}</span>
                        <span className="chat-report-chip-folder">{item.reportFolder}</span>
                      </span>
                    </button>
                  ) : (
                    <div className="chat-system-notice">
                      <span className="chat-system-notice-icon">{'\u2139\uFE0F'}</span>
                      <span>{item.text}</span>
                    </div>
                  )}
                </div>
              )
            case 'step':
              return (
                <div key={item.key} className="chat-row chat-row-step">
                  <div className="chat-step-divider">
                    <span>{item.label}</span>
                  </div>
                </div>
              )
            case 'question':
              return (
                <div key={item.key} className="chat-row chat-row-system">
                  <QuestionCard
                    sessionId={sessionId}
                    questionId={item.questionId}
                    questions={item.questions}
                  />
                </div>
              )
            case 'question-resolved':
              return (
                <div key={item.key} className="chat-row chat-row-system">
                  <ResolvedQuestionCard questions={item.questions} answers={item.answers} />
                </div>
              )
          }
        })}
        {/* Optimistic user bubbles — rendered immediately on Send so the
            chat doesn't appear to swallow the message during the WS
            round-trip (especially noticeable for queued turns). The
            matching real `user` event clears the pending entry on
            arrival; see `clearMatchingPending` in store/sessions.ts. */}
        {pendingUserMessages.map((p) => (
          <div key={p.tempId} className="chat-row chat-row-user">
            <div className="chat-bubble chat-bubble-user chat-bubble-pending">
              {p.text}
              <div className="chat-time chat-time-user">Sending...</div>
            </div>
          </div>
        ))}
        {/* Thinking indicator + inline Interrupt — shown at the end of the
            message log when the agent is working. Combining them keeps the
            "stop the agent" affordance attached to the activity it's
            stopping, instead of a floating toolbar pinned above the input. */}
        {showThinking && (
          <div className="chat-row chat-row-system">
            <div className="chat-thinking">
              <div className="chat-thinking-dots">
                <span />
                <span />
                <span />
              </div>
              <span>Thinking...</span>
              <button
                className="chat-thinking-interrupt"
                onClick={() => interruptSession(sessionId)}
                type="button"
                aria-label="Interrupt agent"
                title="Interrupt the agent"
              >
                Interrupt
              </button>
            </div>
          </div>
        )}
      </div>

      {/* `key` forces a fresh InputBar per session — drafts and any
          pending attachments belong to the session that started them
          and shouldn't bleed across switches. */}
      <InputBar key={sessionId} sessionId={sessionId} agentWorking={agentWorking} />
      {confirmAction && (
        <ConfirmDialog
          title={confirmAction.title}
          message={confirmAction.message}
          confirmLabel="Confirm"
          cancelLabel="Cancel"
          danger
          onConfirm={confirmAction.onConfirm}
          onCancel={() => setConfirmAction(null)}
        />
      )}
    </div>
  )
}

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import rehypeHighlight from 'rehype-highlight'
import type { Event, Session } from '../types/api'
import { authedFetch } from '../store/auth'
import { useWsStore } from '../store/ws'
import { useSessionsStore } from '../store/sessions'
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

interface ChatViewProps {
  sessionId: string
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

export default function ChatView({ sessionId }: ChatViewProps) {
  const events = useSessionsStore((s) => s.eventsBySession[sessionId] ?? EMPTY_EVENTS)
  const loading = useSessionsStore((s) => s.loadingEventsBySession[sessionId] ?? true)
  const fetchEvents = useSessionsStore((s) => s.fetchEvents)
  const appendEvent = useSessionsStore((s) => s.appendEvent)
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

  const subscribe = useWsStore((s) => s.subscribe)
  const unsubscribe = useWsStore((s) => s.unsubscribe)
  const addEventListener = useWsStore((s) => s.addEventListener)
  const removeEventListener = useWsStore((s) => s.removeEventListener)
  const renameSession = useSessionsStore((s) => s.renameSession)
  const clearSession = useSessionsStore((s) => s.clearSession)
  const deleteSession = useSessionsStore((s) => s.deleteSession)
  const interruptSession = useSessionsStore((s) => s.interruptSession)

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

  // Scroll handling
  const handleScroll = useCallback(() => {
    const el = scrollRef.current
    if (!el) return
    const threshold = 60
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < threshold
    userScrolledUp.current = !atBottom
  }, [])

  useEffect(() => {
    if (!userScrolledUp.current) {
      const el = scrollRef.current
      if (el) {
        el.scrollTop = el.scrollHeight
      }
    }
  }, [events])

  const displayItems = buildDisplayItems(events)

  // Live `todo` events are authoritative once any arrive; before then, fall
  // back to the snapshot fetched at load time.
  const eventTodos = useMemo(() => latestTodoSnapshot(events), [events])
  const todos = eventTodos ?? loadedTodos

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
              <button className="danger" onClick={handleDelete}>
                Delete
              </button>
            </div>
          )}
        </div>
      </div>

      <TodoPanel todos={todos} />

      <div className="chat-messages" ref={scrollRef} onScroll={handleScroll}>
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
                    <div className="chat-markdown">
                      <ReactMarkdown
                        remarkPlugins={[remarkGfm]}
                        rehypePlugins={[rehypeHighlight]}
                        components={{
                          a: ({ href, children }) => (
                            <a href={href} target="_blank" rel="noreferrer noopener">
                              {children}
                            </a>
                          ),
                        }}
                      >
                        {item.text}
                      </ReactMarkdown>
                    </div>
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
        {/* Thinking indicator — shown when waiting for agent response */}
        {showThinking && (
          <div className="chat-row chat-row-system">
            <div className="chat-thinking">
              <div className="chat-thinking-dots">
                <span />
                <span />
                <span />
              </div>
              <span>Thinking...</span>
            </div>
          </div>
        )}
      </div>

      {/* Interrupt button — centered above input bar when agent is working */}
      {agentWorking && (
        <div className="chat-interrupt-bar">
          <button className="chat-interrupt-btn" onClick={() => interruptSession(sessionId)}>
            Interrupt
          </button>
        </div>
      )}

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

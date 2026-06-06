import { useCallback, useEffect, useRef, useState } from 'react'
import type { Event, Session } from '../types/api'
import { authedFetch } from '../store/auth'
import { useWsStore } from '../store/ws'
import { useSessionsStore } from '../store/sessions'
import InputBar from './InputBar'
import ToolUseBlock from './ToolUseBlock'
import ConfirmDialog from './ConfirmDialog'

interface ChatViewProps {
  sessionId: string
}

/** Structured question within a question event */
interface QuestionItem {
  question: string
  header?: string
  multiSelect?: boolean
  options?: string[]
}

/** A display item derived from one or more raw events. */
type DisplayItem =
  | { type: 'user'; text: string; key: string }
  | { type: 'assistant'; text: string; key: string }
  | { type: 'tool'; toolName: string; input?: Record<string, unknown>; output?: Record<string, unknown>; error?: string; isRunning: boolean; key: string }
  | { type: 'status'; text: string; key: string }
  | { type: 'system'; text: string; key: string }
  | { type: 'step'; label: string; key: string }
  | { type: 'question'; questionId: string; questions: QuestionItem[]; key: string }
  | { type: 'question-resolved'; questionId: string; questions: QuestionItem[]; answers: Record<string, unknown>; key: string }

/** Derive agent status from events for the toolbar indicator. */
type AgentStatus = 'idle' | 'working' | 'tool' | 'crashed' | 'questioning'

function deriveAgentStatus(events: Event[]): AgentStatus {
  for (let i = events.length - 1; i >= 0; i--) {
    const kind = events[i].kind
    if (kind === 'agent-end') return 'idle'
    if (kind === 'agent-error' || kind === 'error') return 'crashed'
    if (kind === 'question') {
      // Check if resolved later
      const qId = events[i].id
      const resolved = events.slice(i + 1).some(
        (e) => e.kind === 'question-resolved' && (e.data.question_id === qId || e.data.questionId === qId)
      )
      if (!resolved) return 'questioning'
    }
    if (kind === 'agent-tool-start') {
      // Check if ended later
      const toolUseId = (events[i].data.tool_use_id as string) ?? events[i].id
      const ended = events.slice(i + 1).some(
        (e) => e.kind === 'agent-tool-end' && (e.data.tool_use_id as string) === toolUseId
      )
      if (!ended) return 'tool'
    }
    if (kind === 'agent-start') return 'working'
  }
  return 'idle'
}

function getStatusLabel(status: AgentStatus): string {
  switch (status) {
    case 'idle': return 'Idle'
    case 'working': return 'Working...'
    case 'tool': return 'Using tool...'
    case 'crashed': return 'Crashed'
    case 'questioning': return 'Awaiting answer'
  }
}

function getStatusDotClass(status: AgentStatus): string {
  switch (status) {
    case 'idle': return 'status-dot status-dot-idle'
    case 'working': return 'status-dot status-dot-working'
    case 'tool': return 'status-dot status-dot-tool'
    case 'crashed': return 'status-dot status-dot-crashed'
    case 'questioning': return 'status-dot status-dot-questioning'
  }
}

function buildDisplayItems(events: Event[]): DisplayItem[] {
  const items: DisplayItem[] = []
  let assistantBuffer = ''
  let assistantKey = ''

  const flushAssistant = () => {
    if (assistantBuffer) {
      items.push({ type: 'assistant', text: assistantBuffer, key: assistantKey })
      assistantBuffer = ''
      assistantKey = ''
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

  for (const ev of events) {
    switch (ev.kind) {
      case 'user': {
        flushAssistant()
        const text = (ev.data.text as string) ?? JSON.stringify(ev.data)
        items.push({ type: 'user', text, key: ev.id })
        break
      }
      case 'agent-text': {
        const chunk = (ev.data.text as string) ?? ''
        if (!assistantKey) assistantKey = ev.id
        assistantBuffer += chunk
        break
      }
      case 'agent-tool-start': {
        flushAssistant()
        const toolName = (ev.data.tool_name as string) ?? 'tool'
        const input = (ev.data.input as Record<string, unknown>) ?? undefined
        const toolUseId = (ev.data.tool_use_id as string) ?? ev.id
        const idx = items.length
        items.push({ type: 'tool', toolName, input, isRunning: true, key: ev.id })
        openTools.set(toolUseId, idx)
        break
      }
      case 'agent-tool-end': {
        flushAssistant()
        const toolUseId = (ev.data.tool_use_id as string) ?? ''
        const idx = openTools.get(toolUseId)
        if (idx !== undefined) {
          const existing = items[idx] as Extract<DisplayItem, { type: 'tool' }>
          const errorText = ev.data.error as string | undefined
          const output = (ev.data.output as Record<string, unknown>) ?? undefined
          items[idx] = { ...existing, isRunning: false, output, error: errorText }
          openTools.delete(toolUseId)
        } else {
          const toolName = (ev.data.tool_name as string) ?? 'tool'
          const errorText = ev.data.error as string | undefined
          const output = (ev.data.output as Record<string, unknown>) ?? undefined
          items.push({ type: 'tool', toolName, output, error: errorText, isRunning: false, key: ev.id })
        }
        break
      }
      case 'agent-start': {
        flushAssistant()
        items.push({ type: 'status', text: 'Agent started', key: ev.id })
        break
      }
      case 'agent-end': {
        flushAssistant()
        items.push({ type: 'status', text: 'Agent finished', key: ev.id })
        break
      }
      case 'system': {
        flushAssistant()
        const text = (ev.data.text as string) ?? (ev.data.message as string) ?? JSON.stringify(ev.data)
        items.push({ type: 'system', text, key: ev.id })
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
          }))
        } else {
          const text = (ev.data.text as string) ?? (ev.data.question as string) ?? JSON.stringify(ev.data)
          questions = [{ question: text }]
        }

        if (resolvedQuestions.has(ev.id)) {
          // Find the matching resolved event to get answers
          const resolvedEv = events.find(
            (e) => e.kind === 'question-resolved' && ((e.data.question_id as string) === ev.id || (e.data.questionId as string) === ev.id)
          )
          const answers = (resolvedEv?.data.answers as Record<string, unknown>) ?? {}
          items.push({ type: 'question-resolved', questionId: ev.id, questions, answers, key: ev.id })
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

function ResolvedQuestionCard({ questions, answers }: { questions: QuestionItem[]; answers: Record<string, unknown> }) {
  return (
    <div className="question-card question-resolved">
      {questions.map((q, idx) => (
        <div key={idx} className="question-item">
          {q.header && <div className="question-header">{q.header}</div>}
          <div className="question-card-text">{q.question}</div>
          <div className="question-answer-display">
            {String(answers[idx] ?? answers[String(idx)] ?? '(no answer)')}
          </div>
        </div>
      ))}
    </div>
  )
}

function QuestionCard({ sessionId, questionId, questions }: { sessionId: string; questionId: string; questions: QuestionItem[] }) {
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
    <div className="question-card">
      {questions.map((q, idx) => (
        <div key={idx} className="question-item">
          {q.header && <div className="question-header">{q.header}</div>}
          <div className="question-card-text">{q.question}</div>
          {q.options ? (
            <div className="question-options">
              {q.options.map((opt) => (
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
                  <span>{opt}</span>
                </label>
              ))}
            </div>
          ) : (
            <input
              className="question-input"
              type="text"
              placeholder="Type your answer..."
              value={answers[idx] ?? ''}
              onChange={(e) => setAnswer(idx, e.target.value)}
              onKeyDown={(e) => { if (e.key === 'Enter' && questions.length === 1) handleSubmit() }}
              disabled={submitting}
            />
          )}
        </div>
      ))}
      <div className="question-actions">
        <button className="btn-primary" onClick={handleSubmit} disabled={!hasAnswers || submitting}>Submit</button>
        <button className="btn-secondary" onClick={handleDismiss} disabled={submitting}>Dismiss</button>
      </div>
    </div>
  )
}

export default function ChatView({ sessionId }: ChatViewProps) {
  const [events, setEvents] = useState<Event[]>([])
  const [loading, setLoading] = useState(true)
  const [sessionDetail, setSessionDetail] = useState<Session | null>(null)
  const [menuOpen, setMenuOpen] = useState(false)
  const [confirmAction, setConfirmAction] = useState<{ title: string; message: string; onConfirm: () => void } | null>(null)
  const scrollRef = useRef<HTMLDivElement>(null)
  const menuRef = useRef<HTMLDivElement>(null)
  const userScrolledUp = useRef(false)

  const subscribe = useWsStore((s) => s.subscribe)
  const unsubscribe = useWsStore((s) => s.unsubscribe)
  const addEventListener = useWsStore((s) => s.addEventListener)
  const removeEventListener = useWsStore((s) => s.removeEventListener)
  const renameSession = useSessionsStore((s) => s.renameSession)
  const clearSession = useSessionsStore((s) => s.clearSession)
  const deleteSession = useSessionsStore((s) => s.deleteSession)

  // Fetch session detail on mount
  useEffect(() => {
    let cancelled = false
    authedFetch(`/api/sessions/${sessionId}`)
      .then((res) => (res.ok ? res.json() : null))
      .then((data: Session | null) => {
        if (!cancelled && data) setSessionDetail(data)
      })
      .catch(() => {})
    return () => { cancelled = true }
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

  // Fetch initial events
  const fetchEvents = useCallback(() => {
    setLoading(true)
    setEvents([])
    userScrolledUp.current = false

    authedFetch(`/api/sessions/${sessionId}/events`)
      .then((res) => (res.ok ? res.json() : []))
      .then((data: Event[]) => {
        setEvents(data)
        setLoading(false)
      })
      .catch(() => {
        setLoading(false)
      })
  }, [sessionId])

  useEffect(() => {
    fetchEvents()
  }, [fetchEvents])

  // Subscribe to WS events for this session
  useEffect(() => {
    subscribe(sessionId)

    const listener = (event: Event) => {
      if (event.session_id === sessionId) {
        setEvents((prev) => {
          // Dedupe by id
          if (prev.some((e) => e.id === event.id)) return prev
          return [...prev, event]
        })
      }
    }

    addEventListener(listener)

    return () => {
      removeEventListener(listener)
      unsubscribe(sessionId)
    }
  }, [sessionId, subscribe, unsubscribe, addEventListener, removeEventListener])

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

  // Determine if agent is working
  const agentWorking = (() => {
    for (let i = events.length - 1; i >= 0; i--) {
      const kind = events[i].kind
      if (kind === 'agent-start') return true
      if (kind === 'agent-end') return false
    }
    return false
  })()

  const agentStatus = deriveAgentStatus(events)

  // Toolbar actions
  const handleRename = async () => {
    setMenuOpen(false)
    const currentName = sessionDetail?.name ?? ''
    const newName = window.prompt('Rename session:', currentName)
    if (newName && newName !== currentName) {
      await renameSession(sessionId, newName)
      setSessionDetail((prev) => prev ? { ...prev, name: newName } : prev)
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
        fetchEvents()
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
        <span className="chat-toolbar-model">{sessionDetail?.model ?? 'default'}</span>
        <span className="chat-toolbar-status">
          <span className={getStatusDotClass(agentStatus)} />
          {getStatusLabel(agentStatus)}
        </span>
        <div className="chat-toolbar-menu-wrapper" ref={menuRef}>
          <button className="chat-toolbar-menu" onClick={() => setMenuOpen(!menuOpen)} type="button">
            <svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor"><circle cx="8" cy="3" r="1.5" /><circle cx="8" cy="8" r="1.5" /><circle cx="8" cy="13" r="1.5" /></svg>
          </button>
          {menuOpen && (
            <div className="chat-toolbar-dropdown">
              <button onClick={handleRename}>Rename</button>
              <button onClick={handleClear}>Clear</button>
              <button className="danger" onClick={handleDelete}>Delete</button>
            </div>
          )}
        </div>
      </div>

      <div className="chat-messages" ref={scrollRef} onScroll={handleScroll}>
        {displayItems.length === 0 && (
          <div className="chat-empty">No messages yet. Send one below.</div>
        )}
        {displayItems.map((item) => {
          switch (item.type) {
            case 'user':
              return (
                <div key={item.key} className="chat-row chat-row-user">
                  <div className="chat-bubble chat-bubble-user">{item.text}</div>
                </div>
              )
            case 'assistant':
              return (
                <div key={item.key} className="chat-row chat-row-assistant">
                  <div className="chat-bubble chat-bubble-assistant">{item.text}</div>
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
                <div key={item.key} className="chat-row chat-row-status">
                  <span className="chat-status">{item.text}</span>
                </div>
              )
            case 'system':
              return (
                <div key={item.key} className="chat-row chat-row-system">
                  <div className="chat-system-notice">{item.text}</div>
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
                  <QuestionCard sessionId={sessionId} questionId={item.questionId} questions={item.questions} />
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
      </div>
      <InputBar sessionId={sessionId} agentWorking={agentWorking} />
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

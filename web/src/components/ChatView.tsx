import { useCallback, useEffect, useRef, useState } from 'react'
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
import 'highlight.js/styles/github-dark.css'

interface ChatViewProps {
  sessionId: string
}

/** Option object from an AskUserQuestion, with optional description */
interface QuestionOption {
  label: string
  description?: string
}

/** Structured question within a question event */
interface QuestionItem {
  question: string
  header?: string
  multiSelect?: boolean
  options?: string[]
  optionObjects?: QuestionOption[]
}

/** A display item derived from one or more raw events. */
type DisplayItem =
  | { type: 'user'; text: string; key: string; ts: number }
  | { type: 'assistant'; text: string; key: string; ts: number }
  | {
      type: 'tool'
      toolName: string
      input?: Record<string, unknown>
      output?: Record<string, unknown>
      error?: string
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
  | { type: 'question'; questionId: string; questions: QuestionItem[]; key: string }
  | {
      type: 'question-resolved'
      questionId: string
      questions: QuestionItem[]
      answers: Record<string, unknown>
      key: string
    }

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

function getStatusLabel(status: AgentStatus): string {
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

function getStatusDotClass(status: AgentStatus): string {
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

function buildDisplayItems(events: Event[]): DisplayItem[] {
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

  for (const ev of events) {
    switch (ev.kind) {
      case 'user': {
        flushAssistant()
        const text = (ev.data.text as string) ?? JSON.stringify(ev.data)
        items.push({ type: 'user', text, key: ev.id, ts: ev.ts })
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
        const idx = openTools.get(toolUseId)
        if (idx !== undefined) {
          const existing = items[idx] as Extract<DisplayItem, { type: 'tool' }>
          const errorText = ev.data.error as string | undefined
          const output = (ev.data.output as Record<string, unknown>) ?? undefined
          items[idx] = { ...existing, isRunning: false, output, error: errorText }
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
            isRunning: false,
            key: ev.id,
          })
        }
        break
      }
      case 'agent-start': {
        flushAssistant()
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
        if ((ev.data.status as string) === 'crashed') {
          const reason = (ev.data.reason as string) ?? 'unknown error'
          const stderr = ev.data.stderr as string | undefined
          const crashText = stderr
            ? `Agent crashed: ${reason}\n\n${stderr}`
            : `Agent crashed: ${reason}`
          items.push({ type: 'system', text: crashText, key: ev.id, ts: ev.ts })
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
        items.push({ type: 'system', text: 'Interrupted', key: ev.id, ts: ev.ts })
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

function formatTime(ts: number): string {
  if (!ts) return ''
  return new Date(ts).toLocaleTimeString([], {
    hour: 'numeric',
    minute: '2-digit',
    second: '2-digit',
  })
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
  const [events, setEvents] = useState<Event[]>([])
  const [loading, setLoading] = useState(true)
  const [sessionDetail, setSessionDetail] = useState<Session | null>(null)
  const [menuOpen, setMenuOpen] = useState(false)
  const [confirmAction, setConfirmAction] = useState<{
    title: string
    message: string
    onConfirm: () => void
  } | null>(null)
  const [modelDropdownOpen, setModelDropdownOpen] = useState(false)
  const [availableModels, setAvailableModels] = useState<ModelInfo[]>([])
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

  // Show thinking when:
  // 1. Agent is actively working but no text has arrived yet (last display item is agent-start)
  // 2. Last event is 'user' (message just sent, CLI hasn't started yet)
  const showThinking = (() => {
    if (displayItems.length === 0) return false
    const lastDisplay = displayItems[displayItems.length - 1].type
    // After agent-start but before first text/tool
    if (agentWorking && lastDisplay === 'agent-start') return true
    // User just sent, waiting for CLI to boot (last raw event is 'user')
    if (events.length > 0 && events[events.length - 1].kind === 'user') return true
    return false
  })()

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

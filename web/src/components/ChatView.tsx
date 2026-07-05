import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import rehypeHighlight from 'rehype-highlight'
import SafeMarkdown from './SafeMarkdown'
import type { Event, Session } from '../types/api'
import { authedFetch } from '../store/auth'
import { useWsStore } from '../store/ws'
import { useSessionsStore, type PendingUserMessage } from '../store/sessions'
import { effortOptionsForModel, type ProviderInfo } from '../store/resources'
import InputBar from './InputBar'
import ToolUseBlock from './ToolUseBlock'
import ConfirmDialog from './ConfirmDialog'
import { MenuButton, type MenuItem } from './Dropdown'
import ModelPicker from './ModelPicker'
import TodoPanel from './TodoPanel'
import { parseTodoItems, latestTodoSnapshot, type TodoItem } from '../types/todo'
import {
  EMPTY_EVENTS,
  buildDisplayItems,
  deriveAgentStatus,
  formatTime,
  getStatusDotClass,
  getStatusLabel,
  type MessageAttachment,
  type QuestionItem,
} from './chat/events'
import 'highlight.js/styles/github-dark.css'

// Stable empty array so the memoized `todos` keeps referential equality
// when there are no todos (avoids re-renders of TodoPanel and a
// fresh-array warning from React fast refresh).
const EMPTY_TODOS: TodoItem[] = []
const EMPTY_PENDING_MESSAGES: PendingUserMessage[] = []

// Interactive-session context prompt: the banner appears once context
// occupancy reaches this, and after "Continue" reappears each time it grows
// another CONTEXT_PROMPT_STEP. Interactive sessions are never auto-compacted
// — the user chooses (compact / clear / continue). Workers auto-compact
// server-side at 200k instead and never see the banner.
const CONTEXT_PROMPT_THRESHOLD = 150_000
const CONTEXT_PROMPT_STEP = 20_000

/** A plugin-contributed full-page entry for the session page (manifest
 *  `session_items`), surfaced as a toolbar button. */
interface PluginItem {
  plugin: string
  id: string
  label: string
}

interface ChatViewProps {
  sessionId: string
  onOpenTodos?: () => void
  /** Plugin session-page entries to surface as toolbar buttons. */
  pluginItems?: PluginItem[]
  /** Open a plugin entry's full-page view by its item id. */
  onOpenPlugin?: (itemId: string) => void
}

/**
 * Renders the "image attached" indicator chips under a user message. Shown
 * for every provider — the chips come off the persisted `user` event, so a
 * message shows what it carried regardless of which model (Claude, Ollama,
 * mock) actually consumed the bytes. An image-type attachment gets a
 * picture icon, anything else a paperclip.
 */
function MessageAttachments({ attachments }: { attachments?: MessageAttachment[] }) {
  if (!attachments || attachments.length === 0) return null
  return (
    <div className="attachment-chips chat-attachment-chips" data-testid="message-attachments">
      {attachments.map((att, i) => (
        <span key={`${att.filename}-${i}`} className="attachment-chip">
          <span className="attachment-chip-icon">
            {att.mimeType.startsWith('image/') ? '\u{1F5BC}\u{FE0F}' : '\u{1F4CE}'}
          </span>
          <span className="attachment-chip-name">{att.filename}</span>
        </span>
      ))}
    </div>
  )
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

export default function ChatView({
  sessionId,
  onOpenTodos,
  pluginItems,
  onOpenPlugin,
}: ChatViewProps) {
  const events = useSessionsStore((s) => s.eventsBySession[sessionId] ?? EMPTY_EVENTS)
  const loading = useSessionsStore((s) => s.loadingEventsBySession[sessionId] ?? true)
  const eventsError = useSessionsStore((s) => s.eventsErrorBySession[sessionId] ?? false)
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
  const [confirmAction, setConfirmAction] = useState<{
    title: string
    message: string
    onConfirm: () => void
  } | null>(null)
  const [availableModels, setAvailableModels] = useState<ModelInfo[]>([])
  const [availableProviders, setAvailableProviders] = useState<ProviderInfo[]>([])
  const [modelsError, setModelsError] = useState(false)
  const [loadedTodos, setLoadedTodos] = useState<TodoItem[]>([])
  // Session id whose session-detail / todo-snapshot fetch failed. The
  // chat itself still works, so this only drives a retry banner rather
  // than blocking the view. Stored as the failing session id (not a
  // boolean) so an error from a previous session never bleeds into the
  // current one. Bumping `metaRetryNonce` re-runs both fetch effects.
  const [detailErrorFor, setDetailErrorFor] = useState<string | null>(null)
  const [todosErrorFor, setTodosErrorFor] = useState<string | null>(null)
  const [metaRetryNonce, setMetaRetryNonce] = useState(0)
  const metaError = detailErrorFor === sessionId || todosErrorFor === sessionId
  // Message from a refused session PATCH (e.g. 409 "agent is mid-turn"
  // on a provider/account switch). Cleared on the next successful patch
  // or by the dismiss button.
  const [patchError, setPatchError] = useState<string | null>(null)
  // Cross-provider/account model switch awaiting the user's choice in the
  // modal below (hand over a summary / clear & switch / cancel).
  const [pendingModelSwitch, setPendingModelSwitch] = useState<string | null>(null)
  // Suppression floor for the interactive context prompt: the banner shows
  // once contextTokens reaches `until`. Picking Continue bumps it by
  // CONTEXT_PROMPT_STEP so the choice returns as the window keeps filling.
  // `boundary` pins the dismissal to the conversation segment it was made in
  // (the seq of the last handover event, null before any): a compaction or
  // model switch starts a fresh window, so an old dismissal no longer applies.
  const [ctxPromptDismissal, setCtxPromptDismissal] = useState<{
    boundary: number | null
    until: number
  } | null>(null)
  const scrollRef = useRef<HTMLDivElement>(null)
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
      .then((res) => {
        if (!res.ok) throw new Error(`session fetch failed: ${res.status}`)
        return res.json()
      })
      .then((data: Session) => {
        if (!cancelled) {
          setSessionDetail(data)
          setDetailErrorFor(null)
        }
      })
      .catch(() => {
        if (!cancelled) setDetailErrorFor(sessionId)
      })
    return () => {
      cancelled = true
    }
  }, [sessionId, metaRetryNonce])

  // Fetch the current todo snapshot on load so a freshly opened session shows
  // existing todos before any live `todo` event arrives over the WS.
  useEffect(() => {
    let cancelled = false
    authedFetch(`/api/sessions/${sessionId}/todos`)
      .then((res) => {
        if (!res.ok) throw new Error(`todos fetch failed: ${res.status}`)
        return res.json()
      })
      .then((data) => {
        // Always set (the endpoint returns `{ todos: [] }` for a fresh
        // session), so switching sessions clears any prior snapshot.
        if (!cancelled) {
          setLoadedTodos(parseTodoItems(data?.todos))
          setTodosErrorFor(null)
        }
      })
      .catch(() => {
        if (!cancelled) setTodosErrorFor(sessionId)
      })
    return () => {
      cancelled = true
    }
  }, [sessionId, metaRetryNonce])

  // Listen for the server's `session-cleared` broadcast and drop the
  // cached snapshot. Without this the panel keeps rendering pre-clear
  // todos until the user navigates away — the load-time fetch above
  // only runs on sessionId change, not on a same-session wipe.
  // Also zero the seeded context occupancy and re-arm the context prompt:
  // the events list empties, so the badge would otherwise fall back to the
  // stale pre-clear `sessionDetail.context_tokens`.
  useEffect(() => {
    const onCleared = (e: CustomEvent<{ sessionId: string }>) => {
      if (e.detail?.sessionId !== sessionId) return
      setLoadedTodos([])
      setSessionDetail((prev) => (prev ? { ...prev, context_tokens: 0 } : prev))
      setCtxPromptDismissal(null)
    }
    window.addEventListener('peckboard:session-cleared', onCleared as EventListener)
    return () => {
      window.removeEventListener('peckboard:session-cleared', onCleared as EventListener)
    }
  }, [sessionId])
  // Reflect server-pushed session updates (the async model-switch handover
  // flip lands here): refresh the local detail so the model label and the
  // composer's disabled-during-handover state track the backend without a
  // manual refetch.
  useEffect(() => {
    const onUpdated = (e: CustomEvent<{ session_id: string; data: Session }>) => {
      if (e.detail?.session_id !== sessionId) return
      const updated = e.detail?.data
      if (updated && typeof updated === 'object') setSessionDetail(updated)
    }
    window.addEventListener('peckboard:session-updated', onUpdated as EventListener)
    return () => {
      window.removeEventListener('peckboard:session-updated', onUpdated as EventListener)
    }
  }, [sessionId])

  // Load the model catalogue once per session mount so the 3-dot menu's
  // "Model" submenu has options ready the first time the user opens it.
  useEffect(() => {
    if (availableModels.length > 0 || modelsError) return
    authedFetch('/api/models')
      .then((res) => {
        if (!res.ok) throw new Error(`models fetch failed: ${res.status}`)
        return res.json()
      })
      .then((data) => {
        if (data && Array.isArray(data.models)) {
          setAvailableModels(data.models as ModelInfo[])
          if (Array.isArray(data.providers)) {
            setAvailableProviders(data.providers as ProviderInfo[])
          }
        } else {
          setModelsError(true)
        }
      })
      .catch(() => setModelsError(true))
  }, [availableModels.length, modelsError])

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

  // Latest context-window occupancy — live from the turn's `agent-usage`
  // events, seeded by the session fetch. Drives the toolbar context badge.
  // A `handover` event (compaction or model switch) restarts the
  // conversation, so anything recorded before it — including the
  // doc-generation turn's full-context usage — no longer describes the
  // window: occupancy is 0 until the fresh conversation's first turn.
  const contextTokens = useMemo(() => {
    for (let i = events.length - 1; i >= 0; i--) {
      const ev = events[i]
      if (ev.kind === 'handover') return 0
      if (ev.kind !== 'agent-usage') continue
      const ctx = (ev.data?.contextTokens as number) ?? 0
      if (ctx > 0) return ctx
    }
    return sessionDetail?.context_tokens ?? 0
  }, [events, sessionDetail])

  // The context prompt's suppression floor only applies within the
  // conversation segment it was dismissed in — the Compact/Clear buttons
  // bump it past the pre-compact occupancy, which would otherwise mute the
  // banner well past 150k in the fresh conversation.
  const lastHandoverSeq = useMemo(() => {
    for (let i = events.length - 1; i >= 0; i--) {
      if (events[i].kind === 'handover') return events[i].seq
    }
    return null
  }, [events])
  const ctxPromptDismissedUntil =
    ctxPromptDismissal && ctxPromptDismissal.boundary === lastHandoverSeq
      ? ctxPromptDismissal.until
      : CONTEXT_PROMPT_THRESHOLD
  const dismissCtxPrompt = () =>
    setCtxPromptDismissal({ boundary: lastHandoverSeq, until: contextTokens + CONTEXT_PROMPT_STEP })

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
    const currentName = sessionDetail?.name ?? ''
    const newName = window.prompt('Rename session:', currentName)
    if (newName && newName !== currentName) {
      await renameSession(sessionId, newName)
      setSessionDetail((prev) => (prev ? { ...prev, name: newName } : prev))
    }
  }

  const handleClear = () => {
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

  const handleCompact = () => {
    setConfirmAction({
      title: 'Compact context',
      message:
        'Summarize the conversation and drop earlier history from the context window? The transcript stays intact.',
      onConfirm: async () => {
        setConfirmAction(null)
        try {
          const res = await authedFetch(`/api/sessions/${sessionId}/compact`, { method: 'POST' })
          if (!res.ok) {
            const err = (await res.json().catch(() => null)) as { error?: string } | null
            setPatchError(err?.error ?? `compaction failed (${res.status})`)
          }
        } catch {
          /* ignore */
        }
      },
    })
  }

  const handleDelete = () => {
    setConfirmAction({
      title: 'Delete session',
      message: 'Delete this session and all its events?',
      onConfirm: async () => {
        setConfirmAction(null)
        await deleteSession(sessionId)
      },
    })
  }

  const patchSession = async (patch: Record<string, unknown>) => {
    try {
      const res = await authedFetch(`/api/sessions/${sessionId}`, {
        method: 'PATCH',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(patch),
      })
      if (res.ok) {
        const updated: Session = await res.json()
        setSessionDetail(updated)
        setPatchError(null)
      } else {
        // Surface refusals — the backend 409s a provider/account switch
        // while the agent is mid-turn or a handover is already running.
        // Silently ignoring those made the model picker look broken.
        const err = (await res.json().catch(() => null)) as { error?: string } | null
        setPatchError(err?.error ?? `update failed (${res.status})`)
      }
    } catch {
      /* ignore */
    }
  }

  // Effort options for the current session's model, loaded from that
  // model's provider (Claude/Grok expose the full ladder; Cursor/Ollama
  // only "Default"). Includes the Default entry (value '') which clears
  // any override.
  const effortOptions = effortOptionsForModel(sessionDetail?.model, availableProviders)

  const modelDisplayName = (id: string | null | undefined): string => {
    if (!id) return 'auto'
    const m = availableModels.find((x) => x.id === id)
    return m?.display_name ?? id
  }

  // Three-dot menu. Order is shared with the TabBar context menu (see
  // TabBar.tsx) so a session's controls read the same wherever they
  // surface — that's the rule in CLAUDE.md "Component Reuse".
  //   rename, divider, clear session, terminate agent, delete
  // Plus chat-only entries the TabBar menu doesn't carry: Tasks and
  // plugin pages (moved here from the toolbar), Model and Effort
  // submenus, and a manual "Compact context" action.
  // Mirror of the backend continuity key (provider + account). Switching
  // across it means the incoming model starts cold, so confirm with the
  // user before the PATCH: hand over a summary, or clear & switch fresh.
  const continuityKey = (id: string | null | undefined): string => {
    const m = id ?? ''
    const provider = m.includes(':') ? m.slice(0, m.indexOf(':')) : 'claude'
    const at = m.lastIndexOf('@')
    return `${provider}@${at >= 0 ? m.slice(at + 1) : ''}`
  }
  const requestModelChange = (id: string) => {
    const crosses = continuityKey(sessionDetail?.model) !== continuityKey(id)
    if (crosses && events.length > 0 && !sessionDetail?.is_worker) {
      setPendingModelSwitch(id)
    } else {
      patchSession({ model: id })
    }
  }

  const sessionMenuItems: MenuItem[] = [
    { label: 'Rename', onSelect: handleRename, testId: 'chat-menu-rename' },
    { divider: true },
    {
      label: 'Tasks',
      hint:
        todos.length > 0
          ? `${todos.filter((t) => t.status === 'done').length}/${todos.length}`
          : undefined,
      onSelect: onOpenTodos,
      hidden: !onOpenTodos,
      testId: 'chat-menu-tasks',
    },
    ...(onOpenPlugin
      ? (pluginItems ?? []).map((item) => ({
          label: item.label,
          onSelect: () => onOpenPlugin(item.id),
          testId: `chat-menu-plugin-${item.id}`,
        }))
      : []),
    { divider: true },
    {
      label: 'Model',
      hint: modelDisplayName(sessionDetail?.model),
      submenu:
        availableModels.length > 0
          ? availableModels.map((m) => ({
              label: m.display_name,
              active: m.id === sessionDetail?.model,
              onSelect: () => requestModelChange(m.id),
            }))
          : [{ label: 'Loading models…', disabled: true }],
    },
    {
      label: 'Effort',
      hint: sessionDetail?.effort ?? 'default',
      submenu: effortOptions.map((o) => ({
        label: o.label,
        active: (sessionDetail?.effort ?? '') === o.value,
        onSelect: () => patchSession({ effort: o.value || null }),
      })),
    },
    { divider: true },
    {
      label: 'Compact context',
      onSelect: handleCompact,
      testId: 'chat-menu-compact',
    },
    {
      label: 'Clear session',
      onSelect: handleClear,
      testId: 'chat-menu-clear',
      // Worker sessions are owned by their card; repeating-task
      // sessions are a schedule's run history. Both have their
      // transcript guarded server-side (POST /clear → 409). Hide
      // rather than render an always-erroring control.
      hidden: !!sessionDetail?.is_worker || !!sessionDetail?.repeating_task_id,
    },
    {
      label: 'Terminate agent',
      onSelect: handleTerminateAgent,
      testId: 'chat-toolbar-terminate',
    },
    {
      label: 'Delete',
      danger: true,
      // Worker sessions are owned by their card; the backend refuses
      // DELETE /api/sessions/:id for them. Hide rather than render an
      // always-409 control. Repeating-task sessions delete fine — the
      // run is removed from the task's history, the schedule keeps
      // firing — so the entry stays for them.
      hidden: !!sessionDetail?.is_worker,
      onSelect: handleDelete,
      testId: 'chat-menu-delete',
    },
  ]

  if (loading) {
    return (
      <div className="chat-container">
        <div className="chat-loading">Loading events...</div>
      </div>
    )
  }

  if (eventsError) {
    return (
      <div className="chat-container">
        <div className="fetch-error-pane" role="alert" data-testid="chat-events-error">
          <p>Couldn’t load this conversation.</p>
          <button type="button" onClick={() => fetchEvents(sessionId)}>
            Retry
          </button>
        </div>
      </div>
    )
  }

  return (
    <div className="chat-container">
      {/* Toolbar */}
      <div className="chat-toolbar">
        <span className="chat-toolbar-name">{sessionDetail?.name ?? 'Session'}</span>
        <ModelPicker
          value={sessionDetail?.model ?? ''}
          onChange={(id) => requestModelChange(id)}
          models={availableModels}
          valueLabel={modelDisplayName(sessionDetail?.model)}
          triggerClassName="chat-toolbar-model"
          showChevron={false}
          align="left"
          ariaLabel="Change model"
          defaultLabel="Auto"
          emptyHint={modelsError ? 'Failed to load models — reopen to retry' : 'Loading models…'}
          onOpen={() => {
            // Reopening after a failed fetch clears the error flag, which
            // re-arms the load effect (it bails while `modelsError` is set).
            if (modelsError) setModelsError(false)
          }}
          testId="chat-toolbar-model"
        />
        <span className="chat-toolbar-status">
          <span className={getStatusDotClass(agentStatus)} />
          {getStatusLabel(agentStatus)}
        </span>
        {contextTokens > 0 && (
          <span
            className={`chat-toolbar-context${
              contextTokens >= 150_000 ? ' over' : contextTokens >= 120_000 ? ' warn' : ''
            }`}
            title={`Context size: ${contextTokens.toLocaleString()} tokens${
              sessionDetail?.is_worker
                ? ' (auto-compacts at 200k)'
                : " — you'll be prompted to compact past 150k"
            }`}
            data-testid="chat-toolbar-context"
          >
            {Math.round(contextTokens / 1000)}k ctx
          </span>
        )}
        <MenuButton
          ariaLabel="Session menu"
          triggerClassName="chat-toolbar-menu"
          items={sessionMenuItems}
          testId="chat-toolbar-menu"
        />
      </div>

      {metaError && (
        <div className="fetch-error-banner" role="alert" data-testid="chat-meta-error">
          <span>Some session details failed to load.</span>
          <button type="button" onClick={() => setMetaRetryNonce((n) => n + 1)}>
            Retry
          </button>
        </div>
      )}

      {patchError && (
        <div className="fetch-error-banner" role="alert" data-testid="chat-patch-error">
          <span>{patchError}</span>
          <button type="button" onClick={() => setPatchError(null)}>
            Dismiss
          </button>
        </div>
      )}

      {!sessionDetail?.is_worker &&
        !sessionDetail?.repeating_task_id &&
        contextTokens >= ctxPromptDismissedUntil && (
          <div className="chat-context-banner" role="status" data-testid="chat-context-prompt">
            <span className="chat-context-banner-text">
              This conversation is using {Math.round(contextTokens / 1000)}k tokens of context.
              Compact it, clear it, or keep going at higher cost.
            </span>
            <div className="chat-context-banner-actions">
              <button
                type="button"
                className="btn-primary btn-sm"
                data-testid="chat-context-compact"
                onClick={() => {
                  dismissCtxPrompt()
                  handleCompact()
                }}
              >
                Compact
              </button>
              <button
                type="button"
                className="btn-secondary btn-sm"
                data-testid="chat-context-clear"
                onClick={() => {
                  dismissCtxPrompt()
                  handleClear()
                }}
              >
                Clear
              </button>
              <button
                type="button"
                className="btn-secondary btn-sm"
                data-testid="chat-context-continue"
                onClick={dismissCtxPrompt}
              >
                Continue
              </button>
            </div>
          </div>
        )}

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
                    <MessageAttachments attachments={item.attachments} />
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
            case 'handover-start':
              return (
                <div key={item.key} className="chat-row chat-row-system">
                  <div className="chat-agent-start">
                    <span className="chat-agent-start-label">
                      {item.compaction ? 'Compaction' : 'Handover'}
                    </span>
                    <span className="chat-agent-start-detail">
                      {item.compaction
                        ? 'summarizing context to free the window'
                        : `preparing context for ${item.to.replace(/^claude:/, '')}`}
                    </span>
                    <span className="chat-agent-start-time">{formatTime(item.ts)}</span>
                  </div>
                </div>
              )
            case 'handover-aborted': {
              // A reason means the doc turn FAILED (e.g. an expired login's
              // 401) rather than being user-cancelled. The context is safe
              // either way, but a failed compaction leaves the session
              // stuck near the window limit — so spell out the ways
              // forward: log in again from Settings and retry, or clear /
              // switch sessions at the cost of this context.
              const failed = item.reason !== null
              return (
                <div key={item.key} className="chat-row chat-row-system">
                  <div className="chat-agent-start">
                    <span className="chat-agent-start-label">
                      {item.compaction
                        ? failed
                          ? 'Compaction failed'
                          : 'Compaction cancelled'
                        : failed
                          ? 'Model switch failed'
                          : 'Switch cancelled'}
                    </span>
                    <span className="chat-agent-start-detail">
                      {item.compaction
                        ? 'context left intact'
                        : `staying on ${item.from.replace(/^claude:/, '')} — context kept`}
                    </span>
                    <span className="chat-agent-start-time">{formatTime(item.ts)}</span>
                  </div>
                  {failed && (
                    <div
                      className="chat-handover-failed"
                      role="alert"
                      data-testid="chat-handover-failed"
                    >
                      <span className="chat-handover-failed-reason">{item.reason}</span>
                      <span>
                        {item.compaction
                          ? 'Nothing was compacted and no context was lost. If your login expired, '
                          : 'The model was not switched. If your login expired, '}
                        <a href="/settings">log in again from Settings</a>
                        {item.compaction
                          ? ' and retry the compaction — or clear / switch sessions, accepting that this context will be lost.'
                          : ' and retry.'}
                      </span>
                    </div>
                  )}
                </div>
              )
            }
            case 'handover':
              return (
                <div key={item.key} className="chat-row chat-row-system">
                  <details className="chat-handover" data-testid="chat-handover">
                    <summary className="chat-handover-summary">
                      <span className="chat-handover-icon" aria-hidden="true">
                        {'↔️'}
                      </span>
                      <span>
                        {item.compaction
                          ? 'Context compacted'
                          : `Context handed over to ${item.to.replace(/^claude:/, '')}`}
                      </span>
                      <span className="chat-handover-time">{formatTime(item.ts)}</span>
                    </summary>
                    <SafeMarkdown className="chat-markdown chat-handover-doc">
                      {item.doc}
                    </SafeMarkdown>
                  </details>
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
                    images={item.images}
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
              <MessageAttachments attachments={p.attachments} />
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
      <InputBar
        key={sessionId}
        sessionId={sessionId}
        agentWorking={agentWorking}
        handoverActive={!!sessionDetail?.handover_to_model}
      />
      {pendingModelSwitch !== null &&
        createPortal(
          <div
            className="modal-backdrop"
            onMouseDown={(e) => {
              if (e.target === e.currentTarget) setPendingModelSwitch(null)
            }}
          >
            <div
              className="confirm-dialog"
              data-testid="model-switch-prompt"
              onMouseDown={(e) => e.stopPropagation()}
            >
              <h3 className="confirm-dialog-title">Switch model?</h3>
              <p className="confirm-dialog-message">
                {`Switching to ${modelDisplayName(pendingModelSwitch)} crosses a provider or account boundary — the new model starts with no memory of this conversation. Hand over a summary, or clear the context and switch fresh.`}
              </p>
              <div className="confirm-dialog-actions">
                <button className="btn-secondary" onClick={() => setPendingModelSwitch(null)}>
                  Cancel
                </button>
                <button
                  className="btn-secondary"
                  data-testid="model-switch-clear"
                  onClick={async () => {
                    const target = pendingModelSwitch
                    setPendingModelSwitch(null)
                    await clearSession(sessionId)
                    patchSession({ model: target })
                  }}
                >
                  Clear &amp; switch
                </button>
                <button
                  className="btn-primary"
                  data-testid="model-switch-handover"
                  onClick={() => {
                    const target = pendingModelSwitch
                    setPendingModelSwitch(null)
                    patchSession({ model: target })
                  }}
                >
                  Hand over context
                </button>
              </div>
            </div>
          </div>,
          document.body,
        )}
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

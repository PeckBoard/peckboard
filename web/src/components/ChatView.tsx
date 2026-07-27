import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { useVirtualizer } from '@tanstack/react-virtual'
import rehypeHighlight from 'rehype-highlight'
import type { Components } from 'react-markdown'
import SafeMarkdown from './SafeMarkdown'
import type { CostTable, Event, Session } from '../types/api'
import { authedFetch } from '../store/auth'
import { useWsStore } from '../store/ws'
import { useSessionsStore, type PendingUserMessage } from '../store/sessions'
import {
  effortOptionsForModel,
  imagesAllowedForModel,
  interruptAffordanceForModel,
  modelThinks,
  providerForModel,
  useResourcesStore,
  type ProviderInfo,
} from '../store/resources'
import { useUsageStore } from '../store/usage'
import { usageCost } from '../util/cost'
import { downloadTranscript } from '../util/transcript'
import InputBar from './InputBar'
import ToolUseBlock from './ToolUseBlock'
import MermaidBlock from './MermaidBlock'
import DiffBlock from './DiffBlock'
import ConfirmDialog from './ConfirmDialog'
import Modal from './Modal'
import RenameModal from './RenameModal'
import { MenuButton, type MenuItem } from './Dropdown'
import ModelPicker from './ModelPicker'
import TodoPanel from './TodoPanel'
import PreHatchActivity from './chat/PreHatchActivity'
import { fetchPlanId, openPlan } from '../lib/plan'
import { openReport } from '../lib/reports'
import { describeActionError } from '../utils/actionError'
import { parseTodoItems, latestTodoSnapshot, type TodoItem } from '../types/todo'
import {
  EMPTY_EVENTS,
  createDisplayItemsFolder,
  deriveAgentStatus,
  formatTime,
  getStatusDotClass,
  getStatusLabel,
  type AgentStatus,
  type DisplayItem,
  type MessageAttachment,
  type QuestionItem,
} from './chat/events'
import 'highlight.js/styles/github-dark.css'

// Coarse announcement key: `working` and `tool` collapse into one "busy"
// state so an agentic turn that runs ten tools doesn't announce twenty
// times. Only a change of key is worth speaking.
type AnnounceKey = 'busy' | 'idle' | 'crashed' | 'questioning'
function announceKey(status: AgentStatus): AnnounceKey {
  return status === 'working' || status === 'tool' ? 'busy' : status
}

function announcementFor(key: AnnounceKey, hasReply: boolean): string {
  switch (key) {
    case 'busy':
      return 'Agent working'
    case 'idle':
      // Turn boundary only: the reply itself is NOT echoed here. A polite
      // region that repeats the message body duplicates every word in the
      // accessibility tree, and a long agent reply read start-to-finish is
      // exactly the chattiness this region has to avoid. "There is a reply"
      // is the cue; the conversation region below holds the text to read.
      return hasReply ? 'Agent replied' : 'Agent finished'
    case 'crashed':
      // Deliberately NOT the visible row's "Agent crashed" wording: the
      // announcement is a second copy of that string in the DOM, and specs
      // (plus users searching the page) should not see it twice.
      return 'The agent stopped unexpectedly'
    case 'questioning':
      return 'Agent is awaiting your answer'
  }
}

// Stable empty array so the memoized `todos` keeps referential equality
// when there are no todos (avoids re-renders of TodoPanel and a
// fresh-array warning from React fast refresh).
const EMPTY_TODOS: TodoItem[] = []
const EMPTY_PENDING_MESSAGES: PendingUserMessage[] = []

// Render fenced ```mermaid blocks in chat markdown as diagrams — same
// wiring PlanView uses for plan bodies.
const chatMarkdownComponents: Components = {
  code({ className, children }) {
    const text = String(children ?? '')
    if (className && /\blanguage-mermaid\b/.test(className)) {
      return <MermaidBlock code={text.replace(/\n$/, '')} />
    }
    return <code className={className}>{children}</code>
  },
}

/** Live stopwatch; isolated so the 1s tick re-renders only this span. */
function ElapsedSince({ since }: { since: number }) {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 1000)
    return () => clearInterval(id)
  }, [])
  const s = Math.max(0, Math.floor((now - since) / 1000))
  const mm = Math.floor(s / 60)
  return <span className="chat-thinking-elapsed">{mm > 0 ? `${mm}m ${s % 60}s` : `${s}s`}</span>
}
/** No event at all for this long while the agent is "working" means the turn
 *  is very likely dead (dispatch failed, the process died before emitting, a
 *  dropped WS frame) — long enough that a merely slow model never trips it. */
const STALL_MS = 90_000

/** True once `sinceTs` is older than `thresholdMs` while `active`. `activityKey`
 *  is the newest event's seq: the stall is stored against the key that produced
 *  it, so the very next event clears it in the same render as it arrives — no
 *  setState in the effect body, no cascading render. */
function useStalled(
  active: boolean,
  activityKey: number,
  sinceTs: number,
  thresholdMs: number,
): boolean {
  const [stalledKey, setStalledKey] = useState<number | null>(null)
  useEffect(() => {
    if (!active || sinceTs <= 0) return
    const remaining = Math.max(0, sinceTs + thresholdMs - Date.now())
    const id = setTimeout(() => setStalledKey(activityKey), remaining)
    return () => clearTimeout(id)
  }, [active, activityKey, sinceTs, thresholdMs])
  return active && sinceTs > 0 && stalledKey === activityKey
}

/** Compact duration for the usage chip: 3s, 1m 12s. */
function formatTurnDuration(ms: number): string {
  const s = Math.round(ms / 1000)
  if (s < 60) return `${s}s`
  return `${Math.floor(s / 60)}m ${s % 60}s`
}

/** Heuristic: render a user message as markdown only when it contains
 *  markdown constructs — plain prose keeps its typed line breaks via the
 *  bubble's pre-wrap. Raw HTML stays escaped either way (SafeMarkdown). */
const USER_MARKDOWN_RE =
  /```|^#{1,6}\s|\*\*|__|^\s*[-*+]\s+\S|^\s*\d+\.\s+\S|\[[^\]]+\]\([^)]+\)|`[^`]+`|^>\s/m

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
 * One image attachment that self-upgrades from a chip to a thumbnail: the
 * attachments API needs the auth header, so the bytes are fetched and
 * swapped in as an object URL (no plain <img src>). Any failure leaves the
 * plain chip. Click opens the shared lightbox.
 */
function AttachmentThumb({
  sessionId,
  id,
  filename,
}: {
  sessionId: string
  id: string
  filename: string
}) {
  const [url, setUrl] = useState<string | null>(null)
  const [open, setOpen] = useState(false)
  useEffect(() => {
    let objectUrl: string | null = null
    let cancelled = false
    authedFetch(`/api/sessions/${sessionId}/attachments/${id}`)
      .then((r) => (r.ok ? r.blob() : Promise.reject(new Error(String(r.status)))))
      .then((b) => {
        const u = URL.createObjectURL(b)
        if (cancelled) {
          URL.revokeObjectURL(u)
          return
        }
        objectUrl = u
        setUrl(u)
      })
      .catch(() => {
        // Deleted/stale upload — keep the chip.
      })
    return () => {
      cancelled = true
      if (objectUrl) URL.revokeObjectURL(objectUrl)
    }
  }, [sessionId, id])
  if (!url) {
    return (
      <span className="attachment-chip">
        <span className="attachment-chip-icon">{'\u{1F5BC}\u{FE0F}'}</span>
        <span className="attachment-chip-name">{filename}</span>
      </span>
    )
  }
  return (
    <>
      <button
        type="button"
        className="tool-image-thumb"
        onClick={() => setOpen(true)}
        aria-label={`Open ${filename}`}
        data-testid="attachment-thumb"
      >
        <img src={url} alt={filename} loading="lazy" />
      </button>
      {open && (
        <Modal
          onClose={() => setOpen(false)}
          className="image-lightbox"
          backdropClassName="image-lightbox-backdrop"
        >
          <img src={url} alt={filename} className="image-lightbox-img" />
        </Modal>
      )}
    </>
  )
}

/**
 * Attachments under a user message. Image attachments with a recorded id
 * render as inline thumbnails (fetched with auth); anything else keeps the
 * filename chip. The chips come off the persisted `user` event, so a
 * message shows what it carried regardless of provider.
 */
function MessageAttachments({
  sessionId,
  attachments,
}: {
  sessionId: string
  attachments?: MessageAttachment[]
}) {
  if (!attachments || attachments.length === 0) return null
  return (
    <div className="attachment-chips chat-attachment-chips" data-testid="message-attachments">
      {attachments.map((att, i) =>
        att.id && att.mimeType.startsWith('image/') ? (
          <AttachmentThumb
            key={`${att.id}-${i}`}
            sessionId={sessionId}
            id={att.id}
            filename={att.filename}
          />
        ) : (
          <span key={`${att.filename}-${i}`} className="attachment-chip">
            <span className="attachment-chip-icon">
              {att.mimeType.startsWith('image/') ? '\u{1F5BC}\u{FE0F}' : '\u{1F4CE}'}
            </span>
            <span className="attachment-chip-name">{att.filename}</span>
          </span>
        ),
      )}
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
  requestId,
  questions,
}: {
  sessionId: string
  questionId: string
  requestId?: string
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
          data: {
            question_id: questionId,
            ...(requestId ? { request_id: requestId } : {}),
            answers: answerMap,
          },
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
          data: {
            question_id: questionId,
            ...(requestId ? { request_id: requestId } : {}),
            rejected: true,
          },
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

/**
 * One feed row. Memoized: the incremental fold keeps item identity stable
 * for untouched history, so a streamed token chunk re-renders only the
 * growing bubble instead of every mounted row.
 */
const ChatRow = memo(function ChatRow({
  item,
  sessionId,
  costTable,
}: {
  item: DisplayItem
  sessionId: string
  costTable: CostTable
}) {
  switch (item.type) {
    case 'user':
      return (
        <div className="chat-row chat-row-user">
          <div className="chat-bubble chat-bubble-user">
            {item.preHatchEnriched && (
              <div
                className="chat-prehatch-badge"
                title="This message was enriched by the pre-hatcher before dispatch"
              >
                ⚡ pre-hatched
              </div>
            )}
            {USER_MARKDOWN_RE.test(item.text) ? (
              <SafeMarkdown
                className="chat-markdown chat-user-markdown"
                rehypePlugins={[rehypeHighlight]}
                components={chatMarkdownComponents}
              >
                {item.text}
              </SafeMarkdown>
            ) : (
              item.text
            )}
            {item.preHatchEnriched && item.preHatchOriginal && (
              <details className="chat-prehatch-original" data-testid="chat-prehatch-original">
                <summary>Original message</summary>
                <div className="chat-prehatch-original-text">{item.preHatchOriginal}</div>
              </details>
            )}
            <MessageAttachments sessionId={sessionId} attachments={item.attachments} />
            <div className="chat-time chat-time-user">{formatTime(item.ts)}</div>
          </div>
        </div>
      )
    case 'pre-hatch':
      return (
        <div className="chat-row chat-row-user">
          <div
            className="chat-bubble chat-bubble-user chat-bubble-prehatch"
            data-testid="chat-prehatch"
          >
            {item.text}
            <PreHatchActivity
              tempSessionId={item.tempSessionId}
              model={item.model}
              sessionId={sessionId}
            />
            <div className="chat-time chat-time-user">{formatTime(item.ts)}</div>
          </div>
        </div>
      )
    case 'assistant':
      return (
        <div className="chat-row chat-row-assistant">
          <div className="chat-bubble chat-bubble-assistant">
            <SafeMarkdown
              className="chat-markdown"
              rehypePlugins={[rehypeHighlight]}
              components={chatMarkdownComponents}
            >
              {item.text}
            </SafeMarkdown>
            <div className="chat-time">{formatTime(item.ts)}</div>
          </div>
        </div>
      )
    case 'agent-start':
      return (
        <div className="chat-row chat-row-system">
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
        <div className="chat-row chat-row-system">
          <div className="chat-agent-start">
            <span className="chat-agent-start-label">Agent interrupted</span>
            <span className="chat-agent-start-time">{formatTime(item.ts)}</span>
          </div>
        </div>
      )
    case 'agent-crashed': {
      // Plain row, no bubble/icon — mirrors `agent-start` and
      // `interrupt` so all agent lifecycle notices read the same.
      // The reason sits in the detail chip; exit code and stderr
      // (when the `agent-end` payload carried them) expand below
      // for debugging without leaving the chat.
      const hasStderr = typeof item.stderr === 'string' && item.stderr !== ''
      return (
        <div className="chat-row chat-row-system">
          <div className="chat-crash-row">
            <div className="chat-agent-start">
              <span className="chat-agent-start-label">Agent crashed</span>
              <span className="chat-agent-start-detail">
                {item.reason}
                {item.exitCode !== undefined ? ` (exit ${item.exitCode})` : ''}
              </span>
              <span className="chat-agent-start-time">{formatTime(item.ts)}</span>
            </div>
            {hasStderr && (
              <details className="chat-crash-details" data-testid="chat-crash-details">
                <summary>stderr</summary>
                <pre className="tool-pre tool-pre-stderr">{item.stderr}</pre>
              </details>
            )}
          </div>
        </div>
      )
    }
    case 'handover-start':
      return (
        <div className="chat-row chat-row-system">
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
        <div className="chat-row chat-row-system">
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
            <div className="chat-handover-failed" role="alert" data-testid="chat-handover-failed">
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
        <div className="chat-row chat-row-system">
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
            <SafeMarkdown
              className="chat-markdown chat-handover-doc"
              components={chatMarkdownComponents}
            >
              {item.doc}
            </SafeMarkdown>
          </details>
        </div>
      )
    case 'tool':
      return (
        <div className="chat-row chat-row-tool">
          <ToolUseBlock
            sessionId={sessionId}
            toolName={item.toolName}
            input={item.input}
            output={item.output}
            error={item.error}
            images={item.images}
            isRunning={item.isRunning}
            startTs={item.startTs}
            endTs={item.endTs}
            diff={item.diff}
          />
        </div>
      )
    case 'turn-usage': {
      const usd = item.slices.reduce(
        (sum, s) =>
          sum +
          usageCost(costTable, s.model || null, {
            input_tokens: s.input,
            output_tokens: s.output,
            cache_read_tokens: s.cacheRead,
            cache_creation_tokens: s.cacheCreation,
          }),
        0,
      )
      const cost = usd >= 0.005 ? `$${usd.toFixed(2)}` : usd > 0 ? '<$0.01' : ''
      const out =
        item.outputTokens >= 1000
          ? `${(item.outputTokens / 1000).toFixed(1)}k`
          : String(item.outputTokens)
      const kfmt = (n: number) => (n >= 1000 ? `${(n / 1000).toFixed(1)}k` : String(n))
      const delta =
        item.contextDelta !== null && item.contextDelta !== 0
          ? ` (${item.contextDelta > 0 ? '+' : '−'}${kfmt(Math.abs(item.contextDelta))})`
          : ''
      return (
        <div className="chat-row chat-row-usage">
          <span
            className="chat-turn-usage"
            title={item.slices
              .map((s) => s.model)
              .filter(Boolean)
              .join(', ')}
          >
            {cost && <span>{cost}</span>}
            <span>{out} tok out</span>
            {item.durationMs !== null && item.durationMs >= 1000 && (
              <span>{formatTurnDuration(item.durationMs)}</span>
            )}
            {item.contextTokens > 0 && (
              <span title="Context-window occupancy after this turn (change vs the previous turn)">
                {kfmt(item.contextTokens)} ctx
                {delta}
              </span>
            )}
          </span>
        </div>
      )
    }
    case 'thinking':
      return (
        <div className="chat-row chat-row-system">
          <details className="chat-thinking-block" data-testid="chat-thinking-block">
            <summary className="chat-thinking-summary">
              <span aria-hidden="true">{'\u{1F4AD}'}</span>
              <span>Thought process</span>
            </summary>
            <div className="chat-thinking-body">
              <SafeMarkdown className="chat-markdown chat-thinking-markdown">
                {item.text}
              </SafeMarkdown>
            </div>
          </details>
        </div>
      )
    case 'file-diff':
      return (
        <div className="chat-row chat-row-tool">
          <DiffBlock diff={item.diff} />
        </div>
      )
    case 'status':
      return (
        <div className="chat-row chat-row-system">
          <div className="chat-ready-notice">
            <span>{item.text}</span>
            <span className="chat-ready-time">{formatTime(item.ts)}</span>
          </div>
        </div>
      )
    case 'system':
      return (
        <div className="chat-row chat-row-system">
          {item.reportFolder && item.reportFile ? (
            <button
              className="chat-report-chip"
              data-testid="chat-report-chip"
              onClick={() => openReport(item.reportFolder!, item.reportFile!)}
            >
              <span className="chat-report-chip-icon">{'\u{1F4C4}'}</span>
              <span className="chat-report-chip-body">
                <span className="chat-report-chip-title">{item.text}</span>
                <span className="chat-report-chip-folder">{item.reportFolder}</span>
              </span>
            </button>
          ) : item.detail ? (
            // The event carried no human-readable text. Show a label, not the
            // stringified payload — that stays one click away.
            <details className="chat-unknown-event" data-testid="chat-system-detail">
              <summary className="chat-unknown-summary">
                <span className="chat-unknown-kind">{'ℹ️'}</span>
                <span className="chat-unknown-label">{item.text}</span>
                <span className="chat-agent-start-time">{formatTime(item.ts)}</span>
              </summary>
              <pre className="chat-unknown-json">{JSON.stringify(item.detail, null, 2)}</pre>
            </details>
          ) : (
            <div className="chat-system-notice">
              <span className="chat-system-notice-icon">{'ℹ️'}</span>
              <span>{item.text}</span>
              {(item.count ?? 1) > 1 && (
                <span className="chat-system-notice-count">×{item.count}</span>
              )}
            </div>
          )}
        </div>
      )
    case 'step':
      return (
        <div className="chat-row chat-row-step">
          <div className="chat-step-divider">
            <span>{item.label}</span>
          </div>
        </div>
      )
    case 'question':
      return (
        <div className="chat-row chat-row-system">
          <QuestionCard
            sessionId={sessionId}
            questionId={item.questionId}
            requestId={item.requestId}
            questions={item.questions}
          />
        </div>
      )
    case 'question-resolved':
      return (
        <div className="chat-row chat-row-system">
          <ResolvedQuestionCard questions={item.questions} answers={item.answers} />
        </div>
      )
    case 'unknown':
      // Fallback for event kinds this build doesn't recognize (plugin
      // providers, future backend kinds) — visible, with the payload on
      // demand instead of a silently dropped event.
      return (
        <div className="chat-row chat-row-system">
          <details className="chat-unknown-event" data-testid="chat-unknown-event">
            <summary className="chat-unknown-summary">
              <span className="chat-unknown-kind">{item.kind}</span>
              <span className="chat-unknown-label">unrecognized event</span>
              <span className="chat-agent-start-time">{formatTime(item.ts)}</span>
            </summary>
            <pre className="chat-unknown-json">{JSON.stringify(item.data, null, 2)}</pre>
          </details>
        </div>
      )
  }
})
/** A confirm-gated session action. `run` rejects on failure; the dialog
 *  stays open and shows the reason (or `failMessage` when the thrown
 *  value isn't human-readable) so the user can retry in place. */
type ConfirmActionState = {
  title: string
  message: string
  confirmLabel?: string
  testId?: string
  failMessage: string
  run: () => Promise<void>
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
  const olderEventsError = useSessionsStore((s) => s.olderEventsErrorBySession[sessionId] ?? false)
  const appendEvent = useSessionsStore((s) => s.appendEvent)
  const pendingUserMessages = useSessionsStore(
    (s) => s.pendingUserMessages[sessionId] ?? EMPTY_PENDING_MESSAGES,
  )
  const prunePendingUserMessages = useSessionsStore((s) => s.prunePendingUserMessages)
  const costTable = useUsageStore((s) => s.costTable)
  const fetchCostTable = useUsageStore((s) => s.fetchCostTable)
  useEffect(() => {
    void fetchCostTable()
  }, [fetchCostTable])
  const [sessionDetail, setSessionDetail] = useState<Session | null>(null)
  const [planId, setPlanId] = useState<string | null>(null)
  useEffect(() => {
    let cancelled = false
    void fetchPlanId({ sessionId }).then((id) => {
      if (!cancelled) setPlanId(id)
    })
    return () => {
      cancelled = true
    }
  }, [sessionId, events.length])
  const [confirmAction, setConfirmAction] = useState<ConfirmActionState | null>(null)
  // In-flight / failed state of the confirmed action. A failure used to
  // close the dialog silently, which looked exactly like success.
  const [confirmBusy, setConfirmBusy] = useState(false)
  const [confirmError, setConfirmError] = useState<string | null>(null)
  const [renameOpen, setRenameOpen] = useState(false)
  // Inline interrupt request in flight — locks the button so repeated
  // clicking can't stack interrupts on the agent.
  const [interrupting, setInterrupting] = useState(false)
  const [availableModels, setAvailableModels] = useState<ModelInfo[]>([])
  const [availableProviders, setAvailableProviders] = useState<ProviderInfo[]>([])
  const [modelsError, setModelsError] = useState(false)
  const systemPrompts = useResourcesStore((s) => s.systemPrompts)
  const fetchSystemPrompts = useResourcesStore((s) => s.fetchSystemPrompts)
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
  // Load named system prompts once per mount so the 3-dot menu's
  // "System prompt" submenu has options ready the first time it opens.
  useEffect(() => {
    fetchSystemPrompts()
  }, [fetchSystemPrompts])

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

  // Display items via the incremental fold — appends (one WS event or one
  // token chunk at a time) reuse all prior work instead of an O(n) rebuild
  // per render. Session switches and "Load older" prepends are detected by
  // the folder and trigger a one-off full rebuild.
  const foldRef = useRef<((evs: Event[]) => DisplayItem[]) | null>(null)
  if (foldRef.current === null) foldRef.current = createDisplayItemsFolder()
  const fold = foldRef.current
  const displayItems = useMemo(() => fold(events), [fold, events])

  // Windowed rendering: only rows near the viewport mount. Rows are
  // measured (heights vary: markdown, tool blocks, diagrams) and keyed by
  // item key so measurements survive "Load older" prepends.
  const listWrapRef = useRef<HTMLDivElement>(null)
  const [listOffset, setListOffset] = useState(0)
  const rowVirtualizer = useVirtualizer({
    count: displayItems.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 64,
    overscan: 12,
    getItemKey: (i) => displayItems[i].key,
    scrollMargin: listOffset,
  })
  // The virtual list starts below the "Load older" button inside the same
  // scroll container; keep the virtualizer's origin in sync with it.
  useLayoutEffect(() => {
    const el = scrollRef.current
    const wrap = listWrapRef.current
    if (!el || !wrap) return
    setListOffset(wrap.getBoundingClientRect().top - el.getBoundingClientRect().top + el.scrollTop)
  }, [hasMoreOlderEvents, displayItems.length])
  const virtualTotal = rowVirtualizer.getTotalSize()
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
  }, [events, virtualTotal])

  const handleLoadOlder = useCallback(() => {
    const el = scrollRef.current
    if (el) {
      // Capture the current "distance from top of content" so the
      // useEffect above can restore it after the new rows render.
      pendingOlderScrollRestore.current = el.scrollHeight - el.scrollTop
    }
    void fetchOlderEvents(sessionId)
  }, [fetchOlderEvents, sessionId])

  // Queued-turn indicator. The backend broadcasts `queue` WS frames
  // (store/ws.ts re-dispatches them as `peckboard:queue` window events);
  // the durable queue is then confirmed via GET so the chip never shows
  // for a mid-turn message that was already injected into the running
  // stream (that path broadcasts `set` too, but stores nothing).
  const [queuedText, setQueuedText] = useState<string | null>(null)
  useEffect(() => {
    let cancelled = false
    setQueuedText(null)
    const refresh = () => {
      authedFetch(`/api/sessions/${sessionId}/queue`)
        .then((res) => (res.ok ? res.json() : null))
        .then((msg: { text?: string } | null) => {
          if (!cancelled) setQueuedText(typeof msg?.text === 'string' ? msg.text : null)
        })
        .catch(() => {})
    }
    refresh()
    const onQueue = (e: CustomEvent<{ session_id?: string; data?: { action?: string } }>) => {
      if (e.detail?.session_id !== sessionId) return
      if (e.detail?.data?.action === 'set') refresh()
      // `drained` / `deleted` — the message was dispatched or discarded.
      else setQueuedText(null)
    }
    window.addEventListener('peckboard:queue', onQueue as EventListener)
    return () => {
      cancelled = true
      window.removeEventListener('peckboard:queue', onQueue as EventListener)
    }
  }, [sessionId])

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
  const { agentWorking, workingSince } = (() => {
    for (let i = events.length - 1; i >= 0; i--) {
      const ev = events[i]
      if (ev.kind === 'agent-start') return { agentWorking: true, workingSince: ev.ts }
      if (ev.kind === 'agent-end') return { agentWorking: false, workingSince: 0 }
      // User sent a message but CLI hasn't started yet — still "working"
      if (ev.kind === 'user') return { agentWorking: true, workingSince: ev.ts }
    }
    return { agentWorking: false, workingSince: 0 }
  })()

  // Newest event drives stall detection: any event at all — text chunk, tool
  // start, thinking — counts as the agent reporting in. Its seq keys the
  // stall so a fresh event always clears it, even on a same-millisecond ts.
  const lastEvent = events.length > 0 ? events[events.length - 1] : null
  const stalled = useStalled(
    agentWorking,
    lastEvent?.seq ?? 0,
    Math.max(lastEvent?.ts ?? 0, workingSince),
    STALL_MS,
  )
  const agentStatus = deriveAgentStatus(events)

  // Screen-reader announcements.
  //
  // `.chat-messages` used to carry `role="log" aria-live="polite"`, but it is
  // the virtualized scroller: rows mount and unmount as the user scrolls, so
  // a screen reader re-announced old messages on scroll and missed streamed
  // text that landed outside the rendered window. Announcements now come from
  // a dedicated, always-mounted region driven by turn boundaries and status
  // changes — never by the token stream, which must not be read out
  // character by character.
  const turnHasReply = useMemo(() => {
    // Bounded backwards scan: the reply is at or near the end, and this
    // recomputes on every token chunk.
    const stop = Math.max(0, displayItems.length - 50)
    for (let i = displayItems.length - 1; i >= stop; i--) {
      if (displayItems[i].type === 'assistant') return true
    }
    return false
  }, [displayItems])
  // Read inside the status effect without making it a dependency — otherwise
  // every streamed chunk would re-fire it.
  const turnHasReplyRef = useRef(turnHasReply)
  turnHasReplyRef.current = turnHasReply

  const [announcement, setAnnouncement] = useState('')
  const prevAnnounceKeyRef = useRef<AnnounceKey | null>(null)
  useEffect(() => {
    // Session switch: forget the previous conversation, so opening a session
    // never announces the state it happened to be left in.
    prevAnnounceKeyRef.current = null
    setAnnouncement('')
  }, [sessionId])
  const currentAnnounceKey = announceKey(agentStatus)
  useEffect(() => {
    const prev = prevAnnounceKeyRef.current
    prevAnnounceKeyRef.current = currentAnnounceKey
    // The first key seen for a session is its state on open, not a change.
    if (prev === null || prev === currentAnnounceKey) return
    setAnnouncement(announcementFor(currentAnnounceKey, turnHasReplyRef.current))
  }, [currentAnnounceKey, sessionId])

  // Always show the thinking indicator while the agent is working —
  // even when text or tool blocks are streaming above it. The indicator
  // is the user's only persistent signal that the session is still busy.
  const showThinking = agentWorking

  // Live tail of the streaming thought — shown beside the working dots so
  // the user sees progress without expanding the collapsed block.
  const liveThinkingLine = useMemo(() => {
    if (!agentWorking) return ''
    const last = displayItems[displayItems.length - 1]
    if (!last || last.type !== 'thinking') return ''
    const lines = last.text.split('\n')
    for (let i = lines.length - 1; i >= 0; i--) {
      const t = lines[i].trim()
      if (t) return t
    }
    return ''
  }, [agentWorking, displayItems])

  // Toolbar actions
  const handleRename = () => setRenameOpen(true)

  const submitRename = async (newName: string) => {
    await renameSession(sessionId, newName)
    setSessionDetail((prev) => (prev ? { ...prev, name: newName } : prev))
  }

  // Opening a dialog always starts from a clean slate — a stale error
  // from the previous action must never greet the next one.
  const openConfirm = (action: ConfirmActionState) => {
    setConfirmError(null)
    setConfirmBusy(false)
    setConfirmAction(action)
  }

  // Drives the confirm dialog's primary button: success closes it,
  // failure keeps it mounted with a readable reason and re-enables the
  // buttons so the same click retries.
  const runConfirm = async () => {
    if (!confirmAction || confirmBusy) return
    setConfirmBusy(true)
    setConfirmError(null)
    try {
      await confirmAction.run()
      setConfirmAction(null)
    } catch (e) {
      setConfirmError(describeActionError(e, confirmAction.failMessage))
    } finally {
      setConfirmBusy(false)
    }
  }

  const handleClear = () => {
    openConfirm({
      title: 'Clear session',
      message: 'Clear all messages in this session?',
      confirmLabel: 'Clear',
      testId: 'confirm-clear',
      failMessage: "Couldn't clear this session. Please try again.",
      run: async () => {
        await clearSession(sessionId)
        fetchEvents(sessionId)
      },
    })
  }

  const handleTerminateAgent = () => {
    openConfirm({
      title: 'Terminate agent',
      message:
        'Terminate the agent process? Any in-flight turn will be interrupted. The next message will start a fresh process (picking up any new skills or config).',
      confirmLabel: 'Terminate',
      testId: 'confirm-terminate',
      failMessage: "Couldn't terminate the agent. Please try again.",
      run: () => terminateAgent(sessionId),
    })
  }

  const handleCompact = () => {
    openConfirm({
      title: 'Compact context',
      message:
        'Summarize the conversation and drop earlier history from the context window? The transcript stays intact.',
      confirmLabel: 'Compact',
      testId: 'confirm-compact',
      failMessage: "Couldn't compact the context. Please try again.",
      run: async () => {
        const res = await authedFetch(`/api/sessions/${sessionId}/compact`, { method: 'POST' })
        if (!res.ok) {
          const err = (await res.json().catch(() => null)) as { error?: string } | null
          throw new Error(err?.error ?? `Compaction failed (${res.status}).`)
        }
      },
    })
  }

  const handleDelete = () => {
    openConfirm({
      title: 'Delete session',
      message: 'Delete this session and all its events?',
      confirmLabel: 'Delete',
      testId: 'confirm-delete',
      failMessage: "Couldn't delete this session. Please try again.",
      run: () => deleteSession(sessionId),
    })
  }

  // Inline interrupt: the store throws on a non-2xx, which used to be an
  // unhandled rejection — the agent kept running with no sign anything
  // had failed. Surface it in the existing error banner instead.
  const handleInterrupt = async () => {
    if (interrupting) return
    setInterrupting(true)
    try {
      await interruptSession(sessionId)
      setPatchError(null)
    } catch (e) {
      setPatchError(
        describeActionError(e, `Couldn't ${interruptAffordance.label.toLowerCase()} the agent.`),
      )
    } finally {
      setInterrupting(false)
    }
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

  // Capability-driven affordances for the session's provider: whether
  // image attachments would actually reach the model, how the interrupt
  // affordance should read (soft interrupt vs kill), and whether the
  // working indicator may say "Thinking…". Providers without flags fall
  // back to today's Claude-shaped assumptions.
  const sessionModel = sessionDetail?.model
  const attachDisabledReason = imagesAllowedForModel(sessionModel, availableProviders)
    ? null
    : `${providerForModel(sessionModel, availableProviders)?.display_name ?? 'This provider'} doesn't accept image attachments — they would be dropped`
  const interruptAffordance = interruptAffordanceForModel(sessionModel, availableProviders)
  const workingLabel = modelThinks(sessionModel, availableProviders) ? 'Thinking...' : 'Working...'

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

  const autoswitchOn = sessionDetail?.model_autoswitch ?? !!sessionDetail?.is_worker
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
    {
      label: 'Plan',
      onSelect: () => planId && openPlan(planId),
      disabled: !planId,
      testId: 'chat-menu-plan',
    },
    {
      label: 'Export transcript',
      submenu: [
        {
          label: 'Markdown (.md)',
          onSelect: () =>
            void downloadTranscript(sessionId, sessionDetail?.name ?? sessionId, 'markdown'),
        },
        {
          label: 'JSON (.json)',
          onSelect: () =>
            void downloadTranscript(sessionId, sessionDetail?.name ?? sessionId, 'json'),
        },
      ],
      testId: 'chat-menu-export',
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
      // Long catalogue (Cursor alone exposes 100+ models) — filter by
      // display name or id, whose provider:model@account shape lets the
      // user narrow by account too.
      searchable: true,
      searchPlaceholder: 'Search models…',
      submenu:
        availableModels.length > 0
          ? availableModels.map((m) => ({
              label: m.display_name,
              searchText: m.id,
              active: m.id === sessionDetail?.model,
              onSelect: () => requestModelChange(m.id),
            }))
          : [{ label: 'Loading models…', disabled: true }],
    },
    {
      label: 'System prompt',
      hint: sessionDetail?.system_prompt_name || '(none)',
      searchable: true,
      searchPlaceholder: 'Search system prompts…',
      submenu: [
        {
          label: '(none)',
          active: !sessionDetail?.system_prompt_name,
          onSelect: () => patchSession({ system_prompt_name: '' }),
        },
        ...systemPrompts.map((p) => ({
          label: p.name,
          active: p.name === sessionDetail?.system_prompt_name,
          onSelect: () => patchSession({ system_prompt_name: p.name }),
        })),
      ],
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
    {
      label: 'Auto-switch model',
      hint: autoswitchOn ? 'On' : 'Off',
      active: autoswitchOn,
      onSelect: () => patchSession({ model_autoswitch: !autoswitchOn }),
      testId: 'chat-menu-autoswitch',
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
      label: 'Keep session',
      // Temp sessions delete themselves when their last tab closes;
      // keeping clears the flag. The store action also syncs the
      // sessions list row and the tab chip's temp marker.
      hidden: !sessionDetail?.is_temp,
      testId: 'chat-menu-keep',
      onSelect: () => {
        void useSessionsStore
          .getState()
          .keepSession(sessionId)
          .then(() => setSessionDetail((d) => (d ? { ...d, is_temp: false } : d)))
          .catch(() => setPatchError('Failed to keep session'))
      },
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
      {/* Conversation live region: always mounted and initially empty, so a
          screen reader is already observing it when the first announcement
          lands. Fed by the turn-boundary effect above. */}
      <div className="sr-only" role="status" aria-live="polite" data-testid="chat-live-region">
        {announcement}
      </div>

      {/* Toolbar */}
      <div className="chat-toolbar">
        {/* The session name is this view's `h1` — one per view. */}
        <h1 className="chat-toolbar-name">{sessionDetail?.name ?? 'Session'}</h1>
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
        <span className="chat-toolbar-status" data-testid="chat-toolbar-status">
          <span className={getStatusDotClass(agentStatus)} aria-hidden="true" />
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

      <div
        className="chat-messages"
        ref={scrollRef}
        onScroll={handleScroll}
        role="region"
        aria-label="Conversation"
      >
        {/* "Load older" button: shown at the top once the initial
            fetch returned a full page (more history likely exists)
            and hidden once a short page proves the user has reached
            the start of the conversation. The store debounces with
            `loadingOlderEvents` so a rapid double-click loads at most
            one extra page. */}
        {hasMoreOlderEvents && displayItems.length > 0 && (
          <div className="chat-load-older">
            {olderEventsError ? (
              <span
                className="chat-load-older-error"
                role="alert"
                data-testid="chat-load-older-error"
              >
                Couldn’t load older messages.
                <button
                  className="chat-load-older-btn"
                  data-testid="chat-load-older-retry"
                  onClick={handleLoadOlder}
                  disabled={loadingOlderEvents}
                >
                  Retry
                </button>
              </span>
            ) : (
              <button
                className="chat-load-older-btn"
                data-testid="chat-load-older"
                onClick={handleLoadOlder}
                disabled={loadingOlderEvents}
              >
                {loadingOlderEvents ? 'Loading…' : 'Load older messages'}
              </button>
            )}
          </div>
        )}
        {displayItems.length === 0 && (
          <div className="chat-empty">No messages yet. Send one below.</div>
        )}
        <div
          ref={listWrapRef}
          className="chat-virtual"
          style={{ height: virtualTotal }}
          data-testid="chat-virtual"
        >
          {rowVirtualizer.getVirtualItems().map((vi) => {
            const item = displayItems[vi.index]
            return (
              <div
                key={item.key}
                data-index={vi.index}
                ref={rowVirtualizer.measureElement}
                className="chat-vrow"
                style={{
                  transform: `translateY(${vi.start - rowVirtualizer.options.scrollMargin}px)`,
                }}
              >
                <ChatRow item={item} sessionId={sessionId} costTable={costTable} />
              </div>
            )
          })}
        </div>
        {/* Optimistic user bubbles — rendered immediately on Send so the
            chat doesn't appear to swallow the message during the WS
            round-trip (especially noticeable for queued turns). The
            matching real `user` event clears the pending entry on
            arrival; see `clearMatchingPending` in store/sessions.ts. */}
        {pendingUserMessages.map((p) => (
          <div key={p.tempId} className="chat-row chat-row-user">
            <div className="chat-bubble chat-bubble-user chat-bubble-pending">
              {p.text}
              <MessageAttachments sessionId={sessionId} attachments={p.attachments} />
              <div className="chat-time chat-time-user">
                {queuedText === p.text ? 'Queued' : 'Sending...'}
              </div>
            </div>
          </div>
        ))}
        {queuedText !== null && (
          <div className="chat-row chat-row-system">
            <div className="chat-queued-chip" data-testid="chat-queued-chip" title={queuedText}>
              <span className="chat-queued-dot" aria-hidden="true" />
              <span>Queued — sends when the agent finishes</span>
            </div>
          </div>
        )}
        {/* Thinking indicator + inline Interrupt — shown at the end of the
            message log when the agent is working. Combining them keeps the
            "stop the agent" affordance attached to the activity it's
            stopping, instead of a floating toolbar pinned above the input. */}
        {showThinking && (
          <div className="chat-row chat-row-system">
            {stalled ? (
              // Nothing has arrived for STALL_MS. Say so instead of animating
              // dots forever, and give the two moves that actually help:
              // re-fetch (a dropped WS frame) or terminate the dead process.
              <div
                className="chat-thinking chat-thinking-stalled"
                role="status"
                data-testid="chat-stall"
              >
                <span className="chat-stall-dot" aria-hidden="true" />
                <span>No response — the agent may have stopped</span>
                {workingSince > 0 && <ElapsedSince since={workingSince} />}
                <button
                  className="chat-thinking-interrupt"
                  type="button"
                  onClick={() => void fetchEvents(sessionId)}
                  title="Re-fetch this session's events in case an update was missed"
                  data-testid="chat-stall-retry"
                >
                  Retry
                </button>
                <button
                  className="chat-thinking-interrupt"
                  type="button"
                  onClick={handleTerminateAgent}
                  title="Kill the agent process; the next message starts a fresh one"
                  data-testid="chat-stall-terminate"
                >
                  Terminate
                </button>
              </div>
            ) : (
              <div className="chat-thinking">
                <div className="chat-thinking-dots">
                  <span />
                  <span />
                  <span />
                </div>
                <span>{workingLabel}</span>
                {liveThinkingLine && (
                  <span className="chat-thinking-live" title={liveThinkingLine}>
                    {liveThinkingLine}
                  </span>
                )}
                {workingSince > 0 && <ElapsedSince since={workingSince} />}
                <button
                  className="chat-thinking-interrupt"
                  onClick={() => void handleInterrupt()}
                  type="button"
                  disabled={interrupting}
                  aria-busy={interrupting || undefined}
                  aria-label={`${interruptAffordance.label} agent`}
                  title={interruptAffordance.title}
                >
                  {interrupting ? 'Stopping…' : interruptAffordance.label}
                </button>
              </div>
            )}
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
        attachDisabledReason={attachDisabledReason}
      />
      {pendingModelSwitch !== null && (
        <ConfirmDialog
          title="Switch model?"
          message={`Switching to ${modelDisplayName(pendingModelSwitch)} crosses a provider or account boundary — the new model starts with no memory of this conversation. Hand over a summary, or clear the context and switch fresh.`}
          cancelLabel="Cancel"
          secondaryAction={{
            label: 'Clear & switch',
            testId: 'model-switch-clear',
            onSelect: () => {
              const target = pendingModelSwitch
              setPendingModelSwitch(null)
              void (async () => {
                try {
                  await clearSession(sessionId)
                } catch (e) {
                  setPatchError(
                    describeActionError(e, "Couldn't clear this session. Please try again."),
                  )
                  return
                }
                patchSession({ model: target })
              })()
            },
          }}
          confirmLabel="Hand over context"
          confirmTestId="model-switch-handover"
          testId="model-switch-prompt"
          onConfirm={() => {
            const target = pendingModelSwitch
            setPendingModelSwitch(null)
            patchSession({ model: target })
          }}
          onCancel={() => setPendingModelSwitch(null)}
        />
      )}
      {confirmAction && (
        <ConfirmDialog
          title={confirmAction.title}
          message={confirmAction.message}
          confirmLabel={confirmAction.confirmLabel ?? 'Confirm'}
          cancelLabel="Cancel"
          danger
          testId={confirmAction.testId}
          error={confirmError}
          busy={confirmBusy}
          onConfirm={() => void runConfirm()}
          onCancel={() => {
            if (confirmBusy) return
            setConfirmAction(null)
            setConfirmError(null)
          }}
        />
      )}
      {renameOpen && (
        <RenameModal
          title="Rename session"
          label="Session name"
          initialValue={sessionDetail?.name ?? ''}
          onSubmit={submitRename}
          onClose={() => setRenameOpen(false)}
        />
      )}
    </div>
  )
}

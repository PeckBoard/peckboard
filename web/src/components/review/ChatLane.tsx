import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react'

import SafeMarkdown from '../SafeMarkdown'
import QuestionCard from './QuestionCard'
import {
  EMPTY_EVENTS,
  buildDisplayItems,
  deriveAgentStatus,
  formatTime,
  type DisplayItem,
} from '../chat/events'
import { chatMarkdownComponents } from '../chat/markdown'
import { runPass, type ReviewStatus } from '../../lib/review'
import { useSessionsStore } from '../../store/sessions'
import { describeActionError } from '../../utils/actionError'
import './Review.css'

interface Props {
  reviewId: string
  /** The review's AI session — null until the first pass creates it. */
  sessionId: string | null
  status: ReviewStatus
  /** Refetch the review: a sent message moves it to `running`. */
  onSent: () => void
}

/** The pass endpoint persists its instruction as the session's user turn.
 *  Rendered whole, one queued pass would bury the conversation under the
 *  annotation digest, so those turns collapse to a summary. */
const PASS_PREFIX = 'Review pass:'
const PASS_COUNT_RE = /^Review pass: (\d+) new annotation/

/** Grow the composer with its content, capped like the chat's InputBar. */
function useAutoResize(value: string) {
  const ref = useRef<HTMLTextAreaElement>(null)
  useEffect(() => {
    const ta = ref.current
    if (!ta) return
    ta.style.height = 'auto'
    const maxHeight = 20 * 8
    ta.style.overflowY = ta.scrollHeight > maxHeight ? 'auto' : 'hidden'
    ta.style.height = `${Math.min(ta.scrollHeight, maxHeight)}px`
  }, [value])
  return ref
}

/** A revision landed: the doc pane already re-rendered, so the lane only
 *  owes the reader a one-line receipt. */
function RevisionRow({ data }: { data: Record<string, unknown> }) {
  const version = typeof data.version === 'number' ? data.version : null
  const note = typeof data.note === 'string' ? data.note : ''
  return (
    <div className="review-chat__system" data-testid="review-chat-revision">
      <span className="review-chat__system-label">revised → v{version ?? '?'}</span>
      {note && <span className="review-chat__system-note">{note}</span>}
    </div>
  )
}

function PassTurn({ text, ts }: { text: string; ts: number }) {
  const count = PASS_COUNT_RE.exec(text)?.[1]
  return (
    <details className="review-chat__pass">
      <summary>
        <span className="review-chat__pass-label">
          Ran a pass{count ? ` · ${count} annotation${count === '1' ? '' : 's'}` : ''}
        </span>
        <span className="review-chat__time">{formatTime(ts)}</span>
      </summary>
      <div className="review-chat__pass-body">{text}</div>
    </details>
  )
}

/**
 * The review session's free-form lane: the place for asks too big to pin to
 * a passage ("restructure section 3", "give me three options for the intro").
 *
 * It renders the same session the annotation passes run on, folded with the
 * chat's own event folder so streamed text grows live here exactly as it does
 * in a chat. Tool traffic is dropped — a reviewer reading its own document
 * calls tools constantly, and none of it is conversation. Messages go out via
 * the pass endpoint with `include_annotations: false`, so talking here never
 * consumes the annotation queue.
 */
export default function ChatLane({ reviewId, sessionId, status, onSent }: Props) {
  const events = useSessionsStore((s) =>
    sessionId ? (s.eventsBySession[sessionId] ?? EMPTY_EVENTS) : EMPTY_EVENTS,
  )
  const loading = useSessionsStore((s) =>
    sessionId ? (s.loadingEventsBySession[sessionId] ?? true) : false,
  )

  const [text, setText] = useState('')
  const [sending, setSending] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const textareaRef = useAutoResize(text)
  const scrollRef = useRef<HTMLDivElement>(null)
  const stick = useRef(true)

  // The chat's own folder, rebuilt per change: a review session's log is a
  // conversation about one document, not a 10k-event chat, so the
  // incremental folder's bookkeeping buys nothing here.
  const items = useMemo(() => buildDisplayItems(events), [events])

  // The lane's own busy light comes from the session's events, not the
  // review's status: a chat-only turn answers without revising, and the
  // review stays `running` until something bumps the version. The agent
  // going idle is what "the reply is finished" actually means here.
  const working = useMemo(() => {
    const agent = deriveAgentStatus(events)
    return agent === 'working' || agent === 'tool'
  }, [events])

  const rows = useMemo(
    () =>
      items.filter(
        (it) =>
          it.type === 'user' ||
          it.type === 'assistant' ||
          it.type === 'question' ||
          it.type === 'question-resolved' ||
          it.type === 'agent-crashed' ||
          (it.type === 'unknown' && it.kind === 'doc-review-revision'),
      ),
    [items],
  )

  // Stay pinned to the newest row while the user is at the bottom; the
  // moment they scroll up to re-read something, stop yanking them back.
  const onScroll = useCallback(() => {
    const el = scrollRef.current
    if (!el) return
    stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 48
  }, [])
  useEffect(() => {
    if (!stick.current) return
    const el = scrollRef.current
    if (el) el.scrollTop = el.scrollHeight
  })

  const needsInput = status === 'needs_input'

  const send = () => {
    const message = text.trim()
    if (!message || sending || needsInput) return
    setSending(true)
    setError(null)
    runPass(reviewId, { message, include_annotations: false })
      .then(() => {
        setText('')
        setSending(false)
        stick.current = true
        onSent()
      })
      .catch((e: unknown) => {
        setError(describeActionError(e, "Couldn't send that message."))
        setSending(false)
      })
  }

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault()
      send()
    }
  }

  const renderRow = (item: DisplayItem) => {
    switch (item.type) {
      case 'user':
        return item.text.startsWith(PASS_PREFIX) ? (
          <PassTurn key={item.key} text={item.text} ts={item.ts} />
        ) : (
          <div key={item.key} className="review-chat__row review-chat__row--user">
            <div className="review-chat__bubble review-chat__bubble--user">
              <SafeMarkdown className="chat-markdown" components={chatMarkdownComponents}>
                {item.text}
              </SafeMarkdown>
              <span className="review-chat__time">{formatTime(item.ts)}</span>
            </div>
          </div>
        )
      case 'assistant':
        return (
          <div key={item.key} className="review-chat__row">
            <div
              className="review-chat__bubble review-chat__bubble--assistant"
              data-testid="review-chat-reply"
            >
              <SafeMarkdown className="chat-markdown" components={chatMarkdownComponents}>
                {item.text}
              </SafeMarkdown>
              <span className="review-chat__time">{formatTime(item.ts)}</span>
            </div>
          </div>
        )
      case 'question':
        return sessionId ? (
          <QuestionCard
            key={item.key}
            variant="inline"
            sessionId={sessionId}
            questionId={item.questionId}
            requestId={item.requestId}
            questions={item.questions}
          />
        ) : null
      case 'question-resolved':
        return (
          <div key={item.key} className="review-chat__system" data-testid="review-chat-answered">
            <span className="review-chat__system-label">answered</span>
            <span className="review-chat__system-note">
              {Object.values(item.answers)
                .map((a) => String(a))
                .join(' · ') || 'dismissed'}
            </span>
          </div>
        )
      case 'agent-crashed':
        return (
          <p key={item.key} className="form-error" role="alert">
            The reviewer stopped: {item.reason}
          </p>
        )
      case 'unknown':
        return <RevisionRow key={item.key} data={item.data} />
      default:
        return null
    }
  }

  return (
    <div className="review-rail__panel review-chat" data-testid="review-chat">
      <div className="review-chat__feed" ref={scrollRef} onScroll={onScroll}>
        {loading && rows.length === 0 && <div className="loading-spinner" />}
        {!loading && rows.length === 0 && (
          <p className="review-rail__empty">
            Ask for anything bigger than a single passage — restructure a section, draft three
            openings, sanity-check the argument. Annotations queued in the other tab stay put.
          </p>
        )}
        {rows.map(renderRow)}
        {(working || sending) && (
          <div className="review-chat__working" role="status">
            <span className="review-view__pass-spinner" aria-hidden="true" />
            Working…
          </div>
        )}
      </div>

      {error && (
        <p className="form-error review-chat__error" role="alert">
          {error}
        </p>
      )}

      <div className="review-chat__composer">
        <textarea
          ref={textareaRef}
          className="form-input review-chat__input"
          data-testid="review-chat-input"
          rows={1}
          placeholder={needsInput ? 'Answer the question first…' : 'Ask about this document…'}
          aria-label="Message the reviewer"
          value={text}
          disabled={sending || needsInput}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={onKeyDown}
        />
        <button
          type="button"
          className="btn-primary review-chat__send"
          data-testid="review-chat-send"
          disabled={!text.trim() || sending || needsInput}
          onClick={send}
        >
          {sending ? 'Sending…' : 'Send'}
        </button>
      </div>
      {needsInput && (
        <p className="review-chat__hint">
          The reviewer is waiting on your answer — reply to its question above, then the lane
          reopens.
        </p>
      )}
    </div>
  )
}

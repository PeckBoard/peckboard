import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from 'react'

import ConfirmDialog from '../ConfirmDialog'
import { MenuButton, type MenuItem } from '../Dropdown'
import SafeMarkdown from '../SafeMarkdown'
import { EMPTY_EVENTS, findOpenQuestion } from '../chat/events'
import AnnotationRail from './AnnotationRail'
import ChatLane from './ChatLane'
import DocPane, { type BlockAnchor } from './DocPane'
import HistoryTab from './HistoryTab'
import QuestionCard from './QuestionCard'
import ReviewSheet from './ReviewSheet'
import ReviewStatusChip from './ReviewStatusChip'
import SelectionPopover, { type PopoverAction } from './SelectionPopover'
import { useMediaQuery } from '../../hooks/useMediaQuery'
import {
  REVIEW_SOURCE_LABEL,
  ReviewRequestError,
  addComment,
  applyReview,
  deleteComment,
  deleteReview,
  describeReviewSource,
  findQuestionAnchor,
  getReview,
  runPass,
  updateComment,
  type DocReviewComment,
  type ReviewCommentKind,
  type ReviewDetail,
} from '../../lib/review'
import { useSessionsStore } from '../../store/sessions'
import { useTabsStore } from '../../store/tabs'
import { useWsStore } from '../../store/ws'
// Aliased: the DOM `Event` is the one the window listener below takes.
import type { Event as SessionEvent } from '../../types/api'
import { describeActionError } from '../../utils/actionError'
import './Review.css'

interface Props {
  /** Never null: App renders the list view instead when `/review` has no id. */
  reviewId: string
  onBack: () => void
}

type RailTab = 'annotations' | 'chat' | 'history'
type PendingConfirm = 'apply' | 'finish' | 'delete'

/** How long the version badge stays lit after a revision lands. Long enough
 *  to catch out of the corner of an eye, short enough not to become chrome.
 *  Reduced-motion collapses the animation itself (see reduced-motion.css). */
const FLASH_MS = 1_400
const NOTICE_MS = 8_000

/** What an annotation says when the user submitted the verb alone. Only the
 *  two kinds whose editor accepts an empty body need one. */
const DEFAULT_BODY: Partial<Record<ReviewCommentKind, string>> = {
  expand: 'Expand this passage.',
  shorten: 'Shorten this passage.',
}
/** The bottom bar's glyphs: a pen for the annotation queue, a bubble for
 *  the free-form lane, a clock for the version history. 16×16 inline
 *  strokes, the same idiom as the app rail's icons. */
const BAR_ICONS: Record<RailTab, ReactNode> = {
  annotations: (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path
        d="M11.2 2.3l2.5 2.5-7.4 7.4-3.2.7.7-3.2z"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinejoin="round"
      />
    </svg>
  ),
  chat: (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <path
        d="M13.5 9.5a1.5 1.5 0 0 1-1.5 1.5H6l-3 2.5V4a1.5 1.5 0 0 1 1.5-1.5h7.5A1.5 1.5 0 0 1 13.5 4z"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinejoin="round"
      />
    </svg>
  ),
  history: (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
      <circle cx="8" cy="8" r="5.6" stroke="currentColor" strokeWidth="1.4" />
      <path d="M8 4.8V8l2.2 1.6" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" />
    </svg>
  ),
}

/** What each panel is called once it is a sheet of its own. */
const SHEET_TITLE: Record<RailTab, string> = {
  annotations: 'Annotations',
  chat: 'Chat',
  history: 'History',
}

/**
 * The document review screen.
 *
 * Two panes over one review: the rendered document, where every block is
 * anchored back to its source lines and clicking one opens the six-verb
 * popover, and a rail holding the annotation queue. "Run pass" hands the
 * queue to the review session, which answers with a new version — the
 * document is never edited in place and the source file is untouched until
 * someone explicitly applies.
 */
export default function ReviewView({ reviewId, onBack }: Props) {
  const [detail, setDetail] = useState<ReviewDetail | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [actionError, setActionError] = useState<string | null>(null)
  const [notice, setNotice] = useState<string | null>(null)
  const [tab, setTab] = useState<RailTab>('annotations')
  /* One breakpoint, the app's own. Below it the rail is not rendered at
     all: the document takes the full width and the three panels open as
     bottom sheets off the action bar. */
  const isMobile = useMediaQuery('(max-width: 768px)')
  const [openSheet, setSheet] = useState<RailTab | null>(null)
  // Derived, not an effect: widening the window hands the panels straight
  // back to the rail, and a sheet left open in state can't strand itself
  // over the desktop layout.
  const sheet = isMobile ? openSheet : null
  const [activeCommentId, setActiveCommentId] = useState<string | null>(null)
  const [popover, setPopover] = useState<{
    anchor: BlockAnchor
    at: { x: number; y: number }
  } | null>(null)
  const [passBusy, setPassBusy] = useState(false)
  const [confirming, setConfirming] = useState<PendingConfirm | null>(null)
  const [confirmBusy, setConfirmBusy] = useState(false)
  /** A history version opened read-only over the document pane. */
  const [viewing, setViewing] = useState<{ version: number; markdown: string } | null>(null)
  /** The passage a clarifying question points at, once the user asks to see
   *  it: the doc pane scrolls there and lights the block up. */
  const [focusLines, setFocusLines] = useState<{ start: number; end: number } | null>(null)
  const [confirmError, setConfirmError] = useState<string | null>(null)
  const [flash, setFlash] = useState(false)

  const versionRef = useRef<number | null>(null)
  // App hands `onBack` as an inline arrow, so it changes identity on every
  // one of its renders. Threading it through `load`'s deps would re-run the
  // fetch effect forever; the ref keeps the callback current without it.
  const onBackRef = useRef(onBack)
  useEffect(() => {
    onBackRef.current = onBack
  })

  const subscribe = useWsStore((s) => s.subscribe)
  const unsubscribe = useWsStore((s) => s.unsubscribe)
  const addWsListener = useWsStore((s) => s.addEventListener)
  const removeWsListener = useWsStore((s) => s.removeEventListener)

  // Promise chain rather than `await` in the effect body: every setState has
  // to land in a callback, or the cascading-render lint rule fires.
  const load = useCallback(() => {
    // `comments: 'all'` — the rail's Resolved group is the record of what the
    // last pass actually did, so it needs the closed-out annotations too.
    getReview(reviewId, { comments: 'all' })
      .then((d) => {
        const previous = versionRef.current
        versionRef.current = d.review.current_version
        if (previous !== null && previous !== d.review.current_version) setFlash(true)
        setDetail(d)
        setError(null)
        setLoading(false)
      })
      .catch((e: unknown) => {
        // Deleted from another tab (or by the list view): there is nothing
        // to retry, so fall back to the list instead of parking on an error.
        if (e instanceof ReviewRequestError && e.status === 404) {
          onBackRef.current()
          return
        }
        setDetail(null)
        setError(describeActionError(e, "Couldn't load this review."))
        setLoading(false)
      })
  }, [reviewId])

  useEffect(() => {
    load()
  }, [load])

  // `doc-review-update` is a session-scoped frame keyed by the REVIEW id, not
  // the review session's — the server only forwards it to clients subscribed
  // to that id, so the screen subscribes to the review exactly the way a chat
  // subscribes to its own session. The payload's `review_id` is what the
  // listener keys off, because the store fans the frame out globally.
  useEffect(() => {
    subscribe(reviewId)
    const onUpdate = (e: Event) => {
      const data = (e as CustomEvent<{ data?: { review_id?: string } }>).detail?.data
      if (data?.review_id === reviewId) load()
    }
    window.addEventListener('peckboard:doc-review-update', onUpdate)
    return () => {
      window.removeEventListener('peckboard:doc-review-update', onUpdate)
      unsubscribe(reviewId)
    }
  }, [reviewId, load, subscribe, unsubscribe])

  // A revision also lands as a session event on the review session. The WS
  // frame and the session event race; whichever arrives first refetches, and
  // the second is a no-op against the same version.
  const sessionId = detail?.review.session_id ?? null
  // The screen owns the review session's event log: the chat lane reads it
  // out of the store, and the pinned question has to appear whichever rail
  // tab is open — so neither of them can be the one subscribing.
  const sessionEvents = useSessionsStore((s) =>
    sessionId ? (s.eventsBySession[sessionId] ?? EMPTY_EVENTS) : EMPTY_EVENTS,
  )
  const fetchSessionEvents = useSessionsStore((s) => s.fetchEvents)
  const appendSessionEvent = useSessionsStore((s) => s.appendEvent)

  useEffect(() => {
    if (sessionId) fetchSessionEvents(sessionId)
  }, [sessionId, fetchSessionEvents])

  useEffect(() => {
    if (!sessionId) return
    subscribe(sessionId)
    const listener = (event: SessionEvent) => {
      if (event.session_id !== sessionId) return
      appendSessionEvent(event)
      if (event.kind === 'doc-review-revision') load()
    }
    addWsListener(listener)
    return () => {
      removeWsListener(listener)
      unsubscribe(sessionId)
    }
  }, [sessionId, subscribe, unsubscribe, addWsListener, removeWsListener, load, appendSessionEvent])

  /** The reviewer's open clarifying question — pinned above the document so
   *  the passage it is about stays readable while it's answered. */
  const openQuestion = useMemo(() => findOpenQuestion(sessionEvents), [sessionEvents])

  /** Which passage that question is about. `ask_user` carries no document
   *  coordinates, so the anchor comes from the quote the reviewer is asked
   *  to include — falling back to the annotation the pass is working on. */
  const questionAnchor = useMemo(() => {
    if (!openQuestion || !detail) return null
    const text = openQuestion.questions.map((q) => q.question).join('\n')
    return findQuestionAnchor(text, detail.markdown, detail.comments)
  }, [openQuestion, detail])

  useEffect(() => {
    if (!flash) return
    const timer = setTimeout(() => setFlash(false), FLASH_MS)
    return () => clearTimeout(timer)
  }, [flash])

  useEffect(() => {
    if (!notice) return
    const timer = setTimeout(() => setNotice(null), NOTICE_MS)
    return () => clearTimeout(timer)
  }, [notice])

  const review = detail?.review ?? null
  const comments = detail?.comments ?? []
  const pendingCount = comments.filter((c) => c.status === 'pending').length
  const running = review?.status === 'running'
  const openCount = comments.filter((c) => c.status === 'pending' || c.status === 'sent').length

  const submitAnnotation = async (action: PopoverAction, body: string) => {
    if (!review || !popover) return
    const { startLine, endLine, quote } = popover.anchor
    if (action === 'clarify') {
      // Clarify never consumes the queue: it's a question about the document
      // as it stands, and the annotations are for the next deliberate pass.
      await runPass(review.id, {
        message: `Clarify (do not change the document): «${quote}»${body ? ` — ${body}` : ''}`,
        include_annotations: false,
      })
      setNotice('Asked for a clarification — the answer lands in the chat lane.')
      load()
      return
    }
    const created = await addComment(review.id, {
      start_line: startLine,
      end_line: endLine,
      quote: quote || null,
      kind: action,
      // Expand and shorten carry their whole instruction in the verb, so the
      // popover lets them submit empty. The stored annotation still has to
      // say something: the server rejects an empty body, and the injection
      // renders each one as "(kind) «quote» — body".
      body: body || DEFAULT_BODY[action] || body,
    })
    setDetail((d) => (d ? { ...d, comments: [...d.comments, created] } : d))
    setActiveCommentId(created.id)
  }

  const editAnnotation = async (comment: DocReviewComment, body: string) => {
    const updated = await updateComment(reviewId, comment.id, { body })
    setDetail((d) =>
      d ? { ...d, comments: d.comments.map((c) => (c.id === updated.id ? updated : c)) } : d,
    )
  }

  const removeAnnotation = async (comment: DocReviewComment) => {
    await deleteComment(reviewId, comment.id)
    setDetail((d) => (d ? { ...d, comments: d.comments.filter((c) => c.id !== comment.id) } : d))
  }

  const startPass = () => {
    if (!review || passBusy || running) return
    setSheet(null)
    setPassBusy(true)
    setActionError(null)
    runPass(review.id, { include_annotations: true })
      .then(() => {
        setNotice('Pass started — the reviewer is working through the queue.')
        setPassBusy(false)
        load()
      })
      .catch((e: unknown) => {
        setActionError(describeActionError(e, "Couldn't start the pass."))
        setPassBusy(false)
      })
  }

  const confirmAction = () => {
    if (!review || !confirming) return
    setConfirmBusy(true)
    setConfirmError(null)
    setActionError(null)
    if (confirming === 'delete') {
      deleteReview(review.id)
        .then(() => {
          useTabsStore.getState().removeTabsForItem('doc_review', review.id)
          setConfirmBusy(false)
          setConfirming(null)
          onBackRef.current()
        })
        .catch((e: unknown) => {
          setConfirmError(describeActionError(e, "Couldn't delete the review."))
          setConfirmBusy(false)
        })
      return
    }
    const finish = confirming === 'finish'
    applyReview(review.id, finish)
      .then(() => {
        setConfirmBusy(false)
        setConfirming(null)
        setNotice(
          finish
            ? 'Written back to the source and marked approved.'
            : 'Written back to the source.',
        )
        load()
      })
      .catch((e: unknown) => {
        // Both surfaces, deliberately: the dialog keeps the server's words
        // next to the retry button, and the banner keeps them after the
        // dialog is dismissed — an adapter refusal (file gone, path no
        // longer writable) is not something to lose on a stray Escape.
        const message = describeActionError(e, "Couldn't write the document back to its source.")
        setConfirmError(message)
        setActionError(message)
        setConfirmBusy(false)
      })
  }

  if (loading) {
    return (
      <div className="review-view review-view--loading">
        <div className="chat-loading">
          <div className="loading-spinner" />
        </div>
      </div>
    )
  }

  if (error || !review || !detail) {
    return (
      <div className="review-view review-view--error">
        <button className="review-view__back" onClick={onBack} data-testid="review-back">
          ← Back
        </button>
        <p className="form-error">{error ?? 'Review not found.'}</p>
        <button className="btn-primary" onClick={load} data-testid="review-retry">
          Try again
        </button>
      </div>
    )
  }

  /** Open a panel: the rail switches tab on desktop, a sheet opens on
   *  mobile. One entry point so the two surfaces never disagree. */
  const openPanel = (which: RailTab) => {
    setTab(which)
    setSheet(isMobile ? which : null)
  }

  /** The three panels, shared by the desktop rail and the mobile sheets so
   *  neither surface drifts from the other. */
  const renderPanel = (which: RailTab) => {
    if (which === 'annotations')
      return (
        <AnnotationRail
          comments={comments}
          activeId={activeCommentId}
          running={passBusy || running}
          // On mobile the sheet covers the passage the annotation is
          // about, so focusing one hands the screen back to the document.
          onFocus={(c) => {
            setActiveCommentId(c.id)
            setSheet(null)
          }}
          onEdit={editAnnotation}
          onDelete={removeAnnotation}
          onRunPass={startPass}
        />
      )
    if (which === 'chat')
      return (
        <ChatLane reviewId={review.id} sessionId={sessionId} status={review.status} onSent={load} />
      )
    return (
      <HistoryTab
        reviewId={review.id}
        currentVersion={review.current_version}
        viewingVersion={viewing?.version ?? null}
        onView={(v) => {
          setViewing(v)
          setSheet(null)
        }}
        onReverted={load}
      />
    )
  }
  const menuItems: MenuItem[] = [
    {
      label: 'Apply to source',
      hint: describeReviewSource(review),
      onSelect: () => setConfirming('apply'),
      testId: 'review-apply',
    },
    {
      label: 'Apply and finish',
      onSelect: () => setConfirming('finish'),
      testId: 'review-finish',
    },
    { divider: true },
    { label: 'Delete', danger: true, onSelect: () => setConfirming('delete') },
  ]

  const confirmCopy: Record<PendingConfirm, { title: string; message: string; label: string }> = {
    apply: {
      title: 'Apply to the source',
      message: `Write version ${review.current_version} over ${describeReviewSource(review)}? The document stays under review, so you can keep running passes.`,
      label: 'Apply',
    },
    finish: {
      title: 'Apply and finish',
      message: `Write version ${review.current_version} over ${describeReviewSource(review)} and mark this review approved?`,
      label: 'Apply and finish',
    },
    delete: {
      title: 'Delete review',
      message: `Delete this review and every version and annotation on it? ${describeReviewSource(review)} is left untouched.`,
      label: 'Delete',
    },
  }

  return (
    <div className="review-view" data-testid="review-view" data-review-id={review.id}>
      <header
        className={`review-view__header${
          review.status === 'approved' ? ' review-view__header--approved' : ''
        }`}
      >
        <button className="review-view__back" onClick={onBack} data-testid="review-back">
          ← Back
        </button>
        <div className="review-view__titlebar">
          <h1 className="review-view__title" data-testid="review-title">
            {review.title}
          </h1>
          <span className={`review-source review-source--${review.source_kind}`}>
            {REVIEW_SOURCE_LABEL[review.source_kind]}
          </span>
          <ReviewStatusChip status={review.status} testId="review-status" />
          <span
            className={`review-view__version${flash ? ' review-view__version--flash' : ''}`}
            data-testid="review-version"
          >
            v{review.current_version}
          </span>
        </div>

        <div className="review-view__actions">
          {/* A sentence-long notice has nowhere to go on a phone header;
              below the breakpoint it gets its own strip under it. */}
          {notice && !isMobile && (
            <span className="review-view__notice" role="status" data-testid="review-notice">
              {notice}
            </span>
          )}
          {/* On mobile Run pass lives in the bottom bar instead — one
              instance of the control, and the header keeps its title. */}
          {!isMobile && (
            <button
              type="button"
              className="btn-primary review-view__pass"
              data-testid="review-run-pass"
              disabled={passBusy || running || pendingCount === 0}
              aria-busy={passBusy || running || undefined}
              title={pendingCount === 0 ? 'Annotate the document first' : undefined}
              onClick={startPass}
            >
              {(passBusy || running) && (
                <span className="review-view__pass-spinner" aria-hidden="true" />
              )}
              {running ? 'Pass running…' : 'Run pass'}
            </button>
          )}
          <MenuButton items={menuItems} ariaLabel="Review actions" testId="review-menu" />
        </div>
      </header>
      {notice && isMobile && (
        <span
          className="review-view__notice review-view__notice--bar"
          role="status"
          data-testid="review-notice"
        >
          {notice}
        </span>
      )}

      {actionError && (
        <p className="form-error review-view__error" role="alert">
          {actionError}
        </p>
      )}

      <div className="review-view__body">
        {/* The question sits at the top of the DOCUMENT column, not above the
            whole body: pushed above both panes it shoved the rail off the
            bottom of the screen. Here the passage it is about stays
            readable and scrollable underneath, and nothing else moves. */}
        <div className="review-doc-col">
          {openQuestion && sessionId && (
            <QuestionCard
              sessionId={sessionId}
              questionId={openQuestion.questionId}
              requestId={openQuestion.requestId}
              questions={openQuestion.questions}
              anchor={questionAnchor}
              onJump={(a) => {
                setViewing(null)
                setFocusLines({ start: a.start, end: a.end })
              }}
            />
          )}
          {viewing ? (
            <div className="review-doc review-version-view" data-testid="review-version-view">
              <div className="review-version-view__banner" data-testid="review-version-banner">
                <span>
                  Viewing v{viewing.version} — current is v{review.current_version}
                </span>
                <button
                  type="button"
                  className="btn-secondary"
                  data-testid="review-version-close"
                  onClick={() => setViewing(null)}
                >
                  Back to current
                </button>
              </div>
              <SafeMarkdown className="chat-markdown review-doc__body">
                {viewing.markdown}
              </SafeMarkdown>
            </div>
          ) : (
            <DocPane
              markdown={detail.markdown}
              comments={comments}
              activeCommentId={activeCommentId}
              // The spotlight belongs to the open question; once it is
              // answered the passage goes back to reading normally.
              focusLines={openQuestion ? focusLines : null}
              onAnchor={(anchor, at) => {
                setActiveCommentId(null)
                setPopover({ anchor, at })
              }}
              onSelectComment={(id) => {
                openPanel('annotations')
                setActiveCommentId(id)
              }}
            />
          )}
        </div>
        {!isMobile && (
          <aside className="review-rail">
            <div className="review-rail__tabs" role="tablist" aria-label="Review panels">
              <button
                type="button"
                role="tab"
                aria-selected={tab === 'annotations'}
                className={`review-rail__tab${tab === 'annotations' ? ' review-rail__tab--active' : ''}`}
                data-testid="review-tab-annotations"
                onClick={() => openPanel('annotations')}
              >
                Annotations
                {openCount > 0 && <span className="review-rail__count">{openCount}</span>}
              </button>
              <button
                type="button"
                role="tab"
                aria-selected={tab === 'chat'}
                className={`review-rail__tab${tab === 'chat' ? ' review-rail__tab--active' : ''}`}
                data-testid="review-tab-chat"
                onClick={() => openPanel('chat')}
              >
                Chat
              </button>
              <button
                type="button"
                role="tab"
                aria-selected={tab === 'history'}
                className={`review-rail__tab${tab === 'history' ? ' review-rail__tab--active' : ''}`}
                data-testid="review-tab-history"
                onClick={() => openPanel('history')}
              >
                History
              </button>
            </div>

            {renderPanel(tab)}
          </aside>
        )}
      </div>

      {/* The rail's replacement below 768px: the panels behind one row of
          tap targets, with Run pass kept at hand as the primary verb. */}
      {isMobile && (
        <nav
          className="review-mobile-bar"
          data-testid="review-mobile-bar"
          aria-label="Review panels"
        >
          {(['annotations', 'chat', 'history'] as const).map((which) => (
            <button
              key={which}
              type="button"
              className={`review-mobile-bar__tab${
                sheet === which ? ' review-mobile-bar__tab--active' : ''
              }`}
              data-testid={`review-tab-${which}`}
              aria-expanded={sheet === which}
              onClick={() => openPanel(which)}
            >
              <span className="review-mobile-bar__icon">
                {BAR_ICONS[which]}
                {which === 'annotations' && openCount > 0 && (
                  <span className="review-mobile-bar__count">{openCount}</span>
                )}
              </span>
              <span className="review-mobile-bar__label">{SHEET_TITLE[which]}</span>
            </button>
          ))}
          <button
            type="button"
            className="btn-primary review-mobile-bar__pass"
            data-testid="review-run-pass"
            disabled={passBusy || running || pendingCount === 0}
            aria-busy={passBusy || running || undefined}
            title={pendingCount === 0 ? 'Annotate the document first' : undefined}
            onClick={startPass}
          >
            {(passBusy || running) && (
              <span className="review-view__pass-spinner" aria-hidden="true" />
            )}
            {running ? 'Running…' : 'Run pass'}
          </button>
        </nav>
      )}

      {isMobile && sheet && (
        <ReviewSheet
          title={SHEET_TITLE[sheet]}
          testId={`review-sheet-${sheet}`}
          onClose={() => setSheet(null)}
        >
          {renderPanel(sheet)}
        </ReviewSheet>
      )}
      {popover && (
        <SelectionPopover
          anchor={popover.at}
          quote={popover.anchor.quote}
          onClose={() => setPopover(null)}
          onSubmit={submitAnnotation}
        />
      )}

      {confirming && (
        <ConfirmDialog
          title={confirmCopy[confirming].title}
          message={confirmCopy[confirming].message}
          confirmLabel={confirmCopy[confirming].label}
          danger={confirming === 'delete'}
          busy={confirmBusy}
          error={confirmError}
          onConfirm={confirmAction}
          onCancel={() => {
            setConfirming(null)
            setConfirmError(null)
          }}
        />
      )}
    </div>
  )
}

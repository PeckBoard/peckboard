import { useEffect, useRef, useState, type ReactNode } from 'react'

import {
  REVIEW_COMMENT_KIND_LABEL,
  REVIEW_RESOLUTION_LABEL,
  isOpenComment,
  type DocReviewComment,
  type ReviewCommentKind,
} from '../../lib/review'
import { describeActionError } from '../../utils/actionError'
import './Review.css'

interface Props {
  comments: DocReviewComment[]
  /** The annotation the doc pane is currently highlighting. */
  activeId: string | null
  running: boolean
  onFocus: (comment: DocReviewComment) => void
  onEdit: (comment: DocReviewComment, body: string) => Promise<void>
  onDelete: (comment: DocReviewComment) => Promise<void>
  onRunPass: () => void
}

/** 18×18 stroke glyphs, one per annotation kind — the kind has to read at a
 *  glance in a dense rail, and five colour swatches would not survive the
 *  contrast floor. */
function KindIcon({ kind }: { kind: ReviewCommentKind }) {
  const paths: Record<ReviewCommentKind, ReactNode> = {
    comment: (
      <path d="M3 5.5A1.5 1.5 0 0 1 4.5 4h9A1.5 1.5 0 0 1 15 5.5v5A1.5 1.5 0 0 1 13.5 12H7l-3 2.5V12h-.5A1.5 1.5 0 0 1 3 10.5z" />
    ),
    suggest: (
      <>
        <path d="M11.5 3.5 14 6l-7 7H4.5V10.5z" />
        <path d="M3 15.5h12" />
      </>
    ),
    wrong: (
      <>
        <circle cx="9" cy="9" r="6.5" />
        <path d="M6.5 6.5 11.5 11.5M11.5 6.5 6.5 11.5" />
      </>
    ),
    expand: <path d="M4 7.5V4h3.5M14 10.5V14h-3.5M4 4l4.5 4.5M14 14l-4.5-4.5" />,
    shorten: <path d="M7.5 4v3.5H4M10.5 14v-3.5H14M3.5 3.5 7.5 7.5M14.5 14.5l-4-4" />,
  }
  return (
    <svg
      width="18"
      height="18"
      viewBox="0 0 18 18"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.5"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      {paths[kind]}
    </svg>
  )
}

/**
 * The annotations panel of the review screen's right rail: what's queued for
 * the next pass, and what the assistant already closed out.
 *
 * The two groups stay on one surface deliberately — a resolved annotation
 * with the note that resolved it is the record of what the last pass
 * actually did, and hiding it behind a filter makes a revision look like it
 * came from nowhere.
 */
export default function AnnotationRail({
  comments,
  activeId,
  running,
  onFocus,
  onEdit,
  onDelete,
  onRunPass,
}: Props) {
  const [editingId, setEditingId] = useState<string | null>(null)
  const [draft, setDraft] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const listRef = useRef<HTMLDivElement | null>(null)

  const open = comments.filter(isOpenComment)
  const resolved = comments.filter((c) => !isOpenComment(c))
  const pending = open.filter((c) => c.status === 'pending')

  // Scroll sync the other way: a pin clicked in the document brings its
  // annotation into view here.
  useEffect(() => {
    if (!activeId) return
    listRef.current?.querySelector(`[data-comment-id="${activeId}"]`)?.scrollIntoView({
      block: 'nearest',
    })
  }, [activeId])

  const startEdit = (c: DocReviewComment) => {
    setEditingId(c.id)
    setDraft(c.body)
    setError(null)
  }

  const saveEdit = (c: DocReviewComment) => {
    const body = draft.trim()
    if (!body || busy) return
    setBusy(true)
    onEdit(c, body)
      .then(() => {
        setEditingId(null)
        setBusy(false)
      })
      .catch((e: unknown) => {
        setError(describeActionError(e, "Couldn't save that edit."))
        setBusy(false)
      })
  }

  const remove = (c: DocReviewComment) => {
    onDelete(c).catch((e: unknown) => {
      setError(describeActionError(e, "Couldn't delete that annotation."))
    })
  }

  const renderItem = (c: DocReviewComment) => {
    const editable = isOpenComment(c)
    return (
      <li
        key={c.id}
        className={`review-annotation${c.id === activeId ? ' review-annotation--active' : ''}${
          editable ? '' : ' review-annotation--resolved'
        }`}
        data-testid="review-annotation-item"
        data-comment-id={c.id}
        data-kind={c.kind}
        data-status={c.status}
      >
        <button type="button" className="review-annotation__main" onClick={() => onFocus(c)}>
          <span className="review-annotation__kind">
            <KindIcon kind={c.kind} />
            {REVIEW_COMMENT_KIND_LABEL[c.kind]}
            <span className="review-annotation__lines">
              L{c.start_line}
              {c.end_line !== c.start_line ? `–${c.end_line}` : ''}
            </span>
          </span>
          {c.quote && <span className="review-annotation__quote">{c.quote}</span>}
          {c.body && <span className="review-annotation__body">{c.body}</span>}
        </button>

        {editingId === c.id ? (
          <div className="review-annotation__edit">
            <textarea
              className="form-input"
              data-testid="review-annotation-edit"
              rows={3}
              autoFocus
              value={draft}
              disabled={busy}
              aria-label="Edit annotation"
              onChange={(e) => setDraft(e.target.value)}
            />
            <div className="review-annotation__edit-actions">
              <button
                type="button"
                className="btn-secondary"
                disabled={busy}
                onClick={() => setEditingId(null)}
              >
                Cancel
              </button>
              <button
                type="button"
                className="btn-primary"
                disabled={busy || !draft.trim()}
                onClick={() => saveEdit(c)}
              >
                {busy ? 'Saving…' : 'Save'}
              </button>
            </div>
          </div>
        ) : (
          editable && (
            <div className="review-annotation__actions">
              <button
                type="button"
                className="review-annotation__action"
                onClick={() => startEdit(c)}
              >
                Edit
              </button>
              <button
                type="button"
                className="review-annotation__action review-annotation__action--danger"
                onClick={() => remove(c)}
              >
                Delete
              </button>
            </div>
          )
        )}

        {!editable && (
          <p className="review-annotation__resolution">
            <span className={`review-resolution review-resolution--${c.status}`}>
              {REVIEW_RESOLUTION_LABEL[c.status as 'fixed' | 'declined' | 'answered']}
            </span>
            {c.resolution_note}
          </p>
        )}
      </li>
    )
  }

  return (
    <div className="review-rail__panel" data-testid="review-annotation-rail" ref={listRef}>
      {error && (
        <p className="form-error" role="alert">
          {error}
        </p>
      )}

      {comments.length === 0 ? (
        <p className="review-rail__empty">
          Select any passage in the document to comment on it, suggest an edit, or ask for it to be
          expanded.
        </p>
      ) : (
        <>
          {open.length > 0 && (
            <section className="review-rail__group">
              <h3 className="review-rail__group-title">Pending · {open.length}</h3>
              <ul className="review-rail__list">{open.map(renderItem)}</ul>
            </section>
          )}
          {resolved.length > 0 && (
            <section className="review-rail__group">
              <h3 className="review-rail__group-title">Resolved · {resolved.length}</h3>
              <ul className="review-rail__list">{resolved.map(renderItem)}</ul>
            </section>
          )}
        </>
      )}

      {/* The rail is where the queue is read, so it's also where the queue
          gets sent — no scrolling back up to the top bar to act on it. */}
      {pending.length > 0 && (
        <div className="review-rail__cta">
          <button
            type="button"
            className="btn-primary"
            data-testid="review-run-pass-rail"
            disabled={running}
            onClick={onRunPass}
          >
            {running ? 'Pass running…' : `Run pass · ${pending.length}`}
          </button>
        </div>
      )}
    </div>
  )
}

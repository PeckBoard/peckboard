import { useCallback, useEffect, useState } from 'react'

import ReviewStatusChip from './ReviewStatusChip'
import { getReview, type ReviewDetail } from '../../lib/review'
import { describeActionError } from '../../utils/actionError'
import './Review.css'

interface Props {
  /** Never null: App renders the list view instead when `/review` has no id. */
  reviewId: string
  onBack: () => void
}

/**
 * The document review screen.
 *
 * This is the entry-surface shell: it resolves the review from the URL and
 * renders its identity (title, source, status, head version) plus the
 * current document. The annotation layer — clickable block anchors, the
 * selection popover, the annotation rail, the run-pass loop, the chat lane,
 * and the history/diff tabs — lands on top of this shell in the review-screen
 * cards; the data-testid contract those cards target is defined in the
 * feature spec, not here.
 */
export default function ReviewView({ reviewId, onBack }: Props) {
  const [detail, setDetail] = useState<ReviewDetail | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  // Promise chain rather than `await` in the effect body: every setState
  // has to land in a callback, or the cascading-render lint rule fires.
  const load = useCallback(() => {
    getReview(reviewId)
      .then((d) => {
        setDetail(d)
        setError(null)
        setLoading(false)
      })
      .catch((e: unknown) => {
        setDetail(null)
        setError(describeActionError(e, "Couldn't load this review."))
        setLoading(false)
      })
  }, [reviewId])

  useEffect(() => {
    load()
  }, [load])

  // The backend broadcasts on every mutation; the id lives in the payload
  // because the frame is keyed by review id, not session id.
  useEffect(() => {
    const onUpdate = (e: Event) => {
      const data = (e as CustomEvent<{ data?: { review_id?: string } }>).detail?.data
      if (data?.review_id === reviewId) load()
    }
    window.addEventListener('peckboard:doc-review-update', onUpdate)
    return () => window.removeEventListener('peckboard:doc-review-update', onUpdate)
  }, [reviewId, load])

  if (loading) {
    return (
      <div className="review-view review-view--loading">
        <div className="loading-spinner" />
      </div>
    )
  }

  if (error || !detail) {
    return (
      <div className="review-view review-view--error">
        <button className="review-view__back" onClick={onBack}>
          ← Back
        </button>
        <p className="form-error">{error ?? 'Review not found.'}</p>
      </div>
    )
  }

  const { review, markdown } = detail

  return (
    <div className="review-view" data-testid="review-view" data-review-id={review.id}>
      <header className="review-view__header">
        <button className="review-view__back" onClick={onBack} data-testid="review-back">
          ← Back
        </button>
        <div className="review-view__titlebar">
          <h1 className="review-view__title" data-testid="review-title">
            {review.title}
          </h1>
          <span className={`review-source review-source--${review.source_kind}`}>
            {review.source_kind}
          </span>
          <ReviewStatusChip status={review.status} testId="review-status" />
          <span className="review-view__version" data-testid="review-version">
            v{review.current_version}
          </span>
        </div>
      </header>
      <div className="review-view__doc" data-testid="review-doc">
        <pre className="review-view__source">{markdown}</pre>
      </div>
    </div>
  )
}

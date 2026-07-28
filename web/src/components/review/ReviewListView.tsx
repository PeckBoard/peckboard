import { useCallback, useEffect, useState } from 'react'

import ConfirmDialog from '../ConfirmDialog'
import List from '../List'
import ListViewHeader from '../ListViewHeader'
import type { MenuItem } from '../Dropdown'
import NewReviewWizard from './NewReviewWizard'
import ReviewStatusChip from './ReviewStatusChip'
import {
  REVIEW_SOURCE_LABEL,
  deleteReview,
  formatRelativeTime,
  listReviews,
  openReview,
  type DocReview,
} from '../../lib/review'
import { useTabsStore } from '../../store/tabs'
import { describeActionError } from '../../utils/actionError'
import './Review.css'

/**
 * Index of every document review. Rows carry the title, the source kind it
 * was created from, the lifecycle status, the head version, and when it was
 * last touched; clicking one opens the review screen at `/review/<id>` and
 * pins a cross-device tab.
 *
 * Reviews are cheap to create and never touch the source document until the
 * user applies, so the empty state leads straight into the wizard.
 */
export default function ReviewListView() {
  const [reviews, setReviews] = useState<DocReview[]>([])
  const [loaded, setLoaded] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [showWizard, setShowWizard] = useState(false)
  const [confirmDeleteId, setConfirmDeleteId] = useState<string | null>(null)

  const load = useCallback(() => {
    listReviews()
      .then((rs) => {
        setReviews(rs)
        setError(null)
        setLoaded(true)
      })
      .catch((e: unknown) => {
        setError(describeActionError(e, "Couldn't load reviews. Please try again."))
        setLoaded(true)
      })
  }, [])

  // Promise chain rather than `await` in the effect body: the lint rule
  // that guards against cascading renders flags a synchronous setState
  // reached from an effect, and every setState above lands in a callback.
  useEffect(() => {
    load()
  }, [load])

  // A pass finishing (or another device creating a review) moves rows in the
  // list, so refresh on the same broadcast the review screen listens to.
  useEffect(() => {
    const onUpdate = () => load()
    window.addEventListener('peckboard:doc-review-update', onUpdate)
    return () => window.removeEventListener('peckboard:doc-review-update', onUpdate)
  }, [load])

  const open = (review: DocReview) => {
    openReview(review.id)
    void useTabsStore.getState().openTab('doc_review', review.id)
  }

  const confirmDelete = async () => {
    const id = confirmDeleteId
    if (!id) return
    setConfirmDeleteId(null)
    try {
      await deleteReview(id)
      useTabsStore.getState().removeTabsForItem('doc_review', id)
      setReviews((rs) => rs.filter((r) => r.id !== id))
    } catch (e) {
      setError(describeActionError(e, "Couldn't delete the review. Please try again."))
    }
  }

  const buildMenu = (r: DocReview): MenuItem[] => [
    { label: 'Open', onSelect: () => open(r) },
    { divider: true },
    { label: 'Delete', danger: true, onSelect: () => setConfirmDeleteId(r.id) },
  ]

  return (
    <div className="list-view" data-testid="review-list">
      <ListViewHeader
        title="Review"
        actionLabel="+ New review"
        actionTestId="review-new"
        onAction={() => setShowWizard(true)}
      />

      {error && <p className="form-error review-list__error">{error}</p>}

      {!loaded ? (
        <div className="list-view-body">
          <div className="list-view-empty">Loading…</div>
        </div>
      ) : (
        <List<DocReview>
          items={reviews}
          getKey={(r) => r.id}
          onActivate={open}
          getMenuItems={buildMenu}
          renderItem={(r) => (
            <>
              <span className="list-view-name">{r.title}</span>
              <span className="list-view-meta">
                <span className={`review-source review-source--${r.source_kind}`}>
                  {REVIEW_SOURCE_LABEL[r.source_kind]}
                </span>
                <ReviewStatusChip status={r.status} />
                <span className="list-view-tag">v{r.current_version}</span>
                <span className="list-view-time">{formatRelativeTime(r.updated_at)}</span>
              </span>
            </>
          )}
          emptyState={
            <div className="list-view-empty">
              <p>No documents under review yet</p>
              <button className="list-view-empty-action" onClick={() => setShowWizard(true)}>
                Review a document
              </button>
            </div>
          }
        />
      )}

      {showWizard && (
        <NewReviewWizard
          onClose={() => setShowWizard(false)}
          onCreated={(review) => {
            setShowWizard(false)
            setReviews((rs) => [review, ...rs.filter((r) => r.id !== review.id)])
            open(review)
          }}
        />
      )}
      {confirmDeleteId && (
        <ConfirmDialog
          title="Delete review"
          message="Delete this review and every version and annotation on it? The source document is left untouched."
          confirmLabel="Delete"
          cancelLabel="Cancel"
          danger
          onConfirm={confirmDelete}
          onCancel={() => setConfirmDeleteId(null)}
        />
      )}
    </div>
  )
}

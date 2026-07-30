import { useCallback, useEffect, useState } from 'react'

import type { Plan } from '../types/api'
import { authedFetch } from '../store/auth'
import SafeMarkdown from './SafeMarkdown'
import { chatMarkdownComponents } from './chat/markdown'
import ConfirmDialog from './ConfirmDialog'
import FieldError from './FieldError'
import PlanImplementWizard from './PlanImplementWizard'
import ModelPicker from './ModelPicker'
import { useResourcesStore } from '../store/resources'
import { useTabsStore } from '../store/tabs'
import { createReview, listReviews, openReview } from '../lib/review'
import { describeActionError } from '../utils/actionError'
import './PlanView.css'

interface PlanViewProps {
  planId: string | null
  onBack: () => void
  /** Open the session that authored the plan (to watch it revise). */
  onOpenSession?: (sessionId: string) => void
}

/**
 * Full-page rendered view of a saved plan: read it, review it, implement it.
 *
 * Reviewing opens a `plan`-kind Document Review rather than the per-line
 * comment mode this screen used to carry. That mode was a weaker second
 * copy of the same idea — one flat line anchor per note, no versions, no
 * diff, and a "review complete" that flattened every note into a single
 * chat message. A plan is a document; it gets the document reviewer.
 */
export default function PlanView({ planId, onBack, onOpenSession }: PlanViewProps) {
  const [plan, setPlan] = useState<Plan | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [reviewError, setReviewError] = useState<string | null>(null)
  const [submitting, setSubmitting] = useState(false)
  const [showWizard, setShowWizard] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [implModel, setImplModel] = useState('')
  const models = useResourcesStore((s) => s.models)
  const fetchModels = useResourcesStore((s) => s.fetchModels)
  useEffect(() => {
    void fetchModels()
  }, [fetchModels])

  useEffect(() => {
    if (!planId) return
    let cancelled = false
    void authedFetch(`/api/plans/${planId}`)
      .then((r) => {
        if (!r.ok) throw new Error(`plan not found (${r.status})`)
        return r.json() as Promise<{ plan: Plan }>
      })
      .then((data) => {
        if (cancelled) return
        setPlan(data.plan)
        setError(null)
        setLoading(false)
      })
      .catch((e: Error) => {
        if (cancelled) return
        setError(String((e as Error).message ?? e))
        setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [planId])

  /** Review this plan as a document. Reuses the review already pointed at
   *  it rather than stacking a second one on the same plan — two reviews of
   *  one document is two answers to the same question. */
  const reviewPlan = useCallback(async () => {
    if (!planId || !plan) return
    setSubmitting(true)
    setReviewError(null)
    try {
      const existing = (await listReviews()).find(
        (r) => r.source_kind === 'plan' && r.source_ref === planId,
      )
      const review =
        existing ??
        (
          await createReview({
            source_kind: 'plan',
            source_ref: planId,
            title: plan.title,
          })
        ).review
      openReview(review.id)
      void useTabsStore.getState().openTab('doc_review', review.id)
    } catch (e) {
      setReviewError(describeActionError(e, "Couldn't open a review for this plan."))
    } finally {
      setSubmitting(false)
    }
  }, [planId, plan])

  // `plan` still holds the previous plan until the fetch for a newly
  // requested `planId` resolves — back/forward between two plans must show
  // the loader, never the old plan under the new URL.
  const stale = plan !== null && plan.id !== planId
  if (!error && (loading || stale))
    return <div className="plan-view plan-view--loading">Loading plan…</div>
  if (error || !plan)
    return (
      <div className="plan-view plan-view--error">
        <button className="btn-secondary" onClick={onBack}>
          ← Back
        </button>
        <p>Could not load plan: {error ?? 'unknown error'}</p>
      </div>
    )

  const deletePlan = async () => {
    if (!planId) return
    await authedFetch(`/api/plans/${planId}`, { method: 'DELETE' })
    onBack()
  }
  const implementDirect = async () => {
    if (!plan) return
    setSubmitting(true)
    try {
      if (implModel) {
        await authedFetch(`/api/sessions/${plan.session_id}`, {
          method: 'PATCH',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ model: implModel }),
        })
      }
      await authedFetch(`/api/sessions/${plan.session_id}/message`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          text: 'Implement the plan you proposed, step by step. Make the changes, run the tests, and report what you did. Once it aligns with the plan, ask me whether to commit and push.',
        }),
      })
      if (onOpenSession) onOpenSession(plan.session_id)
    } finally {
      setSubmitting(false)
    }
  }
  return (
    <div className="plan-view" data-testid="plan-view" data-plan-id={plan.id}>
      <header className="plan-view__header">
        <button className="plan-view__back" onClick={onBack} data-testid="plan-back">
          ← Back
        </button>
        <div className="plan-view__titlebar">
          <h1 className="plan-view__title" data-testid="plan-title">
            {plan.title}
          </h1>
          <span className={`plan-view__badge plan-view__badge--${plan.status}`}>{plan.status}</span>
          <span className="plan-view__version">v{plan.version}</span>
        </div>
      </header>
      <div className="plan-view__toolbar">
        <div className="plan-view__toolbar-group">
          <div className="form-field plan-view__impl-field">
            <label className="form-label" htmlFor="plan-impl-model">
              Implement with
            </label>
            <ModelPicker
              id="plan-impl-model"
              value={implModel}
              onChange={setImplModel}
              models={models}
              defaultLabel="Same model"
              testId="plan-impl-model"
            />
          </div>
          <button
            className="btn-primary"
            onClick={() => void implementDirect()}
            disabled={submitting}
            data-testid="plan-implement"
          >
            Implement
          </button>
          <button
            className="btn-secondary"
            onClick={() => setShowWizard(true)}
            data-testid="plan-create-cards"
          >
            Create cards…
          </button>
        </div>
        <div className="plan-view__spacer" />
        <div className="plan-view__toolbar-group">
          <button
            className="btn-secondary"
            onClick={() => void reviewPlan()}
            disabled={submitting}
            data-testid="plan-review"
          >
            Review plan
          </button>
          <button
            className="btn-danger"
            onClick={() => setConfirmDelete(true)}
            data-testid="plan-delete"
          >
            Delete
          </button>
        </div>
      </div>
      <FieldError message={reviewError ?? undefined} testId="plan-review-error" />

      <div className="plan-view__rendered" data-testid="plan-rendered">
        <SafeMarkdown components={chatMarkdownComponents}>{plan.markdown}</SafeMarkdown>
      </div>
      {showWizard && plan && (
        <PlanImplementWizard
          sessionId={plan.session_id}
          onClose={() => setShowWizard(false)}
          onSent={(sid) => {
            setShowWizard(false)
            if (onOpenSession) onOpenSession(sid)
          }}
        />
      )}
      {confirmDelete && (
        <ConfirmDialog
          title="Delete plan"
          message="Delete this plan? This cannot be undone. Any review of it keeps its own copy."
          confirmLabel="Delete"
          danger
          onConfirm={() => {
            setConfirmDelete(false)
            void deletePlan()
          }}
          onCancel={() => setConfirmDelete(false)}
        />
      )}
    </div>
  )
}

import { useEffect, useState } from 'react'
import { EMPTY_PM_DECISIONS, EMPTY_PM_QUESTIONS, usePmStore } from '../store/pmStore'
import type { PmDecision, PmPendingQuestion } from '../types/api'
import ConfirmDialog from './ConfirmDialog'

/**
 * PM expert Q&A form view.
 *
 * Deliberately NOT a chat transcript: the PM expert's window is a form.
 * The only inputs are per-question answer fields and the decision edit
 * form — no message composer, no free-form send box. Decisions only
 * change through the explicit, user-confirmed edit flow (the backend
 * supersedes the old decision rather than mutating it).
 */

interface PmExpertViewProps {
  projectId: string
  expertName: string
  onBack: () => void
}

function formatDate(dateStr: string | null): string {
  if (!dateStr) return ''
  const d = new Date(dateStr)
  return Number.isNaN(d.getTime()) ? '' : d.toLocaleString()
}

function askedByLabel(askedBySessionId: string | null): string {
  return askedBySessionId ? `worker ${askedBySessionId.slice(0, 8)}` : 'the PM expert'
}

function PendingQuestionRow({
  projectId,
  question,
}: {
  projectId: string
  question: PmPendingQuestion
}) {
  const answerQuestion = usePmStore((s) => s.answerQuestion)
  const [answer, setAnswer] = useState('')
  const [submitting, setSubmitting] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const submit = async () => {
    const trimmed = answer.trim()
    if (!trimmed || submitting) return
    setSubmitting(true)
    setError(null)
    try {
      // On success the store removes the question and this row unmounts.
      await answerQuestion(projectId, question.id, trimmed)
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to submit answer')
      setSubmitting(false)
    }
  }

  return (
    <div className="pm-card pm-pending-question" data-testid="pm-pending-question">
      <p className="pm-question-text">{question.question}</p>
      <div className="pm-card-meta">
        Asked by {askedByLabel(question.asked_by_session_id)} · {formatDate(question.asked_at)}
      </div>
      <textarea
        className="form-input pm-answer-input"
        data-testid="pm-answer-input"
        placeholder="Your answer…"
        rows={3}
        value={answer}
        onChange={(e) => setAnswer(e.target.value)}
        disabled={submitting}
      />
      {error && <div className="form-error">{error}</div>}
      <div className="pm-card-actions">
        <button
          className="btn-primary btn-sm"
          data-testid="pm-answer-submit"
          onClick={submit}
          disabled={submitting || !answer.trim()}
        >
          {submitting ? 'Answering…' : 'Answer'}
        </button>
      </div>
    </div>
  )
}

function DecisionRow({ projectId, decision }: { projectId: string; decision: PmDecision }) {
  const editDecision = usePmStore((s) => s.editDecision)
  const [editing, setEditing] = useState(false)
  const [question, setQuestion] = useState('')
  const [answer, setAnswer] = useState('')
  const [confirming, setConfirming] = useState(false)
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const openEdit = () => {
    setQuestion(decision.question)
    setAnswer(decision.answer ?? '')
    setError(null)
    setEditing(true)
  }

  const save = async () => {
    setConfirming(false)
    setSaving(true)
    setError(null)
    try {
      // The store replaces this decision with the superseding one; the
      // row unmounts when the superseded id drops out of the list.
      await editDecision(projectId, decision.id, {
        question: question.trim(),
        answer: answer.trim(),
      })
      setEditing(false)
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to edit decision')
    } finally {
      setSaving(false)
    }
  }

  return (
    <div className="pm-card pm-decision-row" data-testid="pm-decision-row">
      {!editing ? (
        <>
          <div className="pm-decision-head">
            <p className="pm-question-text">{decision.question}</p>
            <button
              className="btn-secondary btn-sm"
              data-testid="pm-decision-edit"
              onClick={openEdit}
            >
              Edit
            </button>
          </div>
          <p className="pm-decision-answer">{decision.answer}</p>
          <div className="pm-card-meta">
            Decided {formatDate(decision.decided_at)}
            {decision.asked_by_session_id
              ? ` · asked by ${askedByLabel(decision.asked_by_session_id)}`
              : ''}
          </div>
        </>
      ) : (
        <div className="pm-decision-edit-form" data-testid="pm-decision-edit-form">
          <div className="form-field">
            <label className="form-label">Question</label>
            <input
              className="form-input"
              data-testid="pm-decision-edit-question"
              value={question}
              onChange={(e) => setQuestion(e.target.value)}
              disabled={saving}
            />
          </div>
          <div className="form-field">
            <label className="form-label">Answer</label>
            <textarea
              className="form-input pm-answer-input"
              data-testid="pm-decision-edit-answer"
              rows={3}
              value={answer}
              onChange={(e) => setAnswer(e.target.value)}
              disabled={saving}
            />
          </div>
          <p className="form-hint">
            Saving records your version as the project decision and supersedes the one above.
          </p>
          {error && <div className="form-error">{error}</div>}
          <div className="pm-card-actions">
            <button
              className="btn-secondary btn-sm"
              data-testid="pm-decision-edit-cancel"
              onClick={() => setEditing(false)}
              disabled={saving}
            >
              Cancel
            </button>
            <button
              className="btn-primary btn-sm"
              data-testid="pm-decision-edit-save"
              onClick={() => setConfirming(true)}
              disabled={saving || !question.trim() || !answer.trim()}
            >
              {saving ? 'Saving…' : 'Save'}
            </button>
          </div>
        </div>
      )}
      {confirming && (
        <ConfirmDialog
          title="Change recorded decision"
          message="You are rewriting a recorded project decision as the user. The previous version will be superseded and workers will follow your new wording. Continue?"
          confirmLabel="Change decision"
          cancelLabel="Cancel"
          onConfirm={save}
          onCancel={() => setConfirming(false)}
        />
      )}
    </div>
  )
}

export default function PmExpertView({ projectId, expertName, onBack }: PmExpertViewProps) {
  const fetchPmState = usePmStore((s) => s.fetchPmState)
  const pending = usePmStore((s) => s.pendingQuestionsByProject[projectId] ?? EMPTY_PM_QUESTIONS)
  const decisions = usePmStore((s) => s.decisionsByProject[projectId] ?? EMPTY_PM_DECISIONS)
  // Refetches never clear the maps, so "fetched once" is stable and the
  // empty states don't flicker back to loading on live updates.
  const fetched = usePmStore((s) => projectId in s.decisionsByProject)

  useEffect(() => {
    fetchPmState(projectId).catch(() => {})
  }, [projectId, fetchPmState])

  return (
    <div className="list-view pm-expert-view" data-testid="pm-expert-view">
      <div className="list-view-header pm-expert-header">
        <button className="btn-secondary btn-sm" onClick={onBack} title="Back to experts">
          ← Back
        </button>
        <h2 className="list-view-title">{expertName}</h2>
        <span className="expert-kind-badge expert-kind-pm">PM</span>
      </div>
      <div className="list-view-body pm-expert-body">
        <section className="pm-section" data-testid="pm-pending-section">
          <h3 className="pm-section-title">
            Pending Questions
            {pending.length > 0 && <span className="expert-group-count">{pending.length}</span>}
          </h3>
          {pending.length === 0 ? (
            fetched ? (
              <div className="pm-empty" data-testid="pm-pending-empty">
                No questions waiting for an answer.
              </div>
            ) : (
              <div className="pm-empty">Loading…</div>
            )
          ) : (
            pending.map((q) => <PendingQuestionRow key={q.id} projectId={projectId} question={q} />)
          )}
        </section>
        <section className="pm-section" data-testid="pm-decisions-section">
          <h3 className="pm-section-title">Decisions</h3>
          {decisions.length === 0 ? (
            fetched ? (
              <div className="pm-empty" data-testid="pm-decisions-empty">
                No decisions recorded yet.
              </div>
            ) : (
              <div className="pm-empty">Loading…</div>
            )
          ) : (
            decisions.map((d) => <DecisionRow key={d.id} projectId={projectId} decision={d} />)
          )}
        </section>
      </div>
    </div>
  )
}

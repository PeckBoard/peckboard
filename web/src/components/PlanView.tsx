import { useCallback, useEffect, useState } from 'react'
import type { Components } from 'react-markdown'

import type { Plan, PlanComment } from '../types/api'
import { authedFetch } from '../store/auth'
import SafeMarkdown from './SafeMarkdown'
import MermaidBlock from './MermaidBlock'
import ConfirmDialog from './ConfirmDialog'
import PlanImplementWizard from './PlanImplementWizard'
import { useResourcesStore } from '../store/resources'
import './PlanView.css'

interface PlanViewProps {
  planId: string | null
  onBack: () => void
  /** Open the session that authored the plan (to watch it revise). */
  onOpenSession?: (sessionId: string) => void
}

// Render fenced ```mermaid blocks as diagrams; everything else stays plain.
const markdownComponents: Components = {
  code({ className, children }) {
    const text = String(children ?? '')
    if (className && /\blanguage-mermaid\b/.test(className)) {
      return <MermaidBlock code={text.replace(/\n$/, '')} />
    }
    return <code className={className}>{children}</code>
  },
}

/** Full-page rendered view of a saved plan, plus a per-line review mode
 *  where the human annotates the plan source. "Mark review complete" folds
 *  the open comments into a message posted back to the authoring session so
 *  it revises the plan with full context. */
export default function PlanView({ planId, onBack, onOpenSession }: PlanViewProps) {
  const [plan, setPlan] = useState<Plan | null>(null)
  const [comments, setComments] = useState<PlanComment[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [mode, setMode] = useState<'read' | 'review'>('read')
  const [activeLine, setActiveLine] = useState<number | null>(null)
  const [draft, setDraft] = useState('')
  const [submitting, setSubmitting] = useState(false)
  const [showWizard, setShowWizard] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [implModel, setImplModel] = useState('')
  const models = useResourcesStore((s) => s.models)
  const fetchModels = useResourcesStore((s) => s.fetchModels)
  useEffect(() => {
    void fetchModels()
  }, [fetchModels])

  const load = useCallback(async () => {
    if (!planId) return
    setLoading(true)
    try {
      const res = await authedFetch(`/api/plans/${planId}`)
      if (!res.ok) throw new Error(`plan not found (${res.status})`)
      const data = (await res.json()) as { plan: Plan; comments: PlanComment[] }
      setPlan(data.plan)
      setComments(data.comments ?? [])
      setError(null)
    } catch (e) {
      setError(String((e as Error).message ?? e))
    } finally {
      setLoading(false)
    }
  }, [planId])

  useEffect(() => {
    void load()
  }, [load])

  const addComment = useCallback(
    async (anchor: number) => {
      if (!planId || !draft.trim()) return
      setSubmitting(true)
      try {
        const res = await authedFetch(`/api/plans/${planId}/comments`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ anchor, body: draft.trim() }),
        })
        if (res.ok) {
          setDraft('')
          setActiveLine(null)
          await load()
        }
      } finally {
        setSubmitting(false)
      }
    },
    [planId, draft, load],
  )

  const deleteComment = useCallback(
    async (id: string) => {
      if (!planId) return
      await authedFetch(`/api/plans/${planId}/comments/${id}`, { method: 'DELETE' })
      await load()
    },
    [planId, load],
  )

  const reviewComplete = useCallback(async () => {
    if (!planId) return
    setSubmitting(true)
    try {
      const res = await authedFetch(`/api/plans/${planId}/review-complete`, { method: 'POST' })
      if (!res.ok) return
      const data = (await res.json()) as { session_id: string; message: string }
      // Post the synthesized revision request back to the authoring session.
      await authedFetch(`/api/sessions/${data.session_id}/message`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ text: data.message }),
      })
      if (onOpenSession) onOpenSession(data.session_id)
      else await load()
    } finally {
      setSubmitting(false)
    }
  }, [planId, onOpenSession, load])

  if (loading) return <div className="plan-view plan-view--loading">Loading plan…</div>
  if (error || !plan)
    return (
      <div className="plan-view plan-view--error">
        <button className="btn" onClick={onBack}>
          ← Back
        </button>
        <p>Could not load plan: {error ?? 'unknown error'}</p>
      </div>
    )

  const lines = plan.markdown.split('\n')
  const commentsByLine = new Map<number, PlanComment[]>()
  for (const c of comments) {
    const arr = commentsByLine.get(c.anchor) ?? []
    arr.push(c)
    commentsByLine.set(c.anchor, arr)
  }

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
    <div className="plan-view" data-testid="plan-view">
      <header className="plan-view__header">
        <button className="btn" onClick={onBack} data-testid="plan-back">
          ← Back
        </button>
        <h1 className="plan-view__title" data-testid="plan-title">
          {plan.title}
        </h1>
        <span className="plan-view__meta">
          v{plan.version} · {plan.status}
        </span>
        <div className="plan-view__spacer" />
        <div className="plan-view__tabs">
          <button
            className={`btn ${mode === 'read' ? 'btn--active' : ''}`}
            onClick={() => setMode('read')}
            data-testid="plan-tab-read"
          >
            Rendered
          </button>
          <button
            className={`btn ${mode === 'review' ? 'btn--active' : ''}`}
            onClick={() => setMode('review')}
            data-testid="plan-tab-review"
          >
            Review ({comments.length})
          </button>
        </div>
        {comments.length > 0 && (
          <button
            className="btn btn--primary"
            onClick={reviewComplete}
            disabled={submitting}
            data-testid="plan-review-complete"
          >
            Mark review complete
          </button>
        )}
      </header>
      <select
        className="plan-view__impl-model"
        value={implModel}
        onChange={(e) => setImplModel(e.target.value)}
        data-testid="plan-impl-model"
      >
        <option value="">Same model</option>
        {models.map((m) => (
          <option key={m.id} value={m.id}>
            {m.display_name}
          </option>
        ))}
      </select>
      <button
        className="btn"
        onClick={() => void implementDirect()}
        disabled={submitting}
        data-testid="plan-implement"
      >
        Implement
      </button>
      <button className="btn" onClick={() => setShowWizard(true)} data-testid="plan-create-cards">
        Create cards…
      </button>
      <button
        className="btn danger"
        onClick={() => setConfirmDelete(true)}
        data-testid="plan-delete"
      >
        Delete
      </button>

      {mode === 'read' ? (
        <div className="plan-view__rendered" data-testid="plan-rendered">
          <SafeMarkdown components={markdownComponents}>{plan.markdown}</SafeMarkdown>
        </div>
      ) : (
        <div className="plan-view__source" data-testid="plan-source">
          {lines.map((line, i) => {
            const anchor = i + 1
            const lineComments = commentsByLine.get(anchor) ?? []
            return (
              <div className="plan-line" key={anchor}>
                <div className="plan-line__row">
                  <span className="plan-line__num">{anchor}</span>
                  <code className="plan-line__text">{line || ' '}</code>
                  <button
                    className="plan-line__add"
                    onClick={() => {
                      setActiveLine(activeLine === anchor ? null : anchor)
                      setDraft('')
                    }}
                    data-testid={`plan-comment-add-${anchor}`}
                    title="Comment on this line"
                  >
                    ＋
                  </button>
                </div>
                {lineComments.map((c) => (
                  <div className="plan-line__comment" key={c.id} data-testid="plan-comment">
                    <span>{c.body}</span>
                    <button
                      className="plan-line__comment-del"
                      onClick={() => void deleteComment(c.id)}
                      title="Delete comment"
                    >
                      ✕
                    </button>
                  </div>
                ))}
                {activeLine === anchor && (
                  <div className="plan-line__editor">
                    <textarea
                      value={draft}
                      onChange={(e) => setDraft(e.target.value)}
                      placeholder={`Comment on line ${anchor}…`}
                      data-testid="plan-comment-input"
                      rows={2}
                    />
                    <button
                      className="btn btn--primary"
                      disabled={submitting || !draft.trim()}
                      onClick={() => void addComment(anchor)}
                      data-testid="plan-comment-save"
                    >
                      Add comment
                    </button>
                  </div>
                )}
              </div>
            )
          })}
        </div>
      )}
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
          message="Delete this plan and all its review comments? This cannot be undone."
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

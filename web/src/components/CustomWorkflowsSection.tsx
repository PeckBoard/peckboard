import { useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'
import ConfirmDialog from './ConfirmDialog'

interface WorkflowStepInfo {
  step: string
  instructions: string
}

interface WorkflowInfo {
  id: string
  name: string
  description: string
  priority: number
  source: 'builtin' | 'custom'
  steps: WorkflowStepInfo[]
}

type Draft = {
  id: string | null
  name: string
  description: string
  steps: WorkflowStepInfo[]
}

const BLANK_DRAFT: Draft = {
  id: null,
  name: '',
  description: '',
  steps: [
    { step: 'backlog', instructions: '' },
    { step: 'in_progress', instructions: '' },
    { step: 'done', instructions: '' },
  ],
}

function toDraft(w: WorkflowInfo): Draft {
  return {
    id: w.id,
    name: w.name,
    description: w.description,
    steps: w.steps.map((s) => ({ ...s })),
  }
}

/**
 * Settings → Workflows. Lists built-in workflows (read-only) alongside
 * user-defined ones, and provides a structured steps editor for creating
 * and editing custom workflows. Every non-terminal step needs instructions
 * — a step with none never runs a worker (see `workflow::validate_workflow_steps`
 * on the backend, which enforces the same rule).
 */
export default function CustomWorkflowsSection() {
  const [workflows, setWorkflows] = useState<WorkflowInfo[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState('')
  const [draft, setDraft] = useState<Draft | null>(null)
  const [saving, setSaving] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState<WorkflowInfo | null>(null)
  const [deleteError, setDeleteError] = useState<string | null>(null)
  const [deleting, setDeleting] = useState(false)

  const load = () => {
    setLoading(true)
    authedFetch('/api/workflows')
      .then((res) => (res.ok ? res.json() : Promise.reject(new Error('Failed to load'))))
      .then((data: { workflows: WorkflowInfo[] }) => setWorkflows(data.workflows ?? []))
      .catch((e) => setError(e instanceof Error ? e.message : 'Failed to load'))
      .finally(() => setLoading(false))
  }

  useEffect(() => {
    let cancelled = false
    authedFetch('/api/workflows')
      .then((res) => (res.ok ? res.json() : Promise.reject(new Error('Failed to load'))))
      .then((data: { workflows: WorkflowInfo[] }) => {
        if (!cancelled) {
          setWorkflows(data.workflows ?? [])
          setLoading(false)
        }
      })
      .catch((e) => {
        if (!cancelled) {
          setError(e instanceof Error ? e.message : 'Failed to load')
          setLoading(false)
        }
      })
    return () => {
      cancelled = true
    }
  }, [])

  const updateStep = (i: number, patch: Partial<WorkflowStepInfo>) => {
    if (!draft) return
    const steps = draft.steps.map((s, idx) => (idx === i ? { ...s, ...patch } : s))
    setDraft({ ...draft, steps })
  }

  const addStep = () => {
    if (!draft) return
    const steps = [...draft.steps]
    // Insert before the terminal `done` step; name must be unique and
    // slug-shaped (the backend rejects anything else).
    let n = 1
    while (steps.some((s) => s.step === `step_${n}`)) n++
    steps.splice(steps.length - 1, 0, { step: `step_${n}`, instructions: '' })
    setDraft({ ...draft, steps })
  }

  const removeStep = (i: number) => {
    if (!draft) return
    setDraft({ ...draft, steps: draft.steps.filter((_, idx) => idx !== i) })
  }

  const save = async () => {
    if (!draft) return
    if (!draft.name.trim()) {
      setError('Name is required')
      return
    }
    setError('')
    setSaving(true)
    try {
      const body = {
        name: draft.name.trim(),
        description: draft.description,
        steps: draft.steps.map((s) => ({ step: s.step.trim(), instructions: s.instructions })),
      }
      const res = draft.id
        ? await authedFetch(`/api/workflows/${draft.id}`, {
            method: 'PUT',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(body),
          })
        : await authedFetch('/api/workflows', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(body),
          })
      if (!res.ok) {
        const err = await res.json().catch(() => ({ error: 'Failed to save' }))
        throw new Error(err.error || 'Failed to save')
      }
      setDraft(null)
      load()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to save')
    } finally {
      setSaving(false)
    }
  }

  const remove = async (w: WorkflowInfo) => {
    setDeleteError(null)
    setDeleting(true)
    try {
      const res = await authedFetch(`/api/workflows/${w.id}`, { method: 'DELETE' })
      if (!res.ok) {
        const err = await res.json().catch(() => ({ error: 'Failed to delete' }))
        throw new Error(err.error || 'Failed to delete')
      }
      setConfirmDelete(null)
      load()
    } catch (e) {
      setDeleteError(e instanceof Error ? e.message : 'Failed to delete')
    } finally {
      setDeleting(false)
    }
  }

  return (
    <section className="settings-section" data-testid="custom-workflows-section">
      <h3>Workflows</h3>
      <p className="form-hint" style={{ marginTop: 0 }}>
        Built-in workflows are read-only. Define your own step sequence for projects and cards that
        need something different.
      </p>

      {error && <p className="form-error">{error}</p>}

      {loading ? (
        <p className="settings-loading">Loading workflows...</p>
      ) : (
        <ul className="custom-workflows-list" data-testid="custom-workflows-list">
          {workflows.map((w) => (
            <li key={w.id} className="custom-workflow-row" data-testid={`workflow-row-${w.id}`}>
              <div className="custom-workflow-meta">
                <span className="custom-workflow-name">{w.name}</span>
                <span className={`custom-workflow-source custom-workflow-source-${w.source}`}>
                  {w.source === 'custom' ? 'custom' : 'built-in'}
                </span>
                <span className="custom-workflow-desc">{w.description}</span>
              </div>
              {w.source === 'custom' && (
                <div className="custom-workflow-actions">
                  <button
                    type="button"
                    className="btn-secondary"
                    onClick={() => setDraft(toDraft(w))}
                    data-testid={`workflow-edit-${w.id}`}
                  >
                    Edit
                  </button>
                  <button
                    type="button"
                    className="btn-secondary"
                    onClick={() => {
                      setDeleteError(null)
                      setConfirmDelete(w)
                    }}
                    data-testid={`workflow-delete-${w.id}`}
                  >
                    Delete
                  </button>
                </div>
              )}
            </li>
          ))}
        </ul>
      )}

      {draft ? (
        <div className="form-inline-card" data-testid="workflow-editor">
          <input
            className="form-input"
            placeholder="Workflow name"
            value={draft.name}
            onChange={(e) => setDraft({ ...draft, name: e.target.value })}
            data-testid="workflow-name-input"
          />
          <input
            className="form-input"
            placeholder="Description"
            value={draft.description}
            onChange={(e) => setDraft({ ...draft, description: e.target.value })}
            data-testid="workflow-description-input"
          />
          <div className="workflow-steps-editor">
            {draft.steps.map((s, i) => {
              const terminal = i === 0 || i === draft.steps.length - 1
              return (
                <div className="workflow-step-row" key={i}>
                  <input
                    className="form-input workflow-step-name"
                    value={s.step}
                    disabled={terminal}
                    onChange={(e) => updateStep(i, { step: e.target.value })}
                    data-testid={`workflow-step-name-${i}`}
                  />
                  <textarea
                    className="form-input workflow-step-instructions"
                    placeholder={
                      terminal ? 'terminal step — no instructions' : 'Step instructions...'
                    }
                    rows={2}
                    disabled={terminal}
                    value={s.instructions}
                    onChange={(e) => updateStep(i, { instructions: e.target.value })}
                    data-testid={`workflow-step-instructions-${i}`}
                  />
                  {!terminal && (
                    <button
                      type="button"
                      className="btn-secondary"
                      onClick={() => removeStep(i)}
                      disabled={draft.steps.length <= 3}
                      data-testid={`workflow-step-remove-${i}`}
                    >
                      Remove
                    </button>
                  )}
                </div>
              )
            })}
            <button
              type="button"
              className="btn-secondary"
              onClick={addStep}
              data-testid="workflow-step-add"
            >
              + Add step
            </button>
          </div>
          <div className="form-actions">
            <button type="button" className="btn-secondary" onClick={() => setDraft(null)}>
              Cancel
            </button>
            <button
              type="button"
              className="btn-primary"
              onClick={save}
              disabled={saving}
              data-testid="workflow-save"
            >
              {saving ? 'Saving…' : 'Save'}
            </button>
          </div>
        </div>
      ) : (
        <button
          type="button"
          className="btn-secondary"
          onClick={() =>
            setDraft({ ...BLANK_DRAFT, steps: BLANK_DRAFT.steps.map((s) => ({ ...s })) })
          }
          data-testid="workflow-new"
        >
          + New workflow
        </button>
      )}

      {confirmDelete && (
        <ConfirmDialog
          testId="workflow-delete-confirm"
          danger
          title={`Delete "${confirmDelete.name}"?`}
          message={`The "${confirmDelete.name}" workflow is deleted for good. This fails with an error if any project or card still uses it.`}
          confirmLabel="Delete workflow"
          error={deleteError}
          busy={deleting}
          busyLabel="Deleting…"
          onConfirm={() => void remove(confirmDelete)}
          onCancel={() => {
            setConfirmDelete(null)
            setDeleteError(null)
          }}
        />
      )}
    </section>
  )
}

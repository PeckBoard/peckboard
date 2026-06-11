import { useEffect, useMemo, useState } from 'react'
import { authedFetch } from '../store/auth'
import { useResourcesStore, type WorkflowInfo, type WorkflowStepInfo } from '../store/resources'
import Modal from './Modal'

/** All overrides for a project, keyed by workflow id then by step. */
export type WorkflowInstructionsDraft = Record<string, Record<string, string>>

type Props =
  | {
      mode?: 'project'
      /** Existing project to edit overrides for. */
      projectId: string
      /** Workflow to show first when the modal opens. The picker inside the
       *  modal lets the user switch to any other workflow. Defaults to the
       *  first workflow returned by the registry when omitted. */
      initialWorkflowId?: string
      onClose: () => void
    }
  | {
      mode: 'draft'
      /** No project exists yet; bring up the editor for the soon-to-be project. */
      initialWorkflowId?: string
      drafts: WorkflowInstructionsDraft
      onCommit: (drafts: WorkflowInstructionsDraft) => void
      onClose: () => void
    }

interface OverrideRow {
  workflow_id: string
  step: string
  instructions: string
}

/**
 * Per-workflow, per-step "additional instructions" editor.
 *
 * Cards in a project can each pick their own workflow, so the editor lets
 * you customize ANY workflow's prompts — not just the project's default
 * card workflow. The user picks a workflow at the top; the per-step
 * editor below shows that workflow's worker-running steps. Workflows
 * that already have overrides get a "customized" badge in the picker so
 * the user can find existing work without clicking through each.
 *
 * Each step that runs a worker (i.e. has built-in instructions) gets a card:
 * the built-in prompt shown read-only, a big `+`, and a textarea for the
 * project's additional instructions.
 *
 * Two modes:
 *
 * - `project` (default): backed by the live API; each step's Save button
 *   PUTs to `/api/projects/:id/workflow-instructions`. Empty input clears
 *   the override on the server.
 *
 * - `draft`: used by NewProjectModal before the project exists. Edits
 *   stay in memory; `onCommit` is called when the user clicks Save so
 *   the parent can hand the staged drafts back to the create-project
 *   flow, which upserts each entry after the project is created.
 */
export default function WorkflowInstructionsModal(props: Props) {
  const mode = props.mode ?? 'project'
  const { onClose } = props
  const projectId = mode === 'project' ? (props as { projectId: string }).projectId : null
  const workflows = useResourcesStore((s) => s.workflows)
  const fetchWorkflows = useResourcesStore((s) => s.fetchWorkflows)

  // Whole-project overrides keyed by (workflowId, step). For "project" mode
  // this mirrors what the server has stored; for "draft" mode it mirrors
  // what the parent passed in.
  const initialMap: WorkflowInstructionsDraft =
    mode === 'draft' ? (props as { drafts: WorkflowInstructionsDraft }).drafts : {}
  const [overrides, setOverrides] = useState<WorkflowInstructionsDraft>(initialMap)
  const [drafts, setDrafts] = useState<WorkflowInstructionsDraft>(initialMap)
  // For draft mode, no network load is needed.
  const [loading, setLoading] = useState(mode === 'project')
  const [savingKey, setSavingKey] = useState<string | null>(null)
  const [error, setError] = useState('')
  const [savedKey, setSavedKey] = useState<string | null>(null)
  // The picker tracks an EXPLICIT user choice; when none has been made
  // (or the chosen id isn't in the loaded workflow list yet), we derive
  // the effective selection from `sortedWorkflows` below. This avoids a
  // `setState` inside an effect when the workflow list resolves — the
  // initial selection just falls through to the first available id.
  const [explicitSelection, setExplicitSelection] = useState<string | null>(
    props.initialWorkflowId ?? null,
  )

  // Sort workflows the same way WorkflowSelect does — by priority, then name —
  // so the picker order is stable across the app.
  const sortedWorkflows = useMemo(
    () =>
      [...workflows].sort((a, b) => {
        if (a.priority !== b.priority) return a.priority - b.priority
        return a.name.localeCompare(b.name)
      }),
    [workflows],
  )

  useEffect(() => {
    fetchWorkflows()
  }, [fetchWorkflows])

  // Effective selection: prefer the explicit pick when it matches a
  // loaded workflow, else fall back to the first one. Derived so the
  // workflow list loading later doesn't have to trigger a setState.
  const selectedWorkflowId: string =
    explicitSelection && sortedWorkflows.some((w) => w.id === explicitSelection)
      ? explicitSelection
      : (sortedWorkflows[0]?.id ?? '')

  useEffect(() => {
    // Draft mode has nothing to load — the parent's drafts are the
    // source of truth and got picked up via initial state.
    if (mode !== 'project' || !projectId) return
    let cancelled = false
    authedFetch(`/api/projects/${projectId}/workflow-instructions`)
      .then((res) => (res.ok ? res.json() : Promise.reject(new Error('failed to load'))))
      .then((data) => {
        if (cancelled) return
        const rows: OverrideRow[] = data?.instructions ?? []
        const map: WorkflowInstructionsDraft = {}
        for (const r of rows) {
          if (!map[r.workflow_id]) map[r.workflow_id] = {}
          map[r.workflow_id][r.step] = r.instructions
        }
        setOverrides(map)
        setDrafts(map)
        setError('')
      })
      .catch((e: unknown) => {
        if (!cancelled) setError(e instanceof Error ? e.message : 'Failed to load instructions')
      })
      .finally(() => {
        if (!cancelled) setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [mode, projectId])

  const selectedWorkflow: WorkflowInfo | undefined = useMemo(
    () => sortedWorkflows.find((w) => w.id === selectedWorkflowId),
    [sortedWorkflows, selectedWorkflowId],
  )

  const hasOverridesFor = (workflowId: string): boolean => {
    const wf = overrides[workflowId]
    if (!wf) return false
    return Object.values(wf).some((v) => v.trim() !== '')
  }

  const handleSave = async (step: string) => {
    if (!selectedWorkflowId) return
    const draftValue = drafts[selectedWorkflowId]?.[step] ?? ''
    const stepKey = `${selectedWorkflowId}/${step}`
    setSavingKey(stepKey)
    setError('')
    setSavedKey(null)
    const trimmed = draftValue.trim()
    // Returns the next `overrides` map with this single (workflow, step)
    // entry applied, so we can both update local state and bubble the
    // same value up to the parent in draft mode.
    const computeNext = (prev: WorkflowInstructionsDraft): WorkflowInstructionsDraft => {
      const next: WorkflowInstructionsDraft = { ...prev }
      const wfMap = { ...(next[selectedWorkflowId] ?? {}) }
      if (trimmed === '') {
        delete wfMap[step]
      } else {
        wfMap[step] = trimmed
      }
      if (Object.keys(wfMap).length === 0) delete next[selectedWorkflowId]
      else next[selectedWorkflowId] = wfMap
      return next
    }
    const commitLocal = () => {
      setOverrides(computeNext)
      setDrafts((prev) => {
        const next = { ...prev }
        next[selectedWorkflowId] = { ...(next[selectedWorkflowId] ?? {}), [step]: trimmed }
        return next
      })
      setSavedKey(stepKey)
    }
    try {
      if (mode === 'project' && projectId) {
        const res = await authedFetch(`/api/projects/${projectId}/workflow-instructions`, {
          method: 'PUT',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            workflow_id: selectedWorkflowId,
            step,
            instructions: draftValue,
          }),
        })
        if (!res.ok) {
          const body = (await res.json().catch(() => ({}))) as { error?: string }
          throw new Error(body.error ?? `HTTP ${res.status}`)
        }
        commitLocal()
      } else {
        // Draft mode: stage locally and bubble the full per-workflow
        // staged map up to the parent so it survives the modal close.
        commitLocal()
        const next = computeNext(overrides)
        ;(props as { onCommit: (d: WorkflowInstructionsDraft) => void }).onCommit(next)
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to save')
    } finally {
      setSavingKey(null)
    }
  }

  // Only steps with built-in instructions actually run a worker. Terminal
  // steps (`backlog`, `done`) have empty instructions and don't render a
  // card — overrides there would never reach a worker anyway.
  const editableSteps: WorkflowStepInfo[] = selectedWorkflow
    ? selectedWorkflow.steps
        .map((s): WorkflowStepInfo => (typeof s === 'string' ? { step: s, instructions: '' } : s))
        .filter((s) => s.instructions.trim() !== '')
    : []

  return (
    <Modal onClose={onClose} className="workflow-instructions-modal" maxWidth={720}>
      <h2>Workflow Instructions</h2>
      <p className="form-hint" style={{ marginTop: 0, marginBottom: 16 }}>
        Cards in this project can each use a different workflow, so customize whichever ones you
        need. Your text is appended below the built-in instructions a worker receives — both apply.
        Use it for things every card on that workflow should do, like "commit to master and push
        when finished."
      </p>

      <div className="form-field">
        <label className="form-label" htmlFor="workflow-instructions-picker">
          Workflow
        </label>
        <select
          id="workflow-instructions-picker"
          className="form-input"
          value={selectedWorkflowId}
          onChange={(e) => {
            setExplicitSelection(e.target.value)
            setSavedKey(null)
          }}
          disabled={sortedWorkflows.length === 0}
        >
          {sortedWorkflows.map((w) => (
            <option key={w.id} value={w.id}>
              {w.name}
              {hasOverridesFor(w.id) ? ' • customized' : ''}
            </option>
          ))}
        </select>
        {selectedWorkflow?.description && (
          <p className="form-hint">{selectedWorkflow.description}</p>
        )}
      </div>

      {loading && <p className="form-hint">Loading current instructions…</p>}

      {!loading &&
        selectedWorkflow &&
        editableSteps.map((s) => {
          const draft = drafts[selectedWorkflowId]?.[s.step] ?? ''
          const saved = overrides[selectedWorkflowId]?.[s.step] ?? ''
          const dirty = draft.trim() !== saved.trim()
          const stepKey = `${selectedWorkflowId}/${s.step}`
          const saving = savingKey === stepKey
          return (
            <section key={s.step} className="workflow-step-block">
              <header className="workflow-step-header">
                <h3 className="workflow-step-title">{prettyStepName(s.step)}</h3>
                {saved && <span className="workflow-step-badge">customized</span>}
              </header>

              <div className="workflow-step-builtin" aria-label="Built-in instructions">
                <div className="workflow-step-builtin-label">Built-in instructions</div>
                <pre className="workflow-step-builtin-body">{s.instructions}</pre>
              </div>

              <div className="workflow-step-plus" aria-hidden="true">
                +
              </div>

              <div className="workflow-step-extra">
                <label className="form-label" htmlFor={`extra-${selectedWorkflowId}-${s.step}`}>
                  Your additional instructions
                </label>
                <textarea
                  id={`extra-${selectedWorkflowId}-${s.step}`}
                  className="form-input"
                  value={draft}
                  onChange={(e) => {
                    const v = e.target.value
                    setDrafts((prev) => {
                      const next = { ...prev }
                      next[selectedWorkflowId] = {
                        ...(next[selectedWorkflowId] ?? {}),
                        [s.step]: v,
                      }
                      return next
                    })
                    if (savedKey === stepKey) setSavedKey(null)
                  }}
                  placeholder="e.g. At the end, commit to master and push."
                  rows={4}
                  style={{ resize: 'vertical' }}
                />
                <div className="workflow-step-actions">
                  <button
                    type="button"
                    className="btn-primary"
                    disabled={saving || !dirty}
                    onClick={() => handleSave(s.step)}
                  >
                    {saving ? 'Saving…' : 'Save'}
                  </button>
                  {savedKey === stepKey && !dirty && (
                    <span className="form-hint workflow-step-saved">Saved.</span>
                  )}
                </div>
              </div>
            </section>
          )
        })}

      {!loading && selectedWorkflow && editableSteps.length === 0 && (
        <p className="form-hint">This workflow has no worker steps to customize.</p>
      )}

      {error && <p className="form-error">{error}</p>}

      <div className="form-actions">
        <button type="button" className="btn-secondary" onClick={onClose}>
          Close
        </button>
      </div>
    </Modal>
  )
}

/**
 * Make `in_progress` → `In progress`, leaving already-friendly names alone.
 */
function prettyStepName(step: string): string {
  const spaced = step.replace(/[_-]+/g, ' ')
  return spaced.charAt(0).toUpperCase() + spaced.slice(1)
}

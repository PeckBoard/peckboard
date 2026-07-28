import { useEffect, useMemo, useState, type FormEvent } from 'react'
import { useProjectsStore } from '../store/projects'
import { authedFetch } from '../store/auth'
import { effortOptionsForModel, type ProviderInfo } from '../store/resources'
import type { Project } from '../types/api'
import Modal from './Modal'
import ModelPicker from './ModelPicker'
import WorkflowSelect from './WorkflowSelect'
import WorkflowInstructionsModal from './WorkflowInstructionsModal'
import FieldError from './FieldError'

interface Props {
  project: Project
  onClose: () => void
}

interface ModelInfo {
  id: string
  display_name: string
}

export default function EditProjectModal({ project, onClose }: Props) {
  const updateProject = useProjectsStore((s) => s.updateProject)

  const [name, setName] = useState(project.name)
  const [context, setContext] = useState(project.context)
  const [workerCount, setWorkerCount] = useState(project.worker_count)
  const [workflow, setWorkflow] = useState(project.workflow)
  const [model, setModel] = useState(project.model ?? '')
  const [effort, setEffort] = useState(project.effort ?? '')
  const [parallelInstructions, setParallelInstructions] = useState(project.parallel_instructions)
  const [workerCommunication, setWorkerCommunication] = useState(project.worker_communication)
  const [autoNotifyChanges, setAutoNotifyChanges] = useState(project.auto_notify_changes)
  const [worktreeIsolation, setWorktreeIsolation] = useState(project.worktree_isolation)
  const [budgetDollars, setBudgetDollars] = useState(
    project.budget_usd_cents != null ? String(project.budget_usd_cents / 100) : '',
  )
  const [budgetPeriod, setBudgetPeriod] = useState<string>(project.budget_period ?? '')
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState('')
  const [showInstructions, setShowInstructions] = useState(false)

  const [models, setModels] = useState<ModelInfo[]>([])
  const [providers, setProviders] = useState<ProviderInfo[]>([])

  useEffect(() => {
    authedFetch('/api/models')
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        if (data?.models) setModels(data.models)
        if (data?.providers) setProviders(data.providers)
      })
      .catch(() => {})
  }, [])

  // Effort options follow the chosen model's provider.
  const effortOptions = useMemo(() => effortOptionsForModel(model, providers), [model, providers])
  // Clear a now-invalid effort back to Default on model change so we never
  // save one the provider can't use.
  const handleModelChange = (id: string) => {
    setModel(id)
    const opts = effortOptionsForModel(id, providers)
    if (providers.length > 0 && effort && !opts.some((o) => o.value === effort)) setEffort('')
  }

  // Why Save is disabled, shown next to the button — a disabled control
  // with no stated reason leaves the user hunting for the bad field.
  const workerCountProblem =
    Number.isInteger(workerCount) && workerCount >= 1 && workerCount <= 10
      ? ''
      : 'Worker count must be between 1 and 10'
  const disabledReason = !name.trim()
    ? 'Enter a name'
    : !workflow
      ? 'Pick a workflow'
      : workerCountProblem

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault()
    if (!name.trim()) {
      setError('Name is required')
      return
    }
    if (!workflow) {
      setError('Workflow is required')
      return
    }
    if (workerCountProblem) {
      setError(workerCountProblem)
      return
    }
    setLoading(true)
    setError('')
    try {
      await updateProject(project.id, {
        name: name.trim(),
        context: context.trim(),
        worker_count: workerCount,
        workflow,
        model: model || null,
        effort: effort || null,
        parallel_instructions: parallelInstructions,
        auto_notify_changes: autoNotifyChanges,
        worker_communication: workerCommunication,
        budget_usd_cents:
          budgetDollars && budgetPeriod ? Math.round(parseFloat(budgetDollars) * 100) : null,
        budget_period: budgetPeriod || null,
        worktree_isolation: worktreeIsolation,
      } as Partial<Project>)
      onClose()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to update project')
    } finally {
      setLoading(false)
    }
  }

  return (
    <>
      <Modal onClose={onClose} maxWidth={520}>
        <h2>Edit Project</h2>
        <form onSubmit={handleSubmit}>
          <div className="form-field">
            <label className="form-label" htmlFor="edit-project-name">
              Name
            </label>
            <input
              id="edit-project-name"
              className="form-input"
              value={name}
              onChange={(e) => setName(e.target.value)}
              required
            />
          </div>
          <div className="form-field">
            <label className="form-label" htmlFor="edit-project-context">
              Context
            </label>
            <textarea
              id="edit-project-context"
              className="form-input"
              value={context}
              onChange={(e) => setContext(e.target.value)}
              placeholder="Project context for workers..."
              rows={3}
              style={{ resize: 'vertical' }}
            />
          </div>
          <div className="form-field">
            <label className="form-label" htmlFor="edit-project-worker-count">
              Worker count
            </label>
            <input
              id="edit-project-worker-count"
              className="form-input"
              type="number"
              min={1}
              max={10}
              value={Number.isFinite(workerCount) ? workerCount : ''}
              aria-invalid={workerCountProblem ? true : undefined}
              onChange={(e) => setWorkerCount(parseInt(e.target.value, 10))}
            />
            <FieldError message={workerCountProblem} testId="edit-project-worker-count-error" />
          </div>
          <div className="form-field">
            <label className="form-label" htmlFor="edit-project-workflow">
              Default card workflow
            </label>
            <WorkflowSelect id="edit-project-workflow" value={workflow} onChange={setWorkflow} />
            <p className="form-hint">
              Cards default to this workflow when created here. Each card can still override it
              individually.
            </p>
            <div className="form-workflow-extras">
              <button
                type="button"
                className="btn-secondary btn-small"
                onClick={() => setShowInstructions(true)}
              >
                Edit workflow instructions…
              </button>
              <p className="form-hint">
                Add extra instructions every card runs at a given column — e.g. "commit to master
                and push when done." Customize any workflow your project uses, not just the default.
                Your text is appended to the built-in step prompts, not replacing them.
              </p>
            </div>
          </div>
          <div className="form-field">
            <label className="form-label" htmlFor="edit-project-model">
              Model
            </label>
            <ModelPicker
              id="edit-project-model"
              value={model}
              onChange={handleModelChange}
              models={models}
              testId="edit-project-model"
            />
          </div>
          <div className="form-field">
            <label className="form-label" htmlFor="edit-project-effort">
              Effort
            </label>
            <select
              id="edit-project-effort"
              className="form-input"
              value={effort}
              onChange={(e) => setEffort(e.target.value)}
            >
              {effortOptions.map((o) => (
                <option key={o.value} value={o.value}>
                  {o.label}
                </option>
              ))}
            </select>
          </div>
          <div className="form-field">
            <label className="form-checkbox-label">
              <input
                type="checkbox"
                checked={parallelInstructions}
                onChange={(e) => setParallelInstructions(e.target.checked)}
              />
              <span>Parallel-workflow instructions</span>
            </label>
          </div>
          <div className="form-field">
            <label className="form-checkbox-label">
              <input
                type="checkbox"
                checked={autoNotifyChanges}
                onChange={(e) => setAutoNotifyChanges(e.target.checked)}
              />
              <span>Auto-notify file changes</span>
            </label>
            <div className="form-field">
              <label className="form-label" htmlFor="edit-project-budget">
                Spend budget
              </label>
              <div style={{ display: 'flex', gap: '8px', alignItems: 'center' }}>
                <input
                  id="edit-project-budget"
                  className="form-input"
                  type="number"
                  min={0}
                  step={0.01}
                  placeholder="No limit"
                  value={budgetDollars}
                  onChange={(e) => setBudgetDollars(e.target.value)}
                  style={{ width: '120px' }}
                />
                <select
                  aria-label="Budget period"
                  className="form-input"
                  value={budgetPeriod}
                  onChange={(e) => setBudgetPeriod(e.target.value)}
                  style={{ flex: 1 }}
                >
                  <option value="">No budget</option>
                  <option value="daily">Daily</option>
                  <option value="weekly">Weekly</option>
                  <option value="monthly">Monthly</option>
                </select>
              </div>
              <p className="form-hint">
                Auto-pauses the project when spend in the current window exceeds this amount.
              </p>
            </div>
            <p className="form-hint">
              Automatically notify other workers when files are modified. Prevents merge conflicts.
            </p>
          </div>
          <div className="form-field">
            <label className="form-checkbox-label">
              <input
                type="checkbox"
                checked={workerCommunication}
                onChange={(e) => setWorkerCommunication(e.target.checked)}
              />
              <span>Inter-worker communication</span>
            </label>
            <p className="form-hint">
              Allow workers to share findings and send messages to each other.
            </p>
          </div>
          {error && <p className="form-error">{error}</p>}
          <div className="form-field">
            <label className="form-checkbox-label">
              <input
                type="checkbox"
                checked={worktreeIsolation}
                onChange={(e) => setWorktreeIsolation(e.target.checked)}
              />
              <span>Worktree isolation</span>
            </label>
            <p className="form-hint">
              Give each card its own git worktree so parallel workers cannot race each other&apos;s
              uncommitted state. Requires a git repository.
            </p>
          </div>
          <div className="form-actions">
            {!loading && disabledReason && (
              <span className="form-actions-reason" data-testid="edit-project-disabled-reason">
                {disabledReason}
              </span>
            )}
            <button type="button" className="btn-secondary" onClick={onClose}>
              Cancel
            </button>
            <button type="submit" className="btn-primary" disabled={loading || !!disabledReason}>
              {loading ? 'Saving...' : 'Save'}
            </button>
          </div>
        </form>
      </Modal>
      {showInstructions && (
        <WorkflowInstructionsModal
          projectId={project.id}
          initialWorkflowId={workflow || undefined}
          onClose={() => setShowInstructions(false)}
        />
      )}
    </>
  )
}

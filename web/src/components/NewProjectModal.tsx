import { useEffect, useMemo, useState, type FormEvent } from 'react'
import { useProjectsStore } from '../store/projects'
import { useFoldersStore } from '../store/folders'
import { effortOptionsForModel, useResourcesStore } from '../store/resources'
import { authedFetch } from '../store/auth'
import Modal from './Modal'
import ModelPicker from './ModelPicker'
import WorkflowSelect from './WorkflowSelect'
import WorkflowInstructionsModal, {
  type WorkflowInstructionsDraft,
} from './WorkflowInstructionsModal'
import FolderManager from './ManageFoldersModal'
import FieldError from './FieldError'

interface Props {
  onClose: () => void
}

export default function NewProjectModal({ onClose }: Props) {
  const createProject = useProjectsStore((s) => s.createProject)
  const setActiveProject = useProjectsStore((s) => s.setActiveProject)
  const folders = useFoldersStore((s) => s.folders)
  const fetchFolders = useFoldersStore((s) => s.fetchFolders)
  const models = useResourcesStore((s) => s.models)
  const providers = useResourcesStore((s) => s.providers)
  const fetchWorkflows = useResourcesStore((s) => s.fetchWorkflows)
  const fetchModels = useResourcesStore((s) => s.fetchModels)

  const [name, setName] = useState('')
  // `chosenFolderId` is what the user explicitly picked; until they pick,
  // we fall back to the first available folder. Derived in render so the
  // default updates the moment folders load — no effect needed.
  const [chosenFolderId, setChosenFolderId] = useState<string | null>(null)
  const folderId = chosenFolderId ?? folders[0]?.id ?? ''
  const [context, setContext] = useState('')
  const [workerCount, setWorkerCount] = useState(1)
  const [workflow, setWorkflow] = useState('')
  const [model, setModel] = useState('')
  const [effort, setEffort] = useState('')
  const [parallelInstructions, setParallelInstructions] = useState(false)
  const [autoNotifyChanges, setAutoNotifyChanges] = useState(false)
  const [workerCommunication, setWorkerCommunication] = useState(false)
  const [worktreeIsolation, setWorktreeIsolation] = useState(false)
  const [budgetDollars, setBudgetDollars] = useState('')
  const [budgetPeriod, setBudgetPeriod] = useState<'' | 'daily' | 'weekly' | 'monthly'>('')
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(false)
  // Per-workflow staged drafts: { workflowId: { step: text } }. Survives
  // the workflow picker so the user doesn't lose work when switching
  // between workflows inside the instructions modal.
  const [instructionDrafts, setInstructionDrafts] = useState<WorkflowInstructionsDraft>({})
  const [showInstructions, setShowInstructions] = useState(false)
  const [showAdvanced, setShowAdvanced] = useState(false)
  // Folder manager stacked on top of this modal, so a first-run user with
  // no folders can create one without losing the form they started.
  const [showFolders, setShowFolders] = useState(false)

  useEffect(() => {
    fetchFolders()
  }, [fetchFolders])

  useEffect(() => {
    fetchWorkflows()
    fetchModels()
  }, [fetchWorkflows, fetchModels])

  // Effort options follow the chosen model's provider.
  const effortOptions = useMemo(() => effortOptionsForModel(model, providers), [model, providers])
  // When the model changes to a provider that doesn't offer the current
  // effort, clear it back to Default so we never submit an effort the
  // provider can't use.
  const handleModelChange = (id: string) => {
    setModel(id)
    const opts = effortOptionsForModel(id, providers)
    if (providers.length > 0 && effort && !opts.some((o) => o.value === effort)) setEffort('')
  }

  const workerCountProblem =
    Number.isInteger(workerCount) && workerCount >= 1 && workerCount <= 10
      ? ''
      : 'Worker count must be between 1 and 10'

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault()
    if (!name.trim() || !folderId) {
      setError('Name and folder are required')
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
      const project = await createProject({
        name: name.trim(),
        folder_id: folderId,
        context: context.trim(),
        worker_count: workerCount,
        workflow,
        model: model || undefined,
        effort: effort || undefined,
        parallel_instructions: parallelInstructions,
        auto_notify_changes: autoNotifyChanges,
        worker_communication: workerCommunication,
        worktree_isolation: worktreeIsolation,
        budget_usd_cents:
          budgetDollars && budgetPeriod ? Math.round(parseFloat(budgetDollars) * 100) : undefined,
        budget_period: budgetPeriod || undefined,
      })
      // After the project exists, persist any staged per-step
      // instructions the user added across ANY workflow they touched.
      // Project creation already succeeded — collect each failure so we
      // can surface a single summary instead of silently dropping the
      // user's drafts.
      type Failure = { workflowId: string; step: string }
      const upserts: Promise<Failure | null>[] = []
      for (const [workflowId, perStep] of Object.entries(instructionDrafts)) {
        for (const [step, instructions] of Object.entries(perStep)) {
          upserts.push(
            authedFetch(`/api/projects/${project.id}/workflow-instructions`, {
              method: 'PUT',
              headers: { 'Content-Type': 'application/json' },
              body: JSON.stringify({ workflow_id: workflowId, step, instructions }),
            })
              .then((res) => (res.ok ? null : ({ workflowId, step } satisfies Failure)))
              .catch(() => ({ workflowId, step }) satisfies Failure),
          )
        }
      }
      const failures = (upserts.length > 0 ? await Promise.all(upserts) : []).filter(
        (f): f is Failure => f !== null,
      )
      setActiveProject(project.id)
      if (failures.length > 0) {
        // Keep the modal open with a clear summary so the user knows
        // their drafts didn't all land. The project itself exists and is
        // reachable from the kanban list once they dismiss this dialog.
        const detail = failures.map((f) => `${f.workflowId}/${f.step}`).join(', ')
        setError(
          `Project created, but ${failures.length} workflow-instruction(s) failed to save (${detail}). Open Edit Project to retry.`,
        )
        return
      }
      onClose()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to create project')
    } finally {
      setLoading(false)
    }
  }

  // Why Create is disabled, shown next to the button — a disabled control
  // with no stated reason leaves the user hunting for the bad field.
  const disabledReason = !name.trim()
    ? 'Enter a name'
    : !folderId
      ? 'Add a folder first'
      : !workflow
        ? 'Pick a workflow'
        : workerCountProblem

  return (
    <>
      <Modal onClose={onClose} maxWidth={520}>
        <h2>New Project</h2>
        <form onSubmit={handleSubmit}>
          <div className="form-field">
            <label className="form-label" htmlFor="new-project-name">
              Name
            </label>
            <input
              id="new-project-name"
              className="form-input"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="My project"
              autoFocus
              required
            />
          </div>
          <div className="form-field">
            <label className="form-label" htmlFor="new-project-folder">
              Folder
            </label>
            {folders.length > 0 ? (
              <select
                id="new-project-folder"
                className="form-input"
                value={folderId}
                onChange={(e) => setChosenFolderId(e.target.value)}
              >
                {folders.map((f) => (
                  <option key={f.id} value={f.id}>
                    {f.name} — {f.path}
                  </option>
                ))}
              </select>
            ) : (
              <>
                <button
                  type="button"
                  className="btn-secondary btn-small"
                  data-testid="new-project-add-folder"
                  onClick={() => setShowFolders(true)}
                >
                  Add a folder…
                </button>
                <p className="form-hint">
                  A project lives inside a folder on disk. Add one here — you keep your place in
                  this form.
                </p>
              </>
            )}
          </div>
          <div className="form-field">
            <label className="form-label" htmlFor="new-project-workflow">
              Default card workflow
            </label>
            <WorkflowSelect id="new-project-workflow" value={workflow} onChange={setWorkflow} />
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
            <label className="form-label" htmlFor="new-project-context">
              Context <span className="optional">(optional)</span>
            </label>
            <textarea
              id="new-project-context"
              className="form-input"
              value={context}
              onChange={(e) => setContext(e.target.value)}
              placeholder="High-level background and instructions for workers on this project..."
              rows={3}
              style={{ resize: 'vertical' }}
            />
          </div>

          <button
            type="button"
            className="form-toggle-advanced"
            onClick={() => setShowAdvanced(!showAdvanced)}
          >
            {showAdvanced ? 'Hide' : 'Show'} advanced settings
          </button>

          {showAdvanced && (
            <div className="form-advanced-section">
              <div className="form-field">
                <label className="form-label" htmlFor="new-project-worker-count">
                  Worker count
                </label>
                <input
                  id="new-project-worker-count"
                  className="form-input"
                  type="number"
                  min={1}
                  max={10}
                  value={Number.isFinite(workerCount) ? workerCount : ''}
                  aria-invalid={workerCountProblem ? true : undefined}
                  onChange={(e) => setWorkerCount(parseInt(e.target.value, 10))}
                />
                <FieldError message={workerCountProblem} testId="new-project-worker-count-error" />
                <p className="form-hint">
                  Number of parallel workers. Keep at 1 unless the repo is set up for parallel work
                  (git worktrees).
                </p>
              </div>
              <div className="form-field">
                <label className="form-label" htmlFor="new-project-budget">
                  Spend budget
                </label>
                <div style={{ display: 'flex', gap: '8px', alignItems: 'center' }}>
                  <input
                    id="new-project-budget"
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
                    onChange={(e) =>
                      setBudgetPeriod(e.target.value as '' | 'daily' | 'weekly' | 'monthly')
                    }
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
              <div className="form-field">
                <label className="form-label" htmlFor="new-project-model">
                  Model
                </label>
                <ModelPicker
                  id="new-project-model"
                  value={model}
                  onChange={handleModelChange}
                  models={models}
                  testId="new-project-model"
                />
                <p className="form-hint">
                  Project-level model override. Cards and workflow steps can further override this.
                </p>
              </div>
              <div className="form-field">
                <label className="form-label" htmlFor="new-project-effort">
                  Effort
                </label>
                <select
                  id="new-project-effort"
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
                <p className="form-hint">
                  Controls reasoning budget. Higher effort = slower but more thorough.
                </p>
              </div>
              <div className="form-field">
                <label className="form-checkbox-label">
                  <input
                    type="checkbox"
                    checked={parallelInstructions}
                    onChange={(e) => setParallelInstructions(e.target.checked)}
                  />
                  <span>Inject parallel-workflow instructions</span>
                </label>
                <p className="form-hint">
                  Appends guidance on git worktrees, dependency isolation, and test isolation to
                  worker prompts. Enable when running multiple workers.
                </p>
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
                <p className="form-hint">
                  Automatically notify other workers when files are modified. Prevents merge
                  conflicts.
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
                  Give each card its own git worktree so parallel workers cannot race each
                  other&apos;s uncommitted state. Requires a git repository.
                </p>
              </div>
            </div>
          )}

          {error && <p className="form-error">{error}</p>}
          <div className="form-actions">
            {!loading && disabledReason && (
              <span className="form-actions-reason" data-testid="new-project-disabled-reason">
                {disabledReason}
              </span>
            )}
            <button type="button" className="btn-secondary" onClick={onClose}>
              Cancel
            </button>
            <button type="submit" className="btn-primary" disabled={loading || !!disabledReason}>
              {loading ? 'Creating...' : 'Create Project'}
            </button>
          </div>
        </form>
      </Modal>
      {showInstructions && (
        <WorkflowInstructionsModal
          mode="draft"
          initialWorkflowId={workflow || undefined}
          drafts={instructionDrafts}
          onCommit={(next) => setInstructionDrafts(next)}
          onClose={() => setShowInstructions(false)}
        />
      )}
      {showFolders && (
        <Modal onClose={() => setShowFolders(false)} maxWidth={560}>
          <FolderManager />
          <div className="form-actions">
            <button
              type="button"
              className="btn-primary"
              data-testid="new-project-folders-done"
              onClick={() => setShowFolders(false)}
            >
              Done
            </button>
          </div>
        </Modal>
      )}
    </>
  )
}

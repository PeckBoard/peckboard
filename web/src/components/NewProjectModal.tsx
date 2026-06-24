import { useEffect, useState, type FormEvent } from 'react'
import { useProjectsStore } from '../store/projects'
import { useFoldersStore } from '../store/folders'
import { useResourcesStore } from '../store/resources'
import { authedFetch } from '../store/auth'
import Modal from './Modal'
import ModelPicker from './ModelPicker'
import WorkflowSelect from './WorkflowSelect'
import WorkflowInstructionsModal, {
  type WorkflowInstructionsDraft,
} from './WorkflowInstructionsModal'

interface Props {
  onClose: () => void
}

const EFFORT_OPTIONS = [
  { value: '', label: 'Default' },
  { value: 'low', label: 'Low' },
  { value: 'medium', label: 'Medium' },
  { value: 'high', label: 'High' },
  { value: 'xhigh', label: 'Extra high' },
  { value: 'max', label: 'Max' },
]

export default function NewProjectModal({ onClose }: Props) {
  const createProject = useProjectsStore((s) => s.createProject)
  const setActiveProject = useProjectsStore((s) => s.setActiveProject)
  const folders = useFoldersStore((s) => s.folders)
  const fetchFolders = useFoldersStore((s) => s.fetchFolders)
  const models = useResourcesStore((s) => s.models)
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
  const [autoNotifyChanges, setAutoNotifyChanges] = useState(true)
  const [workerCommunication, setWorkerCommunication] = useState(true)
  const [showAdvanced, setShowAdvanced] = useState(false)
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(false)
  // Per-workflow staged drafts: { workflowId: { step: text } }. Survives
  // the workflow picker so the user doesn't lose work when switching
  // between workflows inside the instructions modal.
  const [instructionDrafts, setInstructionDrafts] = useState<WorkflowInstructionsDraft>({})
  const [showInstructions, setShowInstructions] = useState(false)

  useEffect(() => {
    fetchFolders()
  }, [fetchFolders])

  useEffect(() => {
    fetchWorkflows()
    fetchModels()
  }, [fetchWorkflows, fetchModels])

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
      } as Record<string, unknown>)
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

  return (
    <>
      <Modal onClose={onClose} maxWidth={520}>
        <h2>New Project</h2>
        <form onSubmit={handleSubmit}>
          <div className="form-field">
            <label className="form-label">Name</label>
            <input
              className="form-input"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="My project"
              autoFocus
              required
            />
          </div>
          <div className="form-field">
            <label className="form-label">Folder</label>
            {folders.length > 0 ? (
              <select
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
              <p style={{ fontSize: 'var(--text-sm)', color: 'var(--text3)' }}>
                No folders. Create one from the folder manager first.
              </p>
            )}
          </div>
          <div className="form-field">
            <label className="form-label">Default card workflow</label>
            <WorkflowSelect value={workflow} onChange={setWorkflow} />
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
            <label className="form-label">
              Context <span className="optional">(optional)</span>
            </label>
            <textarea
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
                <label className="form-label">Worker count</label>
                <input
                  className="form-input"
                  type="number"
                  min={1}
                  max={10}
                  value={workerCount}
                  onChange={(e) => setWorkerCount(Number(e.target.value))}
                />
                <p className="form-hint">
                  Number of parallel workers. Keep at 1 unless the repo is set up for parallel work
                  (git worktrees).
                </p>
              </div>
              <div className="form-field">
                <label className="form-label">Model</label>
                <ModelPicker
                  value={model}
                  onChange={setModel}
                  models={models}
                  testId="new-project-model"
                />
                <p className="form-hint">
                  Project-level model override. Cards and workflow steps can further override this.
                </p>
              </div>
              <div className="form-field">
                <label className="form-label">Effort</label>
                <select
                  className="form-input"
                  value={effort}
                  onChange={(e) => setEffort(e.target.value)}
                >
                  {EFFORT_OPTIONS.map((o) => (
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
            </div>
          )}

          {error && <p className="form-error">{error}</p>}
          <div className="form-actions">
            <button type="button" className="btn-secondary" onClick={onClose}>
              Cancel
            </button>
            <button
              type="submit"
              className="btn-primary"
              disabled={loading || !name.trim() || !folderId || !workflow}
            >
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
    </>
  )
}

import { useEffect, useMemo, useState, type FormEvent } from 'react'
import { useProjectsStore } from '../store/projects'
import { authedFetch } from '../store/auth'
import { effortOptionsForModel, type ProviderInfo } from '../store/resources'
import type { Project } from '../types/api'
import Modal from './Modal'
import ModelPicker from './ModelPicker'
import WorkflowSelect from './WorkflowSelect'
import WorkflowInstructionsModal from './WorkflowInstructionsModal'

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
  const [autoNotifyChanges, setAutoNotifyChanges] = useState(project.auto_notify_changes)
  const [workerCommunication, setWorkerCommunication] = useState(project.worker_communication)
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(false)
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
            <label className="form-label">Name</label>
            <input
              className="form-input"
              value={name}
              onChange={(e) => setName(e.target.value)}
              required
            />
          </div>
          <div className="form-field">
            <label className="form-label">Context</label>
            <textarea
              className="form-input"
              value={context}
              onChange={(e) => setContext(e.target.value)}
              placeholder="Project context for workers..."
              rows={3}
              style={{ resize: 'vertical' }}
            />
          </div>
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
            <label className="form-label">Model</label>
            <ModelPicker
              value={model}
              onChange={handleModelChange}
              models={models}
              testId="edit-project-model"
            />
          </div>
          <div className="form-field">
            <label className="form-label">Effort</label>
            <select
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
          <div className="form-actions">
            <button type="button" className="btn-secondary" onClick={onClose}>
              Cancel
            </button>
            <button
              type="submit"
              className="btn-primary"
              disabled={loading || !name.trim() || !workflow}
            >
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

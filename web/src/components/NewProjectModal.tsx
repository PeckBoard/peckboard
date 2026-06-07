import { useEffect, useState, type FormEvent } from 'react'
import { useProjectsStore } from '../store/projects'
import { useFoldersStore } from '../store/folders'
import { authedFetch } from '../store/auth'

interface Props {
  onClose: () => void
}

interface WorkflowInfo {
  id: string
  name: string
  steps: string[]
}

interface ModelInfo {
  id: string
  display_name: string
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

  const [name, setName] = useState('')
  const [folderId, setFolderId] = useState('')
  const [context, setContext] = useState('')
  const [workerCount, setWorkerCount] = useState(1)
  const [defaultWorkflow, setDefaultWorkflow] = useState('')
  const [model, setModel] = useState('')
  const [effort, setEffort] = useState('')
  const [parallelInstructions, setParallelInstructions] = useState(false)
  const [showAdvanced, setShowAdvanced] = useState(false)
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(false)

  const [workflows, setWorkflows] = useState<WorkflowInfo[]>([])
  const [models, setModels] = useState<ModelInfo[]>([])

  useEffect(() => {
    fetchFolders()
  }, [fetchFolders])
  useEffect(() => {
    if (folders.length > 0 && !folderId) setFolderId(folders[0].id)
  }, [folders, folderId])

  // Fetch workflows and models on mount
  useEffect(() => {
    authedFetch('/api/workflows')
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        if (data?.workflows) setWorkflows(data.workflows)
      })
      .catch(() => {})

    authedFetch('/api/models')
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        if (data?.models) setModels(data.models)
      })
      .catch(() => {})
  }, [])

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault()
    if (!name.trim() || !folderId) {
      setError('Name and folder are required')
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
        default_workflow: defaultWorkflow || undefined,
        model: model || undefined,
        effort: effort || undefined,
        parallel_instructions: parallelInstructions,
      } as Record<string, unknown>)
      setActiveProject(project.id)
      onClose()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to create project')
    } finally {
      setLoading(false)
    }
  }

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()} style={{ maxWidth: 520 }}>
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
                onChange={(e) => setFolderId(e.target.value)}
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
                <label className="form-label">Default workflow</label>
                <select
                  className="form-input"
                  value={defaultWorkflow}
                  onChange={(e) => setDefaultWorkflow(e.target.value)}
                >
                  <option value="">None (use default)</option>
                  {workflows.map((w) => (
                    <option key={w.id} value={w.id}>
                      {w.name}
                    </option>
                  ))}
                </select>
                <p className="form-hint">Pre-selected workflow when creating new cards.</p>
              </div>
              <div className="form-field">
                <label className="form-label">Model</label>
                <select
                  className="form-input"
                  value={model}
                  onChange={(e) => setModel(e.target.value)}
                >
                  <option value="">Default</option>
                  {models.map((m) => (
                    <option key={m.id} value={m.id}>
                      {m.display_name}
                    </option>
                  ))}
                </select>
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
              disabled={loading || !name.trim() || !folderId}
            >
              {loading ? 'Creating...' : 'Create Project'}
            </button>
          </div>
        </form>
      </div>
    </div>
  )
}

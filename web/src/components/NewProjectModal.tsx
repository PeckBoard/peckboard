import { useEffect, useState, type FormEvent } from 'react'
import { useProjectsStore } from '../store/projects'
import { useFoldersStore } from '../store/folders'

interface Props {
  onClose: () => void
}

export default function NewProjectModal({ onClose }: Props) {
  const createProject = useProjectsStore((s) => s.createProject)
  const setActiveProject = useProjectsStore((s) => s.setActiveProject)
  const folders = useFoldersStore((s) => s.folders)
  const fetchFolders = useFoldersStore((s) => s.fetchFolders)

  const [name, setName] = useState('')
  const [folderId, setFolderId] = useState('')
  const [context, setContext] = useState('')
  const [workerCount, setWorkerCount] = useState(1)
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(false)

  useEffect(() => { fetchFolders() }, [fetchFolders])
  useEffect(() => {
    if (folders.length > 0 && !folderId) setFolderId(folders[0].id)
  }, [folders, folderId])

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
      <div className="modal" onClick={(e) => e.stopPropagation()} style={{ maxWidth: 480 }}>
        <h2>New Project</h2>
        <form onSubmit={handleSubmit}>
          <div className="form-field">
            <label className="form-label">Name</label>
            <input className="form-input" value={name} onChange={(e) => setName(e.target.value)} placeholder="My project" autoFocus required />
          </div>
          <div className="form-field">
            <label className="form-label">Folder</label>
            {folders.length > 0 ? (
              <select className="form-input" value={folderId} onChange={(e) => setFolderId(e.target.value)}>
                {folders.map((f) => <option key={f.id} value={f.id}>{f.name} — {f.path}</option>)}
              </select>
            ) : (
              <p style={{ fontSize: 'var(--text-sm)', color: 'var(--text3)' }}>No folders. Create one from the folder manager first.</p>
            )}
          </div>
          <div className="form-field">
            <label className="form-label">Context <span className="optional">(optional)</span></label>
            <textarea className="form-input" value={context} onChange={(e) => setContext(e.target.value)} placeholder="Project context for workers..." rows={3} style={{ resize: 'vertical' }} />
          </div>
          <div className="form-field">
            <label className="form-label">Worker count</label>
            <input className="form-input" type="number" min={1} max={10} value={workerCount} onChange={(e) => setWorkerCount(Number(e.target.value))} />
          </div>
          {error && <p className="form-error">{error}</p>}
          <div className="form-actions">
            <button type="button" className="btn-secondary" onClick={onClose}>Cancel</button>
            <button type="submit" className="btn-primary" disabled={loading || !name.trim() || !folderId}>
              {loading ? 'Creating...' : 'Create Project'}
            </button>
          </div>
        </form>
      </div>
    </div>
  )
}

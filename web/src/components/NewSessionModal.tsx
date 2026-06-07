import { useEffect, useState, type FormEvent } from 'react'
import { useSessionsStore } from '../store/sessions'
import { useFoldersStore } from '../store/folders'
import { useResourcesStore } from '../store/resources'

interface Props {
  onClose: () => void
}

export default function NewSessionModal({ onClose }: Props) {
  const createSession = useSessionsStore((s) => s.createSession)
  const setActiveSession = useSessionsStore((s) => s.setActiveSession)
  const folders = useFoldersStore((s) => s.folders)
  const fetchFolders = useFoldersStore((s) => s.fetchFolders)
  const createFolder = useFoldersStore((s) => s.createFolder)
  const providers = useResourcesStore((s) => s.providers)
  const fetchModels = useResourcesStore((s) => s.fetchModels)

  const [name, setName] = useState('')
  // Same derived-default pattern as NewProjectModal — see comment there.
  const [chosenFolderId, setChosenFolderId] = useState<string | null>(null)
  const folderId = chosenFolderId ?? folders[0]?.id ?? ''
  const [model, setModel] = useState('')
  const [effort, setEffort] = useState('default')
  const [newFolderName, setNewFolderName] = useState('')
  const [newFolderPath, setNewFolderPath] = useState('')
  const [showNewFolder, setShowNewFolder] = useState(false)
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(false)

  useEffect(() => {
    fetchFolders()
  }, [fetchFolders])

  useEffect(() => {
    fetchModels()
  }, [fetchModels])

  const handleCreateFolder = async () => {
    if (!newFolderName.trim() || !newFolderPath.trim()) return
    try {
      const folder = await createFolder(newFolderName.trim(), newFolderPath.trim())
      setChosenFolderId(folder.id)
      setShowNewFolder(false)
      setNewFolderName('')
      setNewFolderPath('')
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to create folder')
    }
  }

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault()
    if (!name.trim() || !folderId) {
      setError('Name and folder are required')
      return
    }
    setLoading(true)
    setError('')
    try {
      const session = await createSession(
        name.trim(),
        folderId,
        model || undefined,
        effort !== 'default' ? effort : undefined,
      )
      setActiveSession(session.id)
      onClose()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to create session')
    } finally {
      setLoading(false)
    }
  }

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h2>New Session</h2>
        <form onSubmit={handleSubmit}>
          <div className="form-field">
            <label className="form-label">Name</label>
            <input
              className="form-input"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="My session"
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
                No folders yet. Create one below.
              </p>
            )}
            <button
              type="button"
              className="form-link-btn"
              onClick={() => setShowNewFolder(!showNewFolder)}
            >
              {showNewFolder ? 'Cancel' : '+ Add folder'}
            </button>
          </div>
          {showNewFolder && (
            <div className="form-inline-card">
              <input
                className="form-input"
                placeholder="Folder name"
                value={newFolderName}
                onChange={(e) => setNewFolderName(e.target.value)}
              />
              <input
                className="form-input"
                placeholder="/path/to/folder"
                value={newFolderPath}
                onChange={(e) => setNewFolderPath(e.target.value)}
              />
              <button
                type="button"
                className="btn-secondary"
                onClick={handleCreateFolder}
                disabled={!newFolderName.trim() || !newFolderPath.trim()}
              >
                Create Folder
              </button>
            </div>
          )}
          <div className="form-field">
            <label className="form-label">Model</label>
            <select className="form-input" value={model} onChange={(e) => setModel(e.target.value)}>
              <option value="">Server default</option>
              {providers.map((p) => (
                <optgroup key={p.id} label={p.display_name}>
                  {p.models.map((m) => (
                    <option key={m.id} value={m.id}>
                      {m.display_name}
                    </option>
                  ))}
                </optgroup>
              ))}
            </select>
          </div>
          <div className="form-field">
            <label className="form-label">Effort</label>
            <select
              className="form-input"
              value={effort}
              onChange={(e) => setEffort(e.target.value)}
            >
              <option value="default">Default</option>
              <option value="low">Low</option>
              <option value="medium">Medium</option>
              <option value="high">High</option>
            </select>
          </div>
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
              {loading ? 'Creating...' : 'Create Session'}
            </button>
          </div>
        </form>
      </div>
    </div>
  )
}

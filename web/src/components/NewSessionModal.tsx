import { useEffect, useMemo, useState, type FormEvent } from 'react'
import { useSessionsStore } from '../store/sessions'
import { useFoldersStore } from '../store/folders'
import { effortOptionsForModel, useResourcesStore } from '../store/resources'
import Modal from './Modal'
import ModelPicker from './ModelPicker'

interface Props {
  onClose: () => void
}

export default function NewSessionModal({ onClose }: Props) {
  const createSession = useSessionsStore((s) => s.createSession)
  const setActiveSession = useSessionsStore((s) => s.setActiveSession)
  const folders = useFoldersStore((s) => s.folders)
  const fetchFolders = useFoldersStore((s) => s.fetchFolders)
  const createFolder = useFoldersStore((s) => s.createFolder)
  const models = useResourcesStore((s) => s.models)
  const providers = useResourcesStore((s) => s.providers)
  const fetchModels = useResourcesStore((s) => s.fetchModels)

  const [name, setName] = useState('')
  // Same derived-default pattern as NewProjectModal — see comment there.
  const [chosenFolderId, setChosenFolderId] = useState<string | null>(null)
  const folderId = chosenFolderId ?? folders[0]?.id ?? ''
  const [model, setModel] = useState('')
  const [effort, setEffort] = useState('')
  // Chat sessions default OFF (workers default ON); a NULL column inherits
  // that, so an unchecked box just leaves auto-switch off.
  const [modelAutoswitch, setModelAutoswitch] = useState(false)
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

  // Effort options follow the chosen model's provider.
  const effortOptions = useMemo(() => effortOptionsForModel(model, providers), [model, providers])
  // Clear a now-invalid effort back to Default on model change so we never
  // submit one the provider can't use.
  const handleModelChange = (id: string) => {
    setModel(id)
    const opts = effortOptionsForModel(id, providers)
    if (providers.length > 0 && effort && !opts.some((o) => o.value === effort)) setEffort('')
  }

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
        effort || undefined,
        modelAutoswitch,
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
    <Modal onClose={onClose}>
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
          <ModelPicker
            value={model}
            onChange={handleModelChange}
            models={models}
            defaultLabel="Auto"
            ariaLabel="Select model"
            testId="new-session-model"
          />
        </div>
        <div className="form-field">
          <label className="form-label">Effort</label>
          <select className="form-input" value={effort} onChange={(e) => setEffort(e.target.value)}>
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
              checked={modelAutoswitch}
              onChange={(e) => setModelAutoswitch(e.target.checked)}
              data-testid="new-session-autoswitch"
            />
            <span>Allow auto-switching to a cheaper model</span>
          </label>
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
    </Modal>
  )
}

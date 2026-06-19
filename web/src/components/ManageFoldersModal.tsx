import { useEffect, useState } from 'react'
import { useFoldersStore } from '../store/folders'
import { authedFetch } from '../store/auth'
import type { Folder } from '../types/api'
import Modal from './Modal'

export default function FoldersPage() {
  const folders = useFoldersStore((s) => s.folders)
  const fetchFolders = useFoldersStore((s) => s.fetchFolders)
  const createFolder = useFoldersStore((s) => s.createFolder)

  const [name, setName] = useState('')
  const [path, setPath] = useState('')
  const [createDir, setCreateDir] = useState(false)
  const [error, setError] = useState('')
  const [creating, setCreating] = useState(false)
  const [deleteTarget, setDeleteTarget] = useState<Folder | null>(null)
  const [deleteSessionCount, setDeleteSessionCount] = useState<number | null>(null)
  const [moveTargetId, setMoveTargetId] = useState('')
  const [deleting, setDeleting] = useState(false)

  useEffect(() => {
    fetchFolders()
  }, [fetchFolders])

  const handleCreate = async () => {
    if (!name.trim() || !path.trim()) return
    setCreating(true)
    setError('')
    try {
      await createFolder(name.trim(), path.trim(), createDir)
      setName('')
      setPath('')
      setCreateDir(false)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to create folder')
    } finally {
      setCreating(false)
    }
  }

  const handleDeleteClick = async (folder: Folder) => {
    setError('')
    // Try to delete — if 409, it has sessions
    const res = await authedFetch(`/api/folders/${folder.id}`, { method: 'DELETE' })
    if (res.ok) {
      fetchFolders()
      return
    }
    if (res.status === 409) {
      const data = await res.json()
      setDeleteTarget(folder)
      setDeleteSessionCount(data.session_count ?? 0)
      // Pre-select a different folder for move target
      const other = folders.find((f) => f.id !== folder.id)
      setMoveTargetId(other?.id ?? '')
    } else {
      const data = await res.json().catch(() => ({ error: 'Failed to delete' }))
      setError(data.error || 'Failed to delete folder')
    }
  }

  const handleDeleteWithSessions = async () => {
    if (!deleteTarget) return
    setDeleting(true)
    try {
      await authedFetch(`/api/folders/${deleteTarget.id}/delete-sessions`, { method: 'POST' })
      setDeleteTarget(null)
      fetchFolders()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to delete')
    } finally {
      setDeleting(false)
    }
  }

  const handleMoveThenDelete = async () => {
    if (!deleteTarget || !moveTargetId) return
    setDeleting(true)
    try {
      await authedFetch(`/api/folders/${deleteTarget.id}/move-sessions`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ target_folder_id: moveTargetId }),
      })
      setDeleteTarget(null)
      fetchFolders()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to move sessions')
    } finally {
      setDeleting(false)
    }
  }

  const otherFolders = folders.filter((f) => f.id !== deleteTarget?.id)

  return (
    <div className="settings-page">
      <h2>Folders</h2>

      <section className="settings-section">
        <h3>Registered Folders</h3>
        <p style={{ fontSize: 'var(--text-sm)', color: 'var(--text2)', marginBottom: 16 }}>
          Folders map to directories on disk. Sessions and projects live inside folders.
        </p>

        <div className="folder-list">
          {folders.map((f) => (
            <div key={f.id} className="folder-row">
              <div className="folder-info">
                <strong>{f.name}</strong>
                <span className="folder-path">{f.path}</span>
              </div>
              <button className="folder-delete" onClick={() => handleDeleteClick(f)} title="Delete">
                &times;
              </button>
            </div>
          ))}
          {folders.length === 0 && (
            <p style={{ color: 'var(--text3)', fontSize: 'var(--text-sm)', padding: '12px 0' }}>
              No folders yet. Add one below to get started.
            </p>
          )}
        </div>
      </section>

      <section className="settings-section">
        <h3>Add Folder</h3>
        <div className="folder-create-fields">
          <input
            className="form-input"
            placeholder="Name (e.g. My Workspace)"
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
          <input
            className="form-input"
            placeholder="Path (e.g. /Users/me/projects)"
            value={path}
            onChange={(e) => setPath(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && handleCreate()}
          />
          <label className="form-checkbox-label" style={{ fontSize: 'var(--text-sm)' }}>
            <input
              type="checkbox"
              checked={createDir}
              onChange={(e) => setCreateDir(e.target.checked)}
            />
            <span>Create directory if it doesn't exist</span>
          </label>
          <button
            className="btn-primary"
            onClick={handleCreate}
            disabled={creating || !name.trim() || !path.trim()}
            style={{ alignSelf: 'flex-start' }}
          >
            {creating ? 'Adding...' : 'Add Folder'}
          </button>
        </div>
        {error && (
          <p className="form-error" style={{ marginTop: 8 }}>
            {error}
          </p>
        )}
      </section>

      {/* Delete folder dialog — shown when folder has sessions */}
      {deleteTarget && (
        <Modal onClose={() => setDeleteTarget(null)}>
          <h2>Delete "{deleteTarget.name}"</h2>
          <p className="modal-subtitle">
            This folder has {deleteSessionCount} session{deleteSessionCount !== 1 ? 's' : ''}.
            Choose how to proceed:
          </p>

          <div style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
            {/* Option 1: Delete sessions */}
            <button
              className="folder-delete-option"
              onClick={handleDeleteWithSessions}
              disabled={deleting}
            >
              <strong>Delete all sessions</strong>
              <span>
                Permanently delete all sessions and their events in this folder, then delete the
                folder.
              </span>
            </button>

            {/* Option 2: Move sessions */}
            {otherFolders.length > 0 && (
              <div className="folder-delete-option-group">
                <div className="folder-delete-option-move">
                  <strong>Move sessions to another folder</strong>
                  <div style={{ display: 'flex', gap: 8, marginTop: 8 }}>
                    <select
                      className="form-input"
                      value={moveTargetId}
                      onChange={(e) => setMoveTargetId(e.target.value)}
                      style={{ flex: 1 }}
                    >
                      {otherFolders.map((f) => (
                        <option key={f.id} value={f.id}>
                          {f.name}
                        </option>
                      ))}
                    </select>
                    <button
                      className="btn-primary"
                      onClick={handleMoveThenDelete}
                      disabled={deleting || !moveTargetId}
                      style={{ whiteSpace: 'nowrap' }}
                    >
                      Move & Delete
                    </button>
                  </div>
                </div>
              </div>
            )}

            {/* Option 3: Cancel */}
            <button
              className="btn-secondary"
              onClick={() => setDeleteTarget(null)}
              style={{ alignSelf: 'flex-start' }}
            >
              Cancel
            </button>
          </div>
        </Modal>
      )}
    </div>
  )
}

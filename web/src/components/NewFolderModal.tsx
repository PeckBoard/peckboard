import { useState, type FormEvent } from 'react'
import { useFoldersStore } from '../store/folders'
import type { Folder } from '../types/api'
import FieldError from './FieldError'
import Modal from './Modal'
import PathAutocomplete from './PathAutocomplete'

interface NewFolderModalProps {
  onClose: () => void
  onCreated?: (folder: Folder) => void
  initialName?: string
}

/**
 * Register a directory as a folder (admin-only on the API). Shared by the
 * Folders page and any form that needs a folder that doesn't exist yet —
 * it may open on top of another Modal, so it keeps its own `<form>` (in
 * Modal's portal) and stops its submit from reaching the parent's.
 */
export default function NewFolderModal({
  onClose,
  onCreated,
  initialName = '',
}: NewFolderModalProps) {
  const createFolder = useFoldersStore((s) => s.createFolder)
  const [name, setName] = useState(initialName)
  const [path, setPath] = useState('')
  // Server verdict on the typed path (null until it's an absolute path):
  // drives the exists / will-be-created status line under the field.
  const [pathExists, setPathExists] = useState<boolean | null>(null)
  // The user's explicit choice; until they toggle, the box follows the
  // path: checked when the directory doesn't exist yet.
  const [createDirChoice, setCreateDirChoice] = useState<boolean | null>(null)
  const createDir = createDirChoice ?? pathExists === false
  const [nameTouched, setNameTouched] = useState(false)
  const [pathTouched, setPathTouched] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')

  const trimmedName = name.trim()
  const trimmedPath = path.trim()
  const pathNotAbsolute = !!trimmedPath && !trimmedPath.startsWith('/')
  const nameError = nameTouched && !trimmedName ? 'Give the folder a name' : ''
  const pathError = pathNotAbsolute
    ? 'Path must be absolute (start with /)'
    : pathTouched && !trimmedPath
      ? 'Enter the directory path'
      : ''
  const disabledReason = !trimmedName
    ? 'Enter a name'
    : !trimmedPath
      ? 'Enter a path'
      : pathNotAbsolute
        ? 'Path must be absolute'
        : ''

  const submit = async (e?: FormEvent) => {
    e?.preventDefault()
    // React bubbles synthetic events through portals, so without this a
    // submit here would also fire the onSubmit of a form hosting the modal.
    e?.stopPropagation()
    setNameTouched(true)
    setPathTouched(true)
    if (disabledReason || busy) return
    setBusy(true)
    setError('')
    try {
      const folder = await createFolder(trimmedName, trimmedPath, createDir)
      onCreated?.(folder)
      onClose()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to create folder')
      setBusy(false)
    }
  }

  return (
    <Modal onClose={busy ? undefined : onClose} maxWidth={520} data-testid="new-folder-modal">
      <h2>New Folder</h2>
      <form onSubmit={submit}>
        <div className="form-field">
          <label className="form-label" htmlFor="new-folder-name">
            Name
          </label>
          <input
            id="new-folder-name"
            className="form-input"
            aria-label="Folder name"
            placeholder="My Workspace"
            value={name}
            onChange={(e) => setName(e.target.value)}
            onBlur={() => setNameTouched(true)}
            maxLength={200}
            autoFocus
            data-testid="new-folder-name"
          />
          <FieldError message={nameError} testId="new-folder-name-error" />
        </div>
        <div className="form-field">
          <span className="form-label">Path</span>
          <PathAutocomplete
            value={path}
            onChange={(v) => {
              setPath(v)
              setError('')
            }}
            onExistsChange={setPathExists}
            placeholder="/home/me/projects"
            testId="new-folder-path"
          />
          <FieldError message={pathError} testId="new-folder-path-error" />
          {trimmedPath.startsWith('/') && pathExists !== null && (
            <p className="form-hint" data-testid="folder-path-status" role="status">
              {pathExists
                ? 'Directory exists on the server.'
                : createDir
                  ? "Directory doesn't exist yet — it will be created."
                  : "Directory doesn't exist — check 'Create directory' below or fix the path."}
            </p>
          )}
          <label className="form-checkbox-label" style={{ fontSize: 'var(--text-sm)' }}>
            <input
              type="checkbox"
              checked={createDir}
              onChange={(e) => setCreateDirChoice(e.target.checked)}
              data-testid="new-folder-create-dir"
            />
            <span>Create directory if it doesn't exist</span>
          </label>
        </div>
        {error && (
          <p className="form-error" role="alert" data-testid="new-folder-error">
            {error}
          </p>
        )}
        <div className="form-actions">
          {disabledReason && (
            <span className="form-actions-reason" data-testid="new-folder-disabled-reason">
              {disabledReason}
            </span>
          )}
          <button type="button" className="btn-secondary" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button
            type="submit"
            className="btn-primary"
            disabled={busy || !!disabledReason}
            data-testid="new-folder-submit"
          >
            {busy ? 'Creating…' : 'Create Folder'}
          </button>
        </div>
      </form>
    </Modal>
  )
}

import { useEffect, useState } from 'react'
import { useFoldersStore } from '../store/folders'
import { authedFetch, useAuthStore } from '../store/auth'
import type { Folder } from '../types/api'
import Modal from './Modal'
import ConfirmDialog from './ConfirmDialog'
import RenameModal from './RenameModal'
import List from './List'
import ListViewHeader from './ListViewHeader'
import NewFolderModal from './NewFolderModal'
import type { MenuItem } from './Dropdown'

/** A plugin-contributed Folders-page entry (manifest `folder_items`). */
export interface FolderPluginItem {
  plugin: string
  id: string
  label: string
  icon?: string | null
  path: string
  /** The page acts on one git repo, not the folder — folder-level surfaces
   *  (this page, the repo-list header) skip it; the repo browser offers it
   *  per repo row instead. */
  repo_scoped?: boolean
}

interface Props {
  /** Folder-scoped plugin pages to offer on each folder row. */
  pluginItems?: FolderPluginItem[]
  /** Open one, for the given folder. */
  onOpenPlugin?: (folderId: string, itemId: string) => void
  /** Open the folder's repo browser (`/folders/<id>/repos`). */
  onOpenRepos?: (folderId: string) => void
  /** Rendered inside another dialog (New Project's "Manage folders"): a
   *  compact heading instead of the page-level list header. */
  embedded?: boolean
}

export default function FoldersPage({
  pluginItems = [],
  onOpenPlugin,
  onOpenRepos,
  embedded = false,
}: Props = {}) {
  const folders = useFoldersStore((s) => s.folders)
  const fetchFolders = useFoldersStore((s) => s.fetchFolders)
  const renameFolder = useFoldersStore((s) => s.renameFolder)
  // Registering a folder hands out host file access (it becomes the cwd and
  // file scope of every agent spawned inside it) and deleting one destroys
  // another user's work, so both are admin-only on the API. Mirror that here
  // so the UI never offers what the server will refuse.
  const isAdmin = useAuthStore((s) => s.user?.role === 'admin')

  const [showNew, setShowNew] = useState(false)
  const [error, setError] = useState('')
  const [deleteTarget, setDeleteTarget] = useState<Folder | null>(null)
  const [deleteSessionCount, setDeleteSessionCount] = useState<number | null>(null)
  const [moveTargetId, setMoveTargetId] = useState('')
  const [deleting, setDeleting] = useState(false)
  // Confirm gate in front of the DELETE. A folder with sessions still gets
  // the choice modal below (the 409 path); an empty one no longer vanishes
  // on a single click.
  const [confirmFolder, setConfirmFolder] = useState<Folder | null>(null)
  const [confirmError, setConfirmError] = useState<string | null>(null)
  const [confirmBusy, setConfirmBusy] = useState(false)
  const [renameTarget, setRenameTarget] = useState<Folder | null>(null)

  useEffect(() => {
    fetchFolders()
  }, [fetchFolders])

  const performDelete = async (folder: Folder) => {
    setError('')
    setConfirmError(null)
    setConfirmBusy(true)
    try {
      // Try to delete — if 409, it has sessions and the choice modal takes over
      const res = await authedFetch(`/api/folders/${folder.id}`, { method: 'DELETE' })
      if (res.ok) {
        setConfirmFolder(null)
        fetchFolders()
        return
      }
      if (res.status === 409) {
        const data = await res.json()
        setConfirmFolder(null)
        setDeleteTarget(folder)
        setDeleteSessionCount(data.session_count ?? 0)
        // Pre-select a different folder for move target
        const other = folders.find((f) => f.id !== folder.id)
        setMoveTargetId(other?.id ?? '')
        return
      }
      const data = await res.json().catch(() => ({ error: 'Failed to delete' }))
      setConfirmError(data.error || 'Failed to delete folder')
    } catch {
      setConfirmError('Failed to delete folder')
    } finally {
      setConfirmBusy(false)
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
  const openNew = isAdmin ? () => setShowNew(true) : undefined

  // One list feeds both the 3-dot menu and the right-click menu.
  const buildMenu = (f: Folder): MenuItem[] => {
    const items: MenuItem[] = []
    // Repo browser — read-only, so not admin-gated: any authenticated user
    // may already call /api/repos.
    if (onOpenRepos) items.push({ label: 'Repos', onSelect: () => onOpenRepos(f.id) })
    // Folder-scoped plugin pages (manifest `folder_items`). Deliberately
    // outside the isAdmin gate: these are the same pages a non-admin already
    // reaches from a project or session, only aimed at the folder itself.
    // Items marked repo_scoped act on ONE repo and live on the repo browser
    // rows instead — never here.
    for (const item of pluginItems) {
      if (item.repo_scoped) continue
      items.push({ label: item.label, onSelect: () => onOpenPlugin?.(f.id, item.id) })
    }
    if (isAdmin) {
      items.push(
        { divider: true },
        { label: 'Rename', onSelect: () => setRenameTarget(f) },
        {
          label: 'Delete',
          danger: true,
          onSelect: () => {
            setConfirmError(null)
            setConfirmFolder(f)
          },
        },
      )
    }
    return items
  }

  const hint = (
    <p className="form-hint folders-hint" data-testid="folders-hint">
      Folders map to directories on disk. Sessions and projects live inside folders.
      {!isAdmin && ' Only an admin can add or remove them.'}
    </p>
  )

  return (
    <div className={embedded ? 'folders-embedded' : 'list-view'}>
      {embedded ? (
        <div className="folders-embedded-header">
          <h2>Folders</h2>
          {openNew && (
            <button
              type="button"
              className="btn-secondary"
              onClick={openNew}
              data-testid="folders-new-folder"
            >
              New Folder
            </button>
          )}
        </div>
      ) : (
        <ListViewHeader
          title="Folders"
          actionLabel={openNew ? '+ New Folder' : undefined}
          onAction={openNew}
          actionTestId="folders-new-folder"
        />
      )}
      {error && (
        <p className="form-error" role="alert" data-testid="folders-error">
          {error}
        </p>
      )}
      <List<Folder>
        items={folders}
        getKey={(f) => f.id}
        bodyClassName={embedded ? 'list-view-rows' : undefined}
        onActivate={(f) => onOpenRepos?.(f.id)}
        getMenuItems={buildMenu}
        renderItem={(f) => (
          <span className="folder-info" data-testid={`folder-row-${f.name}`}>
            <strong>{f.name}</strong>
            <span className="folder-path">{f.path}</span>
          </span>
        )}
        emptyState={
          <div className="list-view-empty" data-testid="folders-empty">
            <p>No folders yet.</p>
            {openNew ? (
              <button type="button" className="list-view-empty-action" onClick={openNew}>
                New Folder
              </button>
            ) : (
              <p>Ask an admin to add one.</p>
            )}
          </div>
        }
        footer={folders.length > 0 ? hint : undefined}
      />

      {showNew && <NewFolderModal onClose={() => setShowNew(false)} />}

      {renameTarget && (
        <RenameModal
          title="Rename folder"
          label="Folder name"
          initialValue={renameTarget.name}
          onSubmit={async (name) => {
            await renameFolder(renameTarget.id, name)
          }}
          onClose={() => setRenameTarget(null)}
        />
      )}

      {/* Delete folder dialog — shown when folder has sessions */}
      {confirmFolder && (
        <ConfirmDialog
          testId="folder-delete-confirm"
          danger
          title={`Delete folder "${confirmFolder.name}"?`}
          message={`"${confirmFolder.name}" (${confirmFolder.path}) is unregistered from PeckBoard, together with its repeating tasks and folder-scoped variables. The directory on disk is left alone. If the folder still holds sessions, you choose what happens to them next.`}
          confirmLabel="Delete folder"
          error={confirmError}
          busy={confirmBusy}
          busyLabel="Deleting…"
          onConfirm={() => void performDelete(confirmFolder)}
          onCancel={() => {
            setConfirmFolder(null)
            setConfirmError(null)
          }}
        />
      )}

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

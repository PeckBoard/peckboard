import { useState, type FormEvent } from 'react'
import List from '../List'
import Modal from '../Modal'
import ConfirmDialog from '../ConfirmDialog'
import FieldError from '../FieldError'
import { useAuthStore } from '../../store/auth'
import { useSkillsStore, type SkillSource } from '../../store/skills'
import type { MenuItem } from '../Dropdown'

const when = (at: string | null) => (at ? new Date(at).toLocaleString() : 'never')

/**
 * Where skills come from: the built-in public catalog (read-only) and any
 * team git repositories. A team repo is cloned and synced on demand; its
 * last sync error shows on the row.
 */
export default function SkillSourcesTab() {
  const sources = useSkillsStore((s) => s.sources)
  const loaded = useSkillsStore((s) => s.sourcesLoaded)
  const syncSource = useSkillsStore((s) => s.syncSource)
  const deleteSource = useSkillsStore((s) => s.deleteSource)
  const isAdmin = useAuthStore((s) => s.user?.role === 'admin')
  const [adding, setAdding] = useState(false)
  const [syncing, setSyncing] = useState<string | null>(null)
  const [syncError, setSyncError] = useState<string | null>(null)
  const [confirmRemove, setConfirmRemove] = useState<SkillSource | null>(null)
  const [removing, setRemoving] = useState(false)
  const [removeError, setRemoveError] = useState<string | null>(null)

  const sync = (s: SkillSource) => {
    setSyncing(s.id)
    setSyncError(null)
    syncSource(s.id)
      .catch((e: Error) => setSyncError(`${s.name}: ${e.message}`))
      .finally(() => setSyncing(null))
  }

  const menu = (s: SkillSource): MenuItem[] => [
    { label: 'Sync now', onSelect: () => sync(s), testId: `skill-source-sync-${s.name}` },
    { divider: true, hidden: s.kind === 'public' },
    {
      label: 'Remove',
      danger: true,
      hidden: s.kind === 'public',
      disabled: !isAdmin,
      onSelect: () => setConfirmRemove(s),
    },
  ]

  return (
    <div className="skills-tab" data-testid="skills-sources">
      <div className="settings-section-head">
        <p className="form-hint">
          The public catalog is built in. Add a private git repository to share skills, groups and
          team memory with your team.
        </p>
        <button
          type="button"
          className="btn-primary btn-sm"
          onClick={() => setAdding(true)}
          disabled={!isAdmin}
          data-testid="skill-source-new"
        >
          + Team repository
        </button>
      </div>
      {!isAdmin && (
        <span className="form-actions-reason">Only an admin can add or remove sources.</span>
      )}
      {syncError && (
        <p className="form-error" role="alert" data-testid="skill-source-sync-error">
          {syncError}
        </p>
      )}
      <List<SkillSource>
        items={sources}
        getKey={(s) => s.id}
        bodyClassName="list-view-rows"
        onActivate={(s) => sync(s)}
        getMenuItems={menu}
        renderItem={(s) => (
          <>
            <span className="list-view-name" data-testid={`skill-source-row-${s.name}`}>
              {s.name}
            </span>
            <span className="list-view-meta">
              <span className="list-view-tag">{s.kind === 'public' ? 'Public' : 'Team'}</span>
              {s.kind === 'public' && <span className="list-view-tag">read-only</span>}
              <span className="skill-row-desc" title={s.url}>
                {s.url}
                {s.branch ? ` @ ${s.branch}` : ''}
              </span>
              <span>{syncing === s.id ? 'Syncing…' : `Synced ${when(s.last_sync_at)}`}</span>
              {s.last_error && (
                <span
                  className="list-view-tag skill-source-error"
                  title={s.last_error}
                  data-testid={`skill-source-error-${s.name}`}
                >
                  {s.last_error}
                </span>
              )}
            </span>
          </>
        )}
        emptyState={
          <div className="list-view-empty">{loaded ? 'No sources configured.' : 'Loading…'}</div>
        }
      />
      <p className="form-hint">Click a source to sync it now.</p>
      {adding && <AddSourceModal onClose={() => setAdding(false)} />}
      {confirmRemove && (
        <ConfirmDialog
          title="Remove source"
          message={`Remove "${confirmRemove.name}"? Skills subscribed from it stop updating and its team memory is no longer shown. The repository itself is not touched.`}
          confirmLabel="Remove"
          danger
          busy={removing}
          busyLabel="Removing…"
          error={removeError}
          testId="skill-source-remove-confirm"
          onConfirm={() => {
            setRemoving(true)
            setRemoveError(null)
            deleteSource(confirmRemove.id)
              .then(() => setConfirmRemove(null))
              .catch((e: Error) => setRemoveError(e.message))
              .finally(() => setRemoving(false))
          }}
          onCancel={() => {
            if (removing) return
            setConfirmRemove(null)
            setRemoveError(null)
          }}
        />
      )}
    </div>
  )
}

function AddSourceModal({ onClose }: { onClose: () => void }) {
  const addSource = useSkillsStore((s) => s.addSource)
  const [name, setName] = useState('')
  const [url, setUrl] = useState('')
  const [branch, setBranch] = useState('main')
  const [touched, setTouched] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)

  const nameError = !name.trim() ? 'Give the source a name' : ''
  // Mirrors the server's transport allowlist (https / ssh / user@host:path /
  // a local path) so the common mistake is caught before the round trip.
  const u = url.trim()
  const urlError = !u
    ? 'Enter the git URL of the repository'
    : !(
          /^(https:\/\/|ssh:\/\/|file:\/\/|\/)/.test(u) ||
          (u.includes('@') && u.includes(':') && !u.includes('::'))
        )
      ? 'Use an https://, ssh:// or git@host:path URL'
      : ''

  const submit = (e: FormEvent) => {
    e.preventDefault()
    setTouched(true)
    if (nameError || urlError) return
    setSaving(true)
    setError(null)
    addSource({
      kind: 'team',
      name: name.trim(),
      url: url.trim(),
      branch: branch.trim() || undefined,
    })
      .then(onClose)
      .catch((err: Error) => setError(err.message))
      .finally(() => setSaving(false))
  }

  return (
    <Modal onClose={onClose} maxWidth={520} data-testid="skill-source-modal">
      <h2>Add team repository</h2>
      <form onSubmit={submit}>
        <div className="form-field">
          <label className="form-label" htmlFor="skill-source-name">
            Name
          </label>
          <input
            id="skill-source-name"
            className="form-input"
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="Acme team"
            data-testid="skill-source-name"
            autoFocus
          />
          <FieldError message={touched ? nameError : ''} />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="skill-source-url">
            Repository URL
          </label>
          <input
            id="skill-source-url"
            className="form-input"
            value={url}
            onChange={(e) => setUrl(e.target.value)}
            placeholder="git@github.com:acme/skills.git"
            data-testid="skill-source-url"
          />
          <FieldError message={touched ? urlError : ''} testId="skill-source-url-error" />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="skill-source-branch">
            Branch
          </label>
          <input
            id="skill-source-branch"
            className="form-input"
            value={branch}
            onChange={(e) => setBranch(e.target.value)}
            data-testid="skill-source-branch"
          />
        </div>
        {error && (
          <p className="form-error" role="alert" data-testid="skill-source-add-error">
            {error}
          </p>
        )}
        <div className="form-actions">
          {touched && (nameError || urlError) && (
            <span className="form-actions-reason">Fix the fields marked above</span>
          )}
          <button type="button" className="btn-secondary" onClick={onClose}>
            Cancel
          </button>
          <button
            type="submit"
            className="btn-primary"
            disabled={saving}
            data-testid="skill-source-save"
          >
            {saving ? 'Adding…' : 'Add repository'}
          </button>
        </div>
      </form>
    </Modal>
  )
}

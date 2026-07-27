import { useCallback, useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'
import { useFoldersStore } from '../store/folders'
import type { EnvVar } from '../types/api'
import ConfirmDialog from './ConfirmDialog'
import SecretInput from './SecretInput'

/**
 * Settings section for user-defined environment variables injected into the
 * commands agents run — never into the agent process itself, and values
 * printed to console output are masked so the agent can't read them.
 * Plain vars show a masked value with a reveal toggle;
 * encrypted vars expose metadata only (the server never returns the
 * ciphertext), so editing one means entering a NEW value plus the owner's
 * password. "Lock now" drops the server's cache of decrypted values.
 */
export default function EnvVarsSection() {
  const [vars, setVars] = useState<EnvVar[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [revealed, setRevealed] = useState<Record<string, boolean>>({})
  const [deleting, setDeleting] = useState<string | null>(null)
  const [locking, setLocking] = useState(false)
  const folders = useFoldersStore((s) => s.folders)
  const fetchFolders = useFoldersStore((s) => s.fetchFolders)

  // Add/edit form. `editing` holds the name of the row being edited (the
  // upsert is keyed by (name, scope), so renaming or re-scoping while
  // editing creates a new var).
  const [editing, setEditing] = useState<string | null>(null)
  const [name, setName] = useState('')
  const [value, setValue] = useState('')
  const [encrypt, setEncrypt] = useState(false)
  const [password, setPassword] = useState('')
  const [folderId, setFolderId] = useState<string>('')
  const [formError, setFormError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  // Delete confirmation. The dialog stays open on failure so the user reads
  // what happened instead of watching it vanish as if it had worked.
  const [confirmDelete, setConfirmDelete] = useState<EnvVar | null>(null)
  const [deleteError, setDeleteError] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      const res = await authedFetch('/api/env-vars')
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      const data = (await res.json()) as { vars: EnvVar[] }
      setVars(data.vars)
      setError(null)
    } catch {
      setError('Could not load environment variables.')
      setVars([])
    }
  }, [])

  // Initial fetch on mount, matching the codebase's fetch-in-effect style.
  useEffect(() => {
    let cancelled = false
    authedFetch('/api/env-vars')
      .then((res) => res.json().then((data) => ({ ok: res.ok, data })))
      .then(({ ok, data }) => {
        if (cancelled) return
        if (!ok) throw new Error('bad status')
        setVars((data as { vars: EnvVar[] }).vars)
        setError(null)
      })
      .catch(() => {
        if (cancelled) return
        setError('Could not load environment variables.')
        setVars([])
      })
    return () => {
      cancelled = true
    }
  }, [])

  // Folder list for the scope select.
  useEffect(() => {
    void fetchFolders()
  }, [fetchFolders])
  const clearForm = () => {
    setEditing(null)
    setName('')
    setValue('')
    setEncrypt(false)
    setPassword('')
    setFolderId('')
    setFormError(null)
  }

  const startEdit = (v: EnvVar) => {
    setEditing(v.name)
    setName(v.name)
    setValue(v.encrypted ? '' : (v.value ?? ''))
    setEncrypt(v.encrypted)
    setPassword('')
    setFolderId(v.folder_id ?? '')
    setFormError(null)
  }

  const submit = async () => {
    if (saving) return
    setSaving(true)
    setFormError(null)
    try {
      const body: Record<string, unknown> = {
        name: name.trim(),
        value,
        encrypt,
        folder_id: folderId === '' ? null : folderId,
      }
      if (encrypt) body.password = password
      const res = await authedFetch('/api/env-vars', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      })
      if (res.status === 403) {
        setFormError('Wrong password')
        return
      }
      if (!res.ok) {
        const d = (await res.json().catch(() => null)) as { error?: string } | null
        setFormError(d?.error ?? `Failed (${res.status}).`)
        return
      }
      clearForm()
      await load()
    } catch {
      setFormError('Failed — server unreachable.')
    } finally {
      setSaving(false)
    }
  }

  const remove = async (v: EnvVar) => {
    setError(null)
    setDeleteError(null)
    setDeleting(v.id)
    try {
      const res = await authedFetch(`/api/env-vars/${encodeURIComponent(v.id)}`, {
        method: 'DELETE',
      })
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      if (editing === v.name) clearForm()
      setConfirmDelete(null)
      await load()
    } catch {
      setDeleteError(`Could not delete "${v.name}".`)
    } finally {
      setDeleting(null)
    }
  }

  const lockNow = async () => {
    if (locking) return
    setError(null)
    setLocking(true)
    try {
      const res = await authedFetch('/api/env-vars/lock', { method: 'POST' })
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
    } catch {
      setError('Could not lock.')
    } finally {
      setLocking(false)
    }
  }

  return (
    <section className="settings-section" data-testid="env-vars-section">
      <h3>Environment Variables</h3>
      <p className="form-hint">
        Injected into the commands agents run — never into the agent itself; secret values that show
        up in console output are masked with ******** so the agent can&rsquo;t read them. Encrypted
        variables are sealed with their owner&rsquo;s login password; a session that needs them
        prompts the owner to unlock.
      </p>

      {error && <p className="settings-error">{error}</p>}

      {vars === null ? (
        <p className="settings-loading">Loading environment variables...</p>
      ) : vars.length === 0 ? (
        <p className="settings-loading">No environment variables yet. Add one below.</p>
      ) : (
        <ul className="env-var-list" aria-label="Environment variables">
          {vars.map((v) => (
            <li className="env-var-row" key={v.id} data-testid={`env-var-${v.name}`}>
              <div className="env-var-main">
                <span className="env-var-name">{v.name}</span>
                <span className={`env-var-scope${v.folder_id ? '' : ' env-var-scope--global'}`}>
                  {v.folder_name ?? 'Global'}
                </span>
                {v.encrypted ? (
                  <span className="env-var-meta">
                    <span aria-hidden>🔒</span> Encrypted with {v.encrypted_by_username ?? 'a user'}
                    &rsquo;s password
                  </span>
                ) : (
                  <span className="env-var-value">
                    {revealed[v.id] ? (v.value ?? '') : '••••••••'}
                  </span>
                )}
              </div>
              <div className="env-var-actions">
                {!v.encrypted && (
                  <button
                    type="button"
                    className="btn-secondary btn-sm"
                    onClick={() => setRevealed((r) => ({ ...r, [v.id]: !r[v.id] }))}
                    data-testid={`env-var-reveal-${v.name}`}
                  >
                    {revealed[v.id] ? 'Hide' : 'Reveal'}
                  </button>
                )}
                <button
                  type="button"
                  className="btn-secondary btn-sm"
                  onClick={() => startEdit(v)}
                  data-testid={`env-var-edit-${v.name}`}
                >
                  Edit
                </button>
                <button
                  type="button"
                  className="btn-secondary btn-sm"
                  onClick={() => {
                    setDeleteError(null)
                    setConfirmDelete(v)
                  }}
                  disabled={deleting === v.id}
                  data-testid={`env-var-delete-${v.name}`}
                >
                  {deleting === v.id ? 'Deleting…' : 'Delete'}
                </button>
              </div>
            </li>
          ))}
        </ul>
      )}

      <form
        className="env-var-form"
        onSubmit={(e) => {
          e.preventDefault()
          void submit()
        }}
      >
        <h4>{editing ? `Edit ${editing}` : 'Add variable'}</h4>
        {editing && encrypt && (
          <p className="form-hint">
            Encrypted values can&rsquo;t be shown — enter a new value and your password to replace
            it.
          </p>
        )}
        <div className="form-field">
          <label className="form-label">Name</label>
          <input
            className="form-input"
            value={name}
            autoComplete="off"
            placeholder="MY_VAR"
            onChange={(e) => setName(e.target.value)}
            data-testid="env-var-name-input"
          />
        </div>
        <div className="form-field">
          <label className="form-label">Value</label>
          <SecretInput
            // Remount when the form switches target so a revealed value from
            // the previous row can't carry over into the next one.
            key={editing ?? 'new'}
            className="form-input"
            value={value}
            onChange={setValue}
            testId="env-var-value-input"
            revealTestId="env-var-value-reveal"
          />
        </div>
        <div className="form-field">
          <label className="form-label">Scope</label>
          <select
            className="form-input"
            value={folderId}
            onChange={(e) => setFolderId(e.target.value)}
            data-testid="env-var-scope-select"
          >
            <option value="">Global</option>
            {folders.map((f) => (
              <option key={f.id} value={f.id}>
                {f.name}
              </option>
            ))}
          </select>
        </div>
        <div className="form-field">
          <label className="form-label env-var-encrypt-label">
            <input
              type="checkbox"
              checked={encrypt}
              onChange={(e) => setEncrypt(e.target.checked)}
              data-testid="env-var-encrypt-checkbox"
            />
            Encrypt with my password
          </label>
        </div>
        {encrypt && (
          <div className="form-field">
            <label className="form-label">Your password</label>
            <input
              className="form-input"
              type="password"
              value={password}
              autoComplete="off"
              onChange={(e) => setPassword(e.target.value)}
              data-testid="env-var-password-input"
            />
          </div>
        )}
        {formError && <p className="form-error">{formError}</p>}
        <div className="form-actions">
          {editing && (
            <button type="button" className="btn-secondary" onClick={clearForm}>
              Cancel
            </button>
          )}
          <button
            type="submit"
            className="btn-primary"
            disabled={saving || name.trim().length === 0 || (encrypt && password.length === 0)}
            data-testid="env-var-save-btn"
          >
            {saving ? 'Saving…' : editing ? 'Save' : 'Add'}
          </button>
        </div>
      </form>

      <div className="env-var-lock">
        <button
          type="button"
          className="btn-secondary"
          onClick={() => void lockNow()}
          disabled={locking}
          data-testid="env-vars-lock-btn"
        >
          {locking ? 'Locking…' : 'Lock now'}
        </button>
        <p className="form-hint">
          Clears unlocked values from server memory; sessions will prompt again.
        </p>
      </div>
      {confirmDelete && (
        <ConfirmDialog
          testId="env-var-delete-confirm"
          danger
          title={`Delete ${confirmDelete.name}?`}
          message={`${confirmDelete.name} (${confirmDelete.folder_name ?? 'Global'}) is removed for good, and commands agents run stop receiving it.${
            confirmDelete.encrypted
              ? ' The encrypted value can’t be recovered — restoring it means re-entering the value and the owner’s password.'
              : ''
          }`}
          confirmLabel="Delete variable"
          error={deleteError}
          busy={deleting === confirmDelete.id}
          busyLabel="Deleting…"
          onConfirm={() => void remove(confirmDelete)}
          onCancel={() => {
            setConfirmDelete(null)
            setDeleteError(null)
          }}
        />
      )}
    </section>
  )
}

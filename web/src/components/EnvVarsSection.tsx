import { useCallback, useEffect, useState } from 'react'
import { authedFetch, useAuthStore } from '../store/auth'
import { useFoldersStore } from '../store/folders'
import type { EnvVar } from '../types/api'
import ConfirmDialog from './ConfirmDialog'
import SecretInput from './SecretInput'
import {
  UNLOCK_DURATIONS,
  formatRemaining,
  loadUnlockDuration,
  saveUnlockDuration,
  type UnlockDuration,
} from '../util/envUnlock'

/** `GET /api/env-vars/unlock-status` — the caller's own unlock window. */
interface UnlockStatus {
  unlocked: boolean
  /** Seconds left; `null` while locked AND for the "until I lock" window. */
  expires_in_secs: number | null
  /** Encrypted vars the caller owns; 0 hides the unlock controls. */
  var_count: number
}

/**
 * Settings section for user-defined environment variables injected into the
 * commands agents run — never into the agent process itself, and values
 * printed to console output are masked so the agent can't read them.
 * Plain vars show a masked value with a reveal toggle;
 * encrypted vars expose metadata only (the server never returns the
 * ciphertext), so editing one means entering a NEW value plus the owner's
 * password. The unlock panel decrypts the caller's own encrypted vars for a
 * chosen window (15m … 24h, or until they lock) so sessions started inside it
 * never raise an unlock prompt; "Lock now" ends the window early.
 */
export default function EnvVarsSection() {
  // Writing a var is host-wide (no per-user ownership), so it's admin-only
  // on the API; mirror that here.
  const isAdmin = useAuthStore((s) => s.user?.role === 'admin')
  const [vars, setVars] = useState<EnvVar[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [revealed, setRevealed] = useState<Record<string, boolean>>({})
  const [deleting, setDeleting] = useState<string | null>(null)
  const [locking, setLocking] = useState(false)
  const [status, setStatus] = useState<UnlockStatus | null>(null)
  const [unlockPassword, setUnlockPassword] = useState('')
  const [duration, setDuration] = useState<UnlockDuration>(loadUnlockDuration)
  const [unlocking, setUnlocking] = useState(false)
  const [unlockError, setUnlockError] = useState<string | null>(null)
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

  // ── Unlock window ───────────────────────────────────────────────────
  // Unlocking here primes the server's cache up front, so every session
  // started inside the window uses the decrypted values instead of raising
  // an unlock prompt.
  const loadStatus = useCallback(async () => {
    try {
      const res = await authedFetch('/api/env-vars/unlock-status')
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      setStatus((await res.json()) as UnlockStatus)
    } catch {
      setStatus(null)
    }
  }, [])

  // Initial fetch, in the same promise-chain shape as the var list above:
  // an effect body that awaits and then setStates trips
  // react-hooks/set-state-in-effect.
  useEffect(() => {
    let cancelled = false
    authedFetch('/api/env-vars/unlock-status')
      .then((res) => res.json().then((data) => ({ ok: res.ok, data })))
      .then(({ ok, data }) => {
        if (cancelled) return
        setStatus(ok ? (data as UnlockStatus) : null)
      })
      .catch(() => {
        if (cancelled) return
        setStatus(null)
      })
    return () => {
      cancelled = true
    }
  }, [])

  // A prompt answered in EnvUnlockDialog opens a window too — refetch when
  // one resolves so this status line doesn't sit there saying "Locked".
  useEffect(() => {
    const onResolved = () => void loadStatus()
    window.addEventListener('peckboard:env-unlock-resolved', onResolved)
    return () => window.removeEventListener('peckboard:env-unlock-resolved', onResolved)
  }, [loadStatus])

  // Local countdown. Reaching zero means the server has purged the values
  // too, so flipping to locked here matches what the next session will see.
  const unlocked = status?.unlocked ?? false
  const bounded = typeof status?.expires_in_secs === 'number'
  useEffect(() => {
    if (!unlocked || !bounded) return
    const timer = setInterval(() => {
      setStatus((s) => {
        if (!s?.unlocked || typeof s.expires_in_secs !== 'number') return s
        const left = s.expires_in_secs - 1
        return left <= 0
          ? { ...s, unlocked: false, expires_in_secs: null }
          : { ...s, expires_in_secs: left }
      })
    }, 1000)
    return () => clearInterval(timer)
  }, [unlocked, bounded])

  const unlockNow = async () => {
    if (unlocking) return
    setUnlocking(true)
    setUnlockError(null)
    try {
      const res = await authedFetch('/api/env-vars/unlock', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ password: unlockPassword, duration }),
      })
      // Never keep a password in state past the request.
      setUnlockPassword('')
      if (res.status === 403) {
        setUnlockError('Wrong password')
        return
      }
      if (!res.ok) {
        const d = (await res.json().catch(() => null)) as { error?: string } | null
        setUnlockError(d?.error ?? `Failed (${res.status}).`)
        return
      }
      saveUnlockDuration(duration)
      await loadStatus()
    } catch {
      setUnlockPassword('')
      setUnlockError('Failed — server unreachable.')
    } finally {
      setUnlocking(false)
    }
  }

  const lockNow = async () => {
    if (locking) return
    setError(null)
    setLocking(true)
    try {
      const res = await authedFetch('/api/env-vars/lock', { method: 'POST' })
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      await loadStatus()
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
                {isAdmin && (
                  <button
                    type="button"
                    className="btn-secondary btn-sm"
                    onClick={() => startEdit(v)}
                    data-testid={`env-var-edit-${v.name}`}
                  >
                    Edit
                  </button>
                )}
                {isAdmin && (
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
                )}
              </div>
            </li>
          ))}
        </ul>
      )}

      {isAdmin && (
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
            <label className="form-label" htmlFor="env-var-name">
              Name
            </label>
            <input
              id="env-var-name"
              className="form-input"
              value={name}
              autoComplete="off"
              placeholder="MY_VAR"
              onChange={(e) => setName(e.target.value)}
              data-testid="env-var-name-input"
            />
          </div>
          <div className="form-field">
            <label className="form-label" htmlFor="env-var-value">
              Value
            </label>
            <SecretInput
              id="env-var-value"
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
            <label className="form-label" htmlFor="env-var-scope">
              Scope
            </label>
            <select
              id="env-var-scope"
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
              <label className="form-label" htmlFor="env-var-password">
                Your password
              </label>
              <input
                id="env-var-password"
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
      )}

      {status && status.var_count > 0 && (
        <div className="env-var-unlock" data-testid="env-unlock-panel">
          <h4>Unlock your encrypted variables</h4>
          <p className="env-var-unlock-status" data-testid="env-unlock-status">
            {status.unlocked ? (
              <>
                <span aria-hidden>🔓</span>{' '}
                {status.expires_in_secs === null
                  ? 'Unlocked until you lock'
                  : `Unlocked — ${formatRemaining(status.expires_in_secs)} left`}
              </>
            ) : (
              <>
                <span aria-hidden>🔒</span> Locked — sessions will prompt for your password
              </>
            )}
          </p>
          <form
            onSubmit={(e) => {
              e.preventDefault()
              void unlockNow()
            }}
          >
            <div className="form-field">
              <label className="form-label" htmlFor="env-unlock-password">
                Your password
              </label>
              <input
                id="env-unlock-password"
                className="form-input"
                type="password"
                value={unlockPassword}
                autoComplete="off"
                onChange={(e) => setUnlockPassword(e.target.value)}
                data-testid="env-unlock-password"
              />
            </div>
            <div className="form-field">
              <label className="form-label" htmlFor="env-unlock-duration-select">
                Keep unlocked for
              </label>
              <select
                id="env-unlock-duration-select"
                className="form-input"
                value={duration}
                onChange={(e) => setDuration(e.target.value as UnlockDuration)}
                data-testid="env-unlock-duration-select"
              >
                {UNLOCK_DURATIONS.map((d) => (
                  <option key={d.value} value={d.value}>
                    {d.label}
                  </option>
                ))}
              </select>
            </div>
            {unlockError && <p className="form-error">{unlockError}</p>}
            <div className="form-actions">
              <button
                type="submit"
                className="btn-primary"
                disabled={unlocking || unlockPassword.length === 0}
                data-testid="env-unlock-btn"
              >
                {unlocking ? 'Unlocking…' : status.unlocked ? 'Extend unlock' : 'Unlock'}
              </button>
              {status.unlocked && (
                <button
                  type="button"
                  className="btn-secondary"
                  onClick={() => void lockNow()}
                  disabled={locking}
                  data-testid="env-vars-lock-btn"
                >
                  {locking ? 'Locking…' : 'Lock now'}
                </button>
              )}
            </div>
          </form>
          <p className="form-hint">
            Sessions started while unlocked use the decrypted values instead of prompting.
            {duration === 'until-lock'
              ? ' “Until I lock” keeps them in server memory until you lock or the server restarts.'
              : ' Locking clears them from server memory; sessions will prompt again.'}
          </p>
        </div>
      )}
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

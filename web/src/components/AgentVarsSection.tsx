import { useCallback, useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'
import { useFoldersStore } from '../store/folders'
import type { AgentVar } from '../types/api'

/**
 * Settings section for agent variables — shared key/value state agents read
 * AND write via the list_variables / set_variable / delete_variable tools.
 * Values are plain (never masked or encrypted); a folder-scoped variable
 * shadows a global one with the same name. Secrets belong in Environment
 * Variables, not here.
 */
export default function AgentVarsSection() {
  const [vars, setVars] = useState<AgentVar[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [deleting, setDeleting] = useState<string | null>(null)
  const folders = useFoldersStore((s) => s.folders)
  const fetchFolders = useFoldersStore((s) => s.fetchFolders)

  // Add/edit form. `editing` holds the name of the row being edited (the
  // upsert is keyed by (name, scope), so renaming or re-scoping while
  // editing creates a new var).
  const [editing, setEditing] = useState<string | null>(null)
  const [name, setName] = useState('')
  const [value, setValue] = useState('')
  const [folderId, setFolderId] = useState<string>('')
  const [formError, setFormError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)

  const load = useCallback(async () => {
    try {
      const res = await authedFetch('/api/agent-vars')
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      const data = (await res.json()) as { vars: AgentVar[] }
      setVars(data.vars)
      setError(null)
    } catch {
      setError('Could not load agent variables.')
      setVars([])
    }
  }, [])

  // Initial fetch on mount, matching the codebase's fetch-in-effect style.
  useEffect(() => {
    let cancelled = false
    authedFetch('/api/agent-vars')
      .then((res) => res.json().then((data) => ({ ok: res.ok, data })))
      .then(({ ok, data }) => {
        if (cancelled) return
        if (!ok) throw new Error('bad status')
        setVars((data as { vars: AgentVar[] }).vars)
        setError(null)
      })
      .catch(() => {
        if (cancelled) return
        setError('Could not load agent variables.')
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
    setFolderId('')
    setFormError(null)
  }

  const startEdit = (v: AgentVar) => {
    setEditing(v.name)
    setName(v.name)
    setValue(v.value)
    setFolderId(v.folder_id ?? '')
    setFormError(null)
  }

  const submit = async () => {
    if (saving) return
    setSaving(true)
    setFormError(null)
    try {
      const body = {
        name: name.trim(),
        value,
        folder_id: folderId === '' ? null : folderId,
      }
      const res = await authedFetch('/api/agent-vars', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      })
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

  const remove = async (v: AgentVar) => {
    setError(null)
    setDeleting(v.id)
    try {
      const res = await authedFetch(`/api/agent-vars/${encodeURIComponent(v.id)}`, {
        method: 'DELETE',
      })
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      if (editing === v.name) clearForm()
      await load()
    } catch {
      setError(`Could not delete "${v.name}".`)
    } finally {
      setDeleting(null)
    }
  }

  return (
    <section className="settings-section" data-testid="agent-vars-section">
      <h3>Agent Variables</h3>
      <p className="form-hint">
        Variables agents can read <strong>and write</strong> via the <code>list_variables</code>,{' '}
        <code>set_variable</code> and <code>delete_variable</code> tools. A folder-scoped variable
        shadows a global one with the same name. Don&rsquo;t store secrets here — use Environment
        Variables for secrets.
      </p>

      {error && <p className="settings-error">{error}</p>}

      {vars === null ? (
        <p className="settings-loading">Loading agent variables...</p>
      ) : vars.length === 0 ? (
        <p className="settings-loading">No agent variables yet. Add one below.</p>
      ) : (
        <ul className="env-var-list" aria-label="Agent variables">
          {vars.map((v) => (
            <li className="env-var-row" key={v.id} data-testid={`agent-var-${v.name}`}>
              <div className="env-var-main">
                <span className="env-var-name">{v.name}</span>
                <span className={`env-var-scope${v.folder_id ? '' : ' env-var-scope--global'}`}>
                  {v.folder_name ?? 'Global'}
                </span>
                <span className="env-var-value">{v.value}</span>
              </div>
              <div className="env-var-actions">
                <button
                  type="button"
                  className="btn-secondary btn-sm"
                  onClick={() => startEdit(v)}
                  data-testid={`agent-var-edit-${v.name}`}
                >
                  Edit
                </button>
                <button
                  type="button"
                  className="btn-secondary btn-sm"
                  onClick={() => void remove(v)}
                  disabled={deleting === v.id}
                  data-testid={`agent-var-delete-${v.name}`}
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
        <div className="form-field">
          <label className="form-label">Name</label>
          <input
            className="form-input"
            value={name}
            autoComplete="off"
            placeholder="my_var"
            onChange={(e) => setName(e.target.value)}
            data-testid="agent-var-name-input"
          />
        </div>
        <div className="form-field">
          <label className="form-label">Value</label>
          <input
            className="form-input"
            value={value}
            autoComplete="off"
            onChange={(e) => setValue(e.target.value)}
            data-testid="agent-var-value-input"
          />
        </div>
        <div className="form-field">
          <label className="form-label">Scope</label>
          <select
            className="form-input"
            value={folderId}
            onChange={(e) => setFolderId(e.target.value)}
            data-testid="agent-var-scope-select"
          >
            <option value="">Global</option>
            {folders.map((f) => (
              <option key={f.id} value={f.id}>
                {f.name}
              </option>
            ))}
          </select>
        </div>
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
            disabled={saving || name.trim().length === 0}
            data-testid="agent-var-save-btn"
          >
            {saving ? 'Saving…' : editing ? 'Save' : 'Add'}
          </button>
        </div>
      </form>
    </section>
  )
}

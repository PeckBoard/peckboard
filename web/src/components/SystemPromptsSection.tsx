import { useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'

interface SystemPrompt {
  id: string
  name: string
  body: string
  source_url: string | null
  created_at: string
  updated_at: string
}

type Draft = { id: string | null; name: string; body: string }

/**
 * Settings → System Prompts. Manages the named-prompt library the
 * cost-aware auto-switch picks from: list with edit/delete, a "New prompt"
 * form, and an "Import from URL" flow that fetches the body server-side
 * (through the hardened `/api/system-prompts/import` route) and shows a
 * preview before saving.
 */
export default function SystemPromptsSection() {
  const [prompts, setPrompts] = useState<SystemPrompt[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState('')
  const [draft, setDraft] = useState<Draft | null>(null)
  const [importName, setImportName] = useState('')
  const [importUrl, setImportUrl] = useState('')
  const [importing, setImporting] = useState(false)

  const load = () => {
    setLoading(true)
    authedFetch('/api/system-prompts')
      .then((res) => (res.ok ? res.json() : Promise.reject(new Error('Failed to load'))))
      .then((data: { prompts: SystemPrompt[] }) => setPrompts(data.prompts ?? []))
      .catch((e) => setError(e instanceof Error ? e.message : 'Failed to load'))
      .finally(() => setLoading(false))
  }

  // Initial fetch on mount. setState lands only in the async callbacks (not
  // synchronously in the effect), matching the codebase's fetch-in-effect style.
  useEffect(() => {
    let cancelled = false
    authedFetch('/api/system-prompts')
      .then((res) => (res.ok ? res.json() : Promise.reject(new Error('Failed to load'))))
      .then((data: { prompts: SystemPrompt[] }) => {
        if (!cancelled) {
          setPrompts(data.prompts ?? [])
          setLoading(false)
        }
      })
      .catch((e) => {
        if (!cancelled) {
          setError(e instanceof Error ? e.message : 'Failed to load')
          setLoading(false)
        }
      })
    return () => {
      cancelled = true
    }
  }, [])

  const saveDraft = async () => {
    if (!draft) return
    if (!draft.name.trim() || !draft.body.trim()) {
      setError('Name and body are required')
      return
    }
    setError('')
    try {
      const res = draft.id
        ? await authedFetch(`/api/system-prompts/${draft.id}`, {
            method: 'PUT',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ name: draft.name.trim(), body: draft.body }),
          })
        : await authedFetch('/api/system-prompts', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ name: draft.name.trim(), body: draft.body }),
          })
      if (!res.ok) {
        const err = await res.json().catch(() => ({ error: 'Failed to save' }))
        throw new Error(err.error || 'Failed to save')
      }
      setDraft(null)
      load()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to save')
    }
  }

  const remove = async (id: string) => {
    setError('')
    try {
      const res = await authedFetch(`/api/system-prompts/${id}`, { method: 'DELETE' })
      if (!res.ok) throw new Error('Failed to delete')
      load()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to delete')
    }
  }

  const doImport = async () => {
    if (!importName.trim() || !importUrl.trim()) {
      setError('Import needs a name and an https URL')
      return
    }
    setImporting(true)
    setError('')
    try {
      const res = await authedFetch('/api/system-prompts/import', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ name: importName.trim(), url: importUrl.trim() }),
      })
      if (!res.ok) {
        const err = await res.json().catch(() => ({ error: 'Import failed' }))
        throw new Error(err.error || 'Import failed')
      }
      const imported: SystemPrompt = await res.json()
      setImportName('')
      setImportUrl('')
      // Drop the user straight into an editable preview of what was fetched.
      setDraft({ id: imported.id, name: imported.name, body: imported.body })
      load()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Import failed')
    } finally {
      setImporting(false)
    }
  }

  return (
    <section className="settings-section" data-testid="system-prompts-section">
      <h3>System Prompts</h3>
      <p className="form-hint" style={{ marginTop: 0 }}>
        Named prompts that steer a session toward a kind of work. The cost-aware auto-switch applies
        a matching prompt when it downgrades a worker.
      </p>

      {error && <p className="form-error">{error}</p>}

      {loading ? (
        <p className="settings-loading">Loading system prompts...</p>
      ) : (
        <ul className="system-prompts-list" data-testid="system-prompts-list">
          {prompts.length === 0 && <li className="form-hint">No system prompts yet.</li>}
          {prompts.map((p) => (
            <li key={p.id} className="system-prompt-row" data-testid={`system-prompt-${p.name}`}>
              <div className="system-prompt-meta">
                <span className="system-prompt-name">{p.name}</span>
                {p.source_url && <span className="system-prompt-source">imported</span>}
                <span className="system-prompt-preview">
                  {p.body.slice(0, 90)}
                  {p.body.length > 90 ? '…' : ''}
                </span>
              </div>
              <div className="system-prompt-actions">
                <button
                  type="button"
                  className="btn-secondary"
                  onClick={() => setDraft({ id: p.id, name: p.name, body: p.body })}
                >
                  Edit
                </button>
                <button type="button" className="btn-secondary" onClick={() => remove(p.id)}>
                  Delete
                </button>
              </div>
            </li>
          ))}
        </ul>
      )}

      {draft ? (
        <div className="form-inline-card" data-testid="system-prompt-editor">
          <input
            className="form-input"
            placeholder="Prompt name (e.g. implement)"
            value={draft.name}
            onChange={(e) => setDraft({ ...draft, name: e.target.value })}
            data-testid="system-prompt-name-input"
          />
          <textarea
            className="form-input"
            placeholder="Prompt body..."
            rows={8}
            value={draft.body}
            onChange={(e) => setDraft({ ...draft, body: e.target.value })}
            data-testid="system-prompt-body-input"
          />
          <div className="form-actions">
            <button type="button" className="btn-secondary" onClick={() => setDraft(null)}>
              Cancel
            </button>
            <button
              type="button"
              className="btn-primary"
              onClick={saveDraft}
              data-testid="system-prompt-save"
            >
              Save
            </button>
          </div>
        </div>
      ) : (
        <button
          type="button"
          className="btn-secondary"
          onClick={() => setDraft({ id: null, name: '', body: '' })}
          data-testid="system-prompt-new"
        >
          + New prompt
        </button>
      )}

      <div className="settings-subsection">
        <h4>Import From URL</h4>
        <p className="form-hint" style={{ marginTop: 0 }}>
          Downloads the prompt body server-side over https. Re-importing a name refreshes it in
          place.
        </p>
        <div className="form-inline-card">
          <input
            className="form-input"
            placeholder="Name"
            value={importName}
            onChange={(e) => setImportName(e.target.value)}
            data-testid="system-prompt-import-name"
          />
          <input
            className="form-input"
            placeholder="https://..."
            value={importUrl}
            onChange={(e) => setImportUrl(e.target.value)}
            data-testid="system-prompt-import-url"
          />
          <button
            type="button"
            className="btn-secondary"
            onClick={doImport}
            disabled={importing || !importName.trim() || !importUrl.trim()}
            data-testid="system-prompt-import-submit"
          >
            {importing ? 'Importing...' : 'Import'}
          </button>
        </div>
      </div>
    </section>
  )
}

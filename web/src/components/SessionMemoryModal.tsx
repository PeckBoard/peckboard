import { useCallback, useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'
import { useSessionsStore } from '../store/sessions'
import ConfirmDialog from './ConfirmDialog'
import EmptyState from './EmptyState'
import FieldError from './FieldError'
import Modal from './Modal'

export interface SessionMemoryEntry {
  id: string
  session_id: string
  content: string
  created_at: string
  updated_at: string
  /** When the note is deleted; `null` for older notes kept without expiry. */
  expires_at: string | null
}

export interface FolderMemoryEntry {
  id: string
  folder_id: string
  category: string
  title: string
  content: string
  source_session_id: string | null
  created_at: string
  updated_at: string
}

/** Display labels for the long-term categories, in the server's order. */
const FOLDER_MEMORY_CATEGORIES: { id: string; label: string }[] = [
  { id: 'architecture', label: 'Architecture' },
  { id: 'decision', label: 'Design decisions' },
  { id: 'structure', label: 'Code structure' },
  { id: 'layout', label: 'Project layout' },
  { id: 'convention', label: 'Conventions' },
]

const TITLE_MAX = 120
const CONTENT_MAX = 4000

/** "5h", "20m", "3d": time left until `expiresAt`, rounded down. */
function timeLeft(expiresAt: string, now: number): string {
  const ms = new Date(expiresAt).getTime() - now
  const minutes = Math.max(1, Math.floor(ms / 60_000))
  if (minutes < 60) return `${minutes}m`
  const hours = Math.floor(minutes / 60)
  if (hours < 48) return `${hours}h`
  return `${Math.floor(hours / 24)}d`
}

/** A short-term note is fading once half its life since the last write has passed. */
function isFading(m: SessionMemoryEntry, now: number): boolean {
  if (!m.expires_at) return false
  const updated = new Date(m.updated_at).getTime()
  const expires = new Date(m.expires_at).getTime()
  return now >= updated + (expires - updated) / 2
}

interface Props {
  sessionId: string
  onClose: () => void
}

type Tab = 'session' | 'folder'

/**
 * The agent's memory, in two tabs. "This session": the session's short-term
 * notes (deleted a set time after they were last written) plus older notes
 * kept without expiry; read + delete only, the agent authors them.
 * "Long-term (folder)": notes shared by every session in the session's
 * folder, grouped by category; the user can add, edit and delete them.
 */
export default function SessionMemoryModal({ sessionId, onClose }: Props) {
  const [tab, setTab] = useState<Tab>('session')
  const storeFolderId = useSessionsStore(
    (s) => s.sessions.find((x) => x.id === sessionId)?.folder_id,
  )
  const [fetchedFolderId, setFetchedFolderId] = useState<string | null>(null)
  const folderId = storeFolderId || fetchedFolderId

  // Sessions not in the list store (e.g. workers) still have a folder:
  // ask the server once.
  useEffect(() => {
    if (storeFolderId) return
    let cancelled = false
    authedFetch(`/api/sessions/${sessionId}`)
      .then((res) => (res.ok ? (res.json() as Promise<{ folder_id?: string }>) : null))
      .then((s) => {
        if (!cancelled && s?.folder_id) setFetchedFolderId(s.folder_id)
      })
      .catch(() => {})
    return () => {
      cancelled = true
    }
  }, [sessionId, storeFolderId])

  return (
    <Modal onClose={onClose} maxWidth={600} data-testid="session-memory-modal">
      <h2>Memory</h2>
      <div className="registry-tabs memory-tabs" role="tablist">
        <button
          type="button"
          role="tab"
          aria-selected={tab === 'session'}
          className={`registry-tab ${tab === 'session' ? 'registry-tab--active' : ''}`}
          data-testid="memory-tab-session"
          onClick={() => setTab('session')}
        >
          This session
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={tab === 'folder'}
          className={`registry-tab ${tab === 'folder' ? 'registry-tab--active' : ''}`}
          data-testid="memory-tab-folder"
          onClick={() => setTab('folder')}
        >
          Long-term (folder)
        </button>
      </div>
      {tab === 'session' && <SessionNotes sessionId={sessionId} />}
      {tab === 'folder' &&
        (folderId ? (
          <FolderNotes folderId={folderId} />
        ) : (
          <p className="session-memory-empty">Loading…</p>
        ))}
      <div className="form-actions">
        <button type="button" className="btn-secondary" onClick={onClose}>
          Close
        </button>
      </div>
    </Modal>
  )
}

function SessionNotes({ sessionId }: { sessionId: string }) {
  const [entries, setEntries] = useState<SessionMemoryEntry[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [pendingDelete, setPendingDelete] = useState<SessionMemoryEntry | null>(null)
  const [deleteBusy, setDeleteBusy] = useState(false)
  const [deleteError, setDeleteError] = useState<string | null>(null)
  const [now, setNow] = useState(() => Date.now())

  // Promise chain rather than async/await: every setState lands in a
  // callback, so the effect below never sets state synchronously.
  const load = useCallback(
    () =>
      authedFetch(`/api/sessions/${sessionId}/memories`)
        .then((res) => {
          if (!res.ok) throw new Error(`memories fetch failed: ${res.status}`)
          return res.json() as Promise<{ memories?: SessionMemoryEntry[] }>
        })
        .then((data) => {
          setEntries(data.memories ?? [])
          setNow(Date.now())
          setError(null)
        })
        .catch(() => setError("Couldn't load this session's memory.")),
    [sessionId],
  )

  useEffect(() => {
    void load()
  }, [load])

  // Keep "expires in …" current while the dialog stays open.
  useEffect(() => {
    const t = window.setInterval(() => setNow(Date.now()), 60_000)
    return () => window.clearInterval(t)
  }, [])

  // The agent may add or compact entries while the modal is open; the
  // server broadcasts `session-memory` on every change.
  useEffect(() => {
    const onChange = (e: CustomEvent<{ sessionId: string }>) => {
      if (e.detail?.sessionId === sessionId) void load()
    }
    window.addEventListener('peckboard:session-memory', onChange as EventListener)
    return () => {
      window.removeEventListener('peckboard:session-memory', onChange as EventListener)
    }
  }, [sessionId, load])

  const confirmDelete = async () => {
    if (!pendingDelete || deleteBusy) return
    setDeleteBusy(true)
    setDeleteError(null)
    try {
      const res = await authedFetch(`/api/sessions/${sessionId}/memories/${pendingDelete.id}`, {
        method: 'DELETE',
      })
      if (!res.ok && res.status !== 404) throw new Error(`delete failed: ${res.status}`)
      setEntries((prev) => (prev ? prev.filter((m) => m.id !== pendingDelete.id) : prev))
      setPendingDelete(null)
    } catch {
      setDeleteError("Couldn't delete this memory. Please try again.")
    } finally {
      setDeleteBusy(false)
    }
  }

  const shortTerm = (entries ?? []).filter((m) => m.expires_at)
  const kept = (entries ?? []).filter((m) => !m.expires_at)

  const renderRow = (m: SessionMemoryEntry) => {
    const fading = isFading(m, now)
    return (
      <li key={m.id} className="session-memory-row" data-testid="session-memory-row">
        <div className="session-memory-content">{m.content}</div>
        <div className="session-memory-meta">
          <span className="session-memory-date-group">
            <span
              className="session-memory-date"
              title={`Saved ${new Date(m.created_at).toLocaleString()}`}
            >
              {new Date(m.created_at).toLocaleDateString()}
            </span>
            {m.expires_at && (
              <span
                className="session-memory-expiry"
                title={`Deleted ${new Date(m.expires_at).toLocaleString()}`}
                data-testid="session-memory-expiry"
              >
                expires in {timeLeft(m.expires_at, now)}
              </span>
            )}
            {fading && (
              <span
                className="session-memory-fading"
                title="Over half its time has passed. The agent now sees only a one-line mention of it."
                data-testid="session-memory-fading"
              >
                Fading
              </span>
            )}
          </span>
          <button
            type="button"
            className="btn-secondary session-memory-delete"
            onClick={() => {
              setDeleteError(null)
              setPendingDelete(m)
            }}
            aria-label="Delete memory"
            data-testid="session-memory-delete"
          >
            Delete
          </button>
        </div>
      </li>
    )
  }

  return (
    <div data-testid="memory-panel-session">
      <p className="session-memory-intro">
        Notes the agent keeps about the current work. They survive clearing the session and are
        deleted a set time after they were last written.
      </p>
      {error && (
        <div className="fetch-error-pane" role="alert" data-testid="session-memory-error">
          <p>{error}</p>
          <button type="button" onClick={() => void load()}>
            Retry
          </button>
        </div>
      )}
      {!error && entries === null && <p className="session-memory-empty">Loading…</p>}
      {!error && entries !== null && entries.length === 0 && (
        <p className="session-memory-empty" data-testid="session-memory-empty">
          Nothing remembered yet. The agent adds notes with its memory tools as it works.
        </p>
      )}
      {!error && entries !== null && entries.length > 0 && (
        <div className="session-memory-scroll" data-testid="session-memory-list">
          {shortTerm.length > 0 && (
            <ul className="session-memory-list">{shortTerm.map(renderRow)}</ul>
          )}
          {kept.length > 0 && (
            <>
              <h3 className="session-memory-group" data-testid="session-memory-kept-heading">
                Kept notes (no expiry)
              </h3>
              <ul className="session-memory-list" data-testid="session-memory-kept">
                {kept.map(renderRow)}
              </ul>
            </>
          )}
        </div>
      )}
      {pendingDelete && (
        <ConfirmDialog
          title="Delete memory"
          message="Forget this note? The agent will no longer see it after its next restart."
          confirmLabel="Delete"
          cancelLabel="Cancel"
          danger
          busy={deleteBusy}
          error={deleteError}
          testId="confirm-delete-memory"
          onConfirm={() => void confirmDelete()}
          onCancel={() => {
            if (deleteBusy) return
            setPendingDelete(null)
            setDeleteError(null)
          }}
        />
      )}
    </div>
  )
}

/** The note being added (`id: null`) or edited. */
interface Draft {
  id: string | null
  category: string
  title: string
  content: string
}

function FolderNotes({ folderId }: { folderId: string }) {
  const [entries, setEntries] = useState<FolderMemoryEntry[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [draft, setDraft] = useState<Draft | null>(null)
  const [touched, setTouched] = useState(false)
  const [saving, setSaving] = useState(false)
  const [saveError, setSaveError] = useState<string | null>(null)
  const [pendingDelete, setPendingDelete] = useState<FolderMemoryEntry | null>(null)
  const [deleteBusy, setDeleteBusy] = useState(false)
  const [deleteError, setDeleteError] = useState<string | null>(null)

  const load = useCallback(
    () =>
      authedFetch(`/api/folders/${folderId}/memories`)
        .then((res) => {
          if (!res.ok) throw new Error(`folder memories fetch failed: ${res.status}`)
          return res.json() as Promise<{ memories?: FolderMemoryEntry[] }>
        })
        .then((data) => {
          setEntries(data.memories ?? [])
          setError(null)
        })
        .catch(() => setError("Couldn't load this folder's long-term memory.")),
    [folderId],
  )

  useEffect(() => {
    void load()
  }, [load])

  useEffect(() => {
    const onChange = (e: CustomEvent<{ folderId: string }>) => {
      if (e.detail?.folderId === folderId) void load()
    }
    window.addEventListener('peckboard:folder-memory', onChange as EventListener)
    return () => {
      window.removeEventListener('peckboard:folder-memory', onChange as EventListener)
    }
  }, [folderId, load])

  const startAdd = () => {
    setSaveError(null)
    setTouched(false)
    setDraft({ id: null, category: FOLDER_MEMORY_CATEGORIES[0].id, title: '', content: '' })
  }

  const titleError = !draft?.title.trim()
    ? 'Give the note a title.'
    : draft.title.length > TITLE_MAX
      ? `Keep the title under ${TITLE_MAX} characters.`
      : undefined
  const contentError = !draft?.content.trim()
    ? 'Write what the note should say.'
    : draft.content.length > CONTENT_MAX
      ? `Keep the note under ${CONTENT_MAX} characters.`
      : undefined
  const invalid = !!titleError || !!contentError

  const save = async () => {
    if (!draft || saving) return
    setTouched(true)
    if (invalid) return
    setSaving(true)
    setSaveError(null)
    try {
      const url = draft.id
        ? `/api/folders/${folderId}/memories/${draft.id}`
        : `/api/folders/${folderId}/memories`
      const res = await authedFetch(url, {
        method: draft.id ? 'PATCH' : 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          category: draft.category,
          title: draft.title.trim(),
          content: draft.content.trim(),
        }),
      })
      if (!res.ok) {
        const body = (await res.json().catch(() => null)) as { error?: string } | null
        throw new Error(body?.error || `save failed: ${res.status}`)
      }
      setDraft(null)
      await load()
    } catch (e) {
      setSaveError(
        e instanceof Error && !e.message.startsWith('save failed')
          ? `Couldn't save the note: ${e.message}`
          : "Couldn't save the note. Please try again.",
      )
    } finally {
      setSaving(false)
    }
  }

  const confirmDelete = async () => {
    if (!pendingDelete || deleteBusy) return
    setDeleteBusy(true)
    setDeleteError(null)
    try {
      const res = await authedFetch(`/api/folders/${folderId}/memories/${pendingDelete.id}`, {
        method: 'DELETE',
      })
      if (!res.ok && res.status !== 404) throw new Error(`delete failed: ${res.status}`)
      setPendingDelete(null)
      await load()
    } catch {
      setDeleteError("Couldn't delete this note. Please try again.")
    } finally {
      setDeleteBusy(false)
    }
  }

  const editor = draft && (
    <form
      className="folder-memory-editor"
      data-testid="folder-memory-editor"
      onSubmit={(e) => {
        e.preventDefault()
        void save()
      }}
    >
      <div className="form-field">
        <label className="form-label" htmlFor="folder-memory-category">
          Kind of note
        </label>
        <select
          id="folder-memory-category"
          className="form-input"
          value={draft.category}
          onChange={(e) => setDraft({ ...draft, category: e.target.value })}
          data-testid="folder-memory-category"
        >
          {FOLDER_MEMORY_CATEGORIES.map((c) => (
            <option key={c.id} value={c.id}>
              {c.label}
            </option>
          ))}
        </select>
      </div>
      <div className="form-field">
        <label className="form-label" htmlFor="folder-memory-title">
          Title
        </label>
        <input
          id="folder-memory-title"
          className="form-input"
          value={draft.title}
          onChange={(e) => setDraft({ ...draft, title: e.target.value })}
          maxLength={TITLE_MAX}
          autoFocus
          data-testid="folder-memory-title"
        />
        <FieldError message={touched ? titleError : undefined} testId="folder-memory-title-error" />
      </div>
      <div className="form-field">
        <label className="form-label" htmlFor="folder-memory-content">
          Note
        </label>
        <textarea
          id="folder-memory-content"
          className="form-input"
          value={draft.content}
          onChange={(e) => setDraft({ ...draft, content: e.target.value })}
          rows={4}
          style={{ resize: 'vertical' }}
          maxLength={CONTENT_MAX}
          data-testid="folder-memory-content"
        />
        <FieldError
          message={touched ? contentError : undefined}
          testId="folder-memory-content-error"
        />
      </div>
      {saveError && (
        <p className="form-error" role="alert" data-testid="folder-memory-save-error">
          {saveError}
        </p>
      )}
      <div className="folder-memory-editor-actions">
        <button
          type="button"
          className="btn-secondary"
          onClick={() => setDraft(null)}
          disabled={saving}
        >
          Cancel
        </button>
        <button
          type="submit"
          className="btn-primary"
          disabled={saving}
          data-testid="folder-memory-save"
        >
          {saving ? 'Saving…' : draft.id ? 'Save changes' : 'Add note'}
        </button>
      </div>
    </form>
  )

  const groups = FOLDER_MEMORY_CATEGORIES.map((c) => ({
    ...c,
    notes: (entries ?? []).filter((m) => m.category === c.id),
  })).filter((g) => g.notes.length > 0)
  // Notes in a category this build doesn't know yet still show, last.
  const known = new Set(FOLDER_MEMORY_CATEGORIES.map((c) => c.id))
  const other = (entries ?? []).filter((m) => !known.has(m.category))
  if (other.length > 0) groups.push({ id: 'other', label: 'Other', notes: other })

  return (
    <div data-testid="memory-panel-folder">
      <p className="session-memory-intro">
        Shared by every session in this folder. These notes never expire.
      </p>
      {error && (
        <div className="fetch-error-pane" role="alert" data-testid="folder-memory-error">
          <p>{error}</p>
          <button type="button" onClick={() => void load()}>
            Retry
          </button>
        </div>
      )}
      {!error && entries === null && <p className="session-memory-empty">Loading…</p>}
      {draft && !draft.id && editor}
      {!error && entries !== null && entries.length === 0 && !draft && (
        <EmptyState
          title="No long-term notes yet"
          body="Agents save facts about the project here, like how it is built and the choices behind it. You can add your own too."
          actionLabel="Add a note"
          onAction={startAdd}
          actionTestId="folder-memory-add-empty"
          term="folder"
          testId="folder-memory-empty"
        />
      )}
      {!error && entries !== null && entries.length > 0 && (
        <>
          {!draft && (
            <div className="folder-memory-toolbar">
              <button
                type="button"
                className="btn-secondary"
                onClick={startAdd}
                data-testid="folder-memory-add"
              >
                Add a note
              </button>
            </div>
          )}
          <div className="session-memory-scroll" data-testid="folder-memory-list">
            {groups.map((g) => (
              <section key={g.id} data-testid={`folder-memory-group-${g.id}`}>
                <h3 className="session-memory-group">{g.label}</h3>
                <ul className="session-memory-list">
                  {g.notes.map((m) =>
                    draft?.id === m.id ? (
                      <li key={m.id} className="session-memory-row">
                        {editor}
                      </li>
                    ) : (
                      <li key={m.id} className="session-memory-row" data-testid="folder-memory-row">
                        <div className="folder-memory-title">{m.title}</div>
                        <div className="session-memory-content">{m.content}</div>
                        <div className="session-memory-meta">
                          <span
                            className="session-memory-date"
                            title={`Last changed ${new Date(m.updated_at).toLocaleString()}`}
                          >
                            {new Date(m.updated_at).toLocaleDateString()}
                          </span>
                          <span className="folder-memory-row-actions">
                            <button
                              type="button"
                              className="btn-secondary session-memory-delete"
                              onClick={() => {
                                setSaveError(null)
                                setTouched(false)
                                setDraft({
                                  id: m.id,
                                  category: m.category,
                                  title: m.title,
                                  content: m.content,
                                })
                              }}
                              aria-label={`Edit ${m.title}`}
                              data-testid="folder-memory-edit"
                            >
                              Edit
                            </button>
                            <button
                              type="button"
                              className="btn-secondary session-memory-delete"
                              onClick={() => {
                                setDeleteError(null)
                                setPendingDelete(m)
                              }}
                              aria-label={`Delete ${m.title}`}
                              data-testid="folder-memory-delete"
                            >
                              Delete
                            </button>
                          </span>
                        </div>
                      </li>
                    ),
                  )}
                </ul>
              </section>
            ))}
          </div>
        </>
      )}
      {pendingDelete && (
        <ConfirmDialog
          title="Delete long-term note"
          message={`Delete "${pendingDelete.title}"? Every session in this folder will stop seeing it.`}
          confirmLabel="Delete"
          cancelLabel="Cancel"
          danger
          busy={deleteBusy}
          error={deleteError}
          testId="confirm-delete-folder-memory"
          onConfirm={() => void confirmDelete()}
          onCancel={() => {
            if (deleteBusy) return
            setPendingDelete(null)
            setDeleteError(null)
          }}
        />
      )}
    </div>
  )
}

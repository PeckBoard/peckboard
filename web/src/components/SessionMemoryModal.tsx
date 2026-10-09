import { useCallback, useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'
import ConfirmDialog from './ConfirmDialog'
import Modal from './Modal'

export interface SessionMemoryEntry {
  id: string
  session_id: string
  content: string
  created_at: string
  updated_at: string
}

interface Props {
  sessionId: string
  onClose: () => void
}

/**
 * The session's durable memory pool — the notes the agent saved through the
 * `memory_*` MCP tools. They survive "Clear session" and context compaction
 * and are injected into the agent's system prompt on every fresh spawn, so
 * this is where the user sees (and prunes) what the agent will keep
 * believing. Read + delete only: the agent authors its own memory.
 */
export default function SessionMemoryModal({ sessionId, onClose }: Props) {
  const [entries, setEntries] = useState<SessionMemoryEntry[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [pendingDelete, setPendingDelete] = useState<SessionMemoryEntry | null>(null)
  const [deleteBusy, setDeleteBusy] = useState(false)
  const [deleteError, setDeleteError] = useState<string | null>(null)

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
          setError(null)
        })
        .catch(() => setError("Couldn't load this session's memory.")),
    [sessionId],
  )

  useEffect(() => {
    void load()
  }, [load])

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

  return (
    <Modal onClose={onClose} maxWidth={560} data-testid="session-memory-modal">
      <h2>Session memory</h2>
      <p className="session-memory-intro">
        Notes the agent chose to keep. They survive clearing the session and context compaction, and
        are shown to the agent whenever it starts fresh.
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
          Nothing remembered yet. The agent adds entries with its memory tools as it learns durable
          facts about this session.
        </p>
      )}
      {!error && entries !== null && entries.length > 0 && (
        <ul className="session-memory-list" data-testid="session-memory-list">
          {entries.map((m) => (
            <li key={m.id} className="session-memory-row" data-testid="session-memory-row">
              <div className="session-memory-content">{m.content}</div>
              <div className="session-memory-meta">
                <span
                  className="session-memory-date"
                  title={`Saved ${new Date(m.created_at).toLocaleString()}`}
                >
                  {new Date(m.created_at).toLocaleDateString()}
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
          ))}
        </ul>
      )}
      <div className="form-actions">
        <button type="button" className="btn-secondary" onClick={onClose}>
          Close
        </button>
      </div>
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
    </Modal>
  )
}

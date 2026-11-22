import { useEffect, useState } from 'react'
import { useDemoProjectStore } from '../store/demoProject'
import ConfirmDialog from './ConfirmDialog'
import '../styles/demoProject.css'

/**
 * "See a demo project": starts (or reopens) the scripted demo board, whose
 * helpers need no AI account, and deletes it when the user is done. Used on
 * the Projects page empty state and the Welcome page.
 */
export default function DemoProjectCard() {
  const demo = useDemoProjectStore((s) => s.demo)
  const loaded = useDemoProjectStore((s) => s.loaded)
  const fetchDemo = useDemoProjectStore((s) => s.fetchDemo)
  const startDemo = useDemoProjectStore((s) => s.startDemo)
  const deleteDemo = useDemoProjectStore((s) => s.deleteDemo)

  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [deleteError, setDeleteError] = useState<string | null>(null)

  useEffect(() => {
    fetchDemo()
  }, [fetchDemo])

  // Both "Start demo" and "Open demo" go through POST, which returns the
  // existing demo when there is one — so a stale `demo` can't strand the user.
  const open = async () => {
    setBusy(true)
    setError(null)
    try {
      await startDemo()
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Couldn’t start the demo.')
    } finally {
      setBusy(false)
    }
  }

  const remove = async () => {
    setBusy(true)
    setDeleteError(null)
    try {
      await deleteDemo()
      setConfirmDelete(false)
    } catch (e) {
      setDeleteError(e instanceof Error ? e.message : 'Couldn’t delete the demo.')
    } finally {
      setBusy(false)
    }
  }

  return (
    <section
      className="demo-project-card"
      aria-labelledby="demo-project-card-title"
      data-testid="demo-project-card"
    >
      <h2 id="demo-project-card-title" className="demo-project-card-title">
        See a demo project
      </h2>
      <p className="demo-project-card-body">
        Watch AI helpers work through a small board of tasks — no AI account needed.
      </p>
      <div className="demo-project-card-actions">
        {demo ? (
          <>
            <button
              type="button"
              className="btn-primary btn-sm"
              onClick={open}
              disabled={busy}
              data-testid="demo-project-open"
            >
              Open demo
            </button>
            <button
              type="button"
              className="btn-secondary btn-sm"
              onClick={() => setConfirmDelete(true)}
              disabled={busy}
              data-testid="demo-project-delete"
            >
              Delete demo
            </button>
          </>
        ) : (
          <button
            type="button"
            className="btn-primary btn-sm"
            onClick={open}
            disabled={busy || !loaded}
            data-testid="demo-project-start"
          >
            {busy ? 'Starting…' : 'Start demo'}
          </button>
        )}
      </div>
      {error && (
        <p className="demo-project-card-error" role="alert">
          {error}
        </p>
      )}
      {confirmDelete && (
        <ConfirmDialog
          title="Delete demo"
          message="Delete the demo project and its cards? Your own projects are not touched."
          confirmLabel="Delete demo"
          cancelLabel="Cancel"
          danger
          busy={busy}
          busyLabel="Deleting…"
          error={deleteError}
          testId="demo-project-delete-dialog"
          onConfirm={remove}
          onCancel={() => {
            setConfirmDelete(false)
            setDeleteError(null)
          }}
        />
      )}
    </section>
  )
}

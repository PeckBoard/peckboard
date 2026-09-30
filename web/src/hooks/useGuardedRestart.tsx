import { useCallback, useState, type ReactNode } from 'react'
import RestartConfirm from '../components/RestartConfirm'
import {
  fetchRestartActivity,
  requestRestart,
  type RestartActivity,
  type RestartKind,
} from '../store/restart'

/**
 * Guard for a server restart (plain or update-and-restart). `request()`
 * first asks the server what is still running: nothing → restart straight
 * away; otherwise render the returned `dialog` (`RestartConfirm`), which
 * lists the work that would be interrupted. `onRestarting` fires once an
 * immediate restart was accepted.
 */
export default function useGuardedRestart(
  kind: RestartKind,
  opts: { version?: string | null; onRestarting: () => void },
): { request: () => Promise<void>; dialog: ReactNode; busy: boolean; error: string | null } {
  const { version, onRestarting } = opts
  const [activity, setActivity] = useState<RestartActivity | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const perform = useCallback(
    async (when: 'now' | 'idle') => {
      setBusy(true)
      setError(null)
      try {
        await requestRestart(kind, when)
        setActivity(null)
        if (when === 'now') onRestarting()
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e))
      } finally {
        setBusy(false)
      }
    },
    [kind, onRestarting],
  )

  const request = useCallback(async () => {
    setBusy(true)
    setError(null)
    let current: RestartActivity
    try {
      current = await fetchRestartActivity()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
      setBusy(false)
      return
    }
    setBusy(false)
    if (current.total === 0) await perform('now')
    else setActivity(current)
  }, [perform])

  const dialog = activity ? (
    <RestartConfirm
      kind={kind}
      version={version}
      activity={activity}
      busy={busy}
      error={error}
      onNow={() => void perform('now')}
      onIdle={() => void perform('idle')}
      onCancel={() => {
        setActivity(null)
        setError(null)
      }}
    />
  ) : null

  // Errors while the dialog is open render inside it; otherwise the caller shows them.
  return { request, dialog, busy, error: activity ? null : error }
}

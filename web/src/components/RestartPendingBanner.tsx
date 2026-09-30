import { useEffect, useState } from 'react'
import { useAuthStore } from '../store/auth'
import { useRestartStore, waitForServerThenReload } from '../store/restart'

/**
 * App-wide banner for a restart parked until the server is idle ("Restart
 * when idle"). Fed by the global `restart-pending` WS event, so every open
 * client sees it; admins get a Cancel. Once the server announces the
 * restart itself, the banner waits for it to come back and reloads.
 */
export default function RestartPendingBanner() {
  const isAdmin = useAuthStore((s) => s.user?.role === 'admin')
  const pending = useRestartStore((s) => s.pending)
  const restarting = useRestartStore((s) => s.restarting)
  const refresh = useRestartStore((s) => s.refresh)
  const cancel = useRestartStore((s) => s.cancel)
  const [error, setError] = useState<string | null>(null)
  const [cancelling, setCancelling] = useState(false)

  // A pending restart requested before this page loaded (or while its WS
  // was down) isn't replayed over the socket; ask once.
  useEffect(() => {
    if (isAdmin) void refresh()
  }, [isAdmin, refresh])

  useEffect(() => {
    if (restarting) void waitForServerThenReload()
  }, [restarting])

  if (!pending && !restarting) return null

  const what =
    pending?.kind === 'update' ? `Upgrade to ${pending.version ?? 'the latest release'}` : 'Restart'
  const n = pending?.remaining ?? 0

  return (
    <div
      className="announcement-banner restart-pending-banner"
      role="status"
      data-testid="restart-pending-banner"
    >
      <div className="announcement-content">
        {restarting ? (
          <strong>Restarting Peckboard… this page reloads when it’s back.</strong>
        ) : (
          <>
            <strong>{what} pending</strong>
            <span data-testid="restart-pending-remaining">
              {n > 0
                ? `Waiting for ${n} running ${n === 1 ? 'item' : 'items'} to finish.`
                : 'Waiting for running work to finish.'}
            </span>
            {error && <span className="settings-error">{error}</span>}
          </>
        )}
      </div>
      {isAdmin && !restarting && (
        <button
          className="announcement-dismiss"
          type="button"
          disabled={cancelling}
          data-testid="restart-pending-cancel"
          onClick={async () => {
            setCancelling(true)
            setError(null)
            try {
              await cancel()
            } catch (e) {
              setError(e instanceof Error ? e.message : String(e))
            } finally {
              setCancelling(false)
            }
          }}
        >
          Cancel restart
        </button>
      )}
    </div>
  )
}

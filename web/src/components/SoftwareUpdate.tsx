import { useCallback, useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'
import { waitForServerThenReload } from '../store/restart'
import useGuardedRestart from '../hooks/useGuardedRestart'

/** Mirrors the backend `UpdateStatus` from `/api/update/check`. */
interface UpdateStatus {
  current_version: string
  latest_version: string | null
  update_available: boolean
  supported: boolean
  asset: string | null
  notes: string | null
  html_url: string | null
}

/**
 * "Software Update" settings section. Checks `/api/update/check` for a newer
 * PeckBoard release and, when one exists, offers a one-click "Upgrade &
 * restart" that POSTs `/api/update/apply`; "Restart server" re-execs the
 * current binary. Both go through `useGuardedRestart`: with work still
 * running the user first sees what would be interrupted and can restart
 * anyway or once idle. After an immediate restart we poll until the server
 * is back, then reload to pick up the (new) embedded frontend.
 */
export default function SoftwareUpdate() {
  const [status, setStatus] = useState<UpdateStatus | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [restarting, setRestarting] = useState<'update' | 'restart' | null>(null)

  const check = useCallback(async () => {
    setLoading(true)
    setError(null)
    try {
      const res = await authedFetch('/api/update/check')
      const data = await res.json()
      if (!res.ok) throw new Error(data?.error || `HTTP ${res.status}`)
      setStatus(data as UpdateStatus)
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setLoading(false)
    }
  }, [])

  // Initial check on mount. setState lands only in the async callbacks (not
  // synchronously in the effect), matching the codebase's fetch-in-effect style.
  useEffect(() => {
    let cancelled = false
    authedFetch('/api/update/check')
      .then((res) => res.json().then((data) => ({ ok: res.ok, statusCode: res.status, data })))
      .then(({ ok, statusCode, data }) => {
        if (cancelled) return
        if (!ok) setError(data?.error || `HTTP ${statusCode}`)
        else setStatus(data as UpdateStatus)
        setLoading(false)
      })
      .catch((e) => {
        if (cancelled) return
        setError(e instanceof Error ? e.message : String(e))
        setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [])

  // The server re-execs and is briefly unreachable; reload once it's back.
  const afterRestart = useCallback(async () => {
    if (await waitForServerThenReload()) return
    setRestarting(null)
    setError('The server is taking longer than expected to restart. Reload the page to check.')
  }, [])

  const onUpdateRestarting = useCallback(() => {
    setRestarting('update')
    void afterRestart()
  }, [afterRestart])
  const onRestarting = useCallback(() => {
    setRestarting('restart')
    void afterRestart()
  }, [afterRestart])

  const upgrade = useGuardedRestart('update', {
    version: status?.latest_version,
    onRestarting: onUpdateRestarting,
  })
  const restart = useGuardedRestart('restart', { onRestarting })
  const busy = upgrade.busy || restart.busy
  const actionError = upgrade.error ?? restart.error

  return (
    <section className="settings-section" data-testid="settings-update">
      <h3>Software Update</h3>

      <div className="settings-info-grid">
        <div className="settings-row">
          <span className="settings-label">Current Version</span>
          <span data-testid="update-current-version">{status?.current_version ?? '…'}</span>
        </div>
        {status?.latest_version && (
          <div className="settings-row">
            <span className="settings-label">Latest Release</span>
            <span>{status.latest_version}</span>
          </div>
        )}
      </div>

      {restarting ? (
        <p className="settings-loading" data-testid="update-restarting">
          {restarting === 'update' ? 'Upgrading and restarting…' : 'Restarting…'} this page will
          reload automatically.
        </p>
      ) : (
        <>
          {loading ? (
            <p className="settings-loading">Checking for updates…</p>
          ) : error ? (
            <div className="settings-update-actions">
              <p className="settings-error">{error}</p>
              <button type="button" className="btn-secondary" onClick={() => void check()}>
                Try again
              </button>
            </div>
          ) : status && !status.supported ? (
            <p className="settings-loading">Self-update isn’t supported on this platform.</p>
          ) : status?.update_available ? (
            <div className="settings-update-actions">
              <p data-testid="update-available">
                Update available — <strong>{status.latest_version}</strong>
              </p>
              {status.html_url && (
                <a
                  href={status.html_url}
                  target="_blank"
                  rel="noreferrer"
                  className="settings-link"
                >
                  Release notes
                </a>
              )}
              <button
                type="button"
                className="btn-primary"
                onClick={() => void upgrade.request()}
                disabled={busy}
                data-testid="update-apply"
              >
                {upgrade.busy ? 'Starting…' : 'Upgrade & restart'}
              </button>
            </div>
          ) : (
            <div className="settings-update-actions">
              <p data-testid="update-uptodate">You’re on the latest version.</p>
              <button type="button" className="btn-secondary" onClick={() => void check()}>
                Check again
              </button>
            </div>
          )}
          <div className="settings-update-actions">
            <button
              type="button"
              className="btn-secondary"
              onClick={() => void restart.request()}
              disabled={busy}
              data-testid="server-restart"
            >
              {restart.busy ? 'Checking…' : 'Restart server'}
            </button>
          </div>
          {actionError && (
            <p className="settings-error" role="alert" data-testid="restart-error">
              {actionError}
            </p>
          )}
        </>
      )}
      {upgrade.dialog}
      {restart.dialog}
    </section>
  )
}

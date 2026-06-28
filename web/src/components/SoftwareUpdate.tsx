import { useCallback, useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'

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
 * restart" that POSTs `/api/update/apply`. The backend swaps its binary and
 * re-execs (same port), so after applying we poll until the server is back and
 * then reload to pick up the new embedded frontend.
 */
export default function SoftwareUpdate() {
  const [status, setStatus] = useState<UpdateStatus | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [applying, setApplying] = useState(false)
  const [restarting, setRestarting] = useState(false)

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

  // After applying, the server re-execs and is briefly unreachable. Poll the
  // check endpoint until it answers again, then reload to load the new bundle.
  const waitForRestartThenReload = useCallback(async () => {
    for (let i = 0; i < 60; i++) {
      await new Promise((r) => setTimeout(r, 2000))
      try {
        const res = await authedFetch('/api/update/check')
        if (res.ok) {
          window.location.reload()
          return
        }
      } catch {
        // server still restarting — keep polling
      }
    }
    // Gave up waiting; let the user reload manually.
    setRestarting(false)
    setError('The server is taking longer than expected to restart. Reload the page to check.')
  }, [])

  const apply = useCallback(async () => {
    setApplying(true)
    setError(null)
    try {
      const res = await authedFetch('/api/update/apply', { method: 'POST' })
      const data = await res.json().catch(() => ({}))
      if (!res.ok) throw new Error(data?.error || `HTTP ${res.status}`)
      setRestarting(true)
      void waitForRestartThenReload()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setApplying(false)
    }
  }, [waitForRestartThenReload])

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
          Upgrading and restarting… this page will reload automatically.
        </p>
      ) : loading ? (
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
            <a href={status.html_url} target="_blank" rel="noreferrer" className="settings-link">
              Release notes
            </a>
          )}
          <button
            type="button"
            className="btn-primary"
            onClick={() => void apply()}
            disabled={applying}
            data-testid="update-apply"
          >
            {applying ? 'Starting…' : 'Upgrade & restart'}
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
    </section>
  )
}

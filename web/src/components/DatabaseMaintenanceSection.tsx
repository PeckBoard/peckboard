import { useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'
import ConfirmDialog from './ConfirmDialog'

interface TableRows {
  name: string
  rows: number
}

/** `GET /api/settings/database/stats`. */
interface Stats {
  file_bytes: number
  reclaimable_bytes: number
  tables: TableRows[]
}

interface CleanupItem {
  key: string
  label: string
  rows: number
}

/** `POST /api/settings/database/cleanup[/preview]`. */
interface CleanupReport {
  items: CleanupItem[]
  total: number
}

interface CompactReport {
  before_bytes: number
  after_bytes: number
}

interface CheckReport {
  ok: boolean
  problems: string[]
}

interface SlowOp {
  site: string
  count: number
  total_run_ms: number
  max_run_ms: number
  max_wait_ms: number
}

/** `GET /api/settings/database/slow`. */
interface SlowReport {
  threshold_ms: number
  since: number
  operations: SlowOp[]
}

/** How many slow operations the readout lists. */
const TOP_SLOW = 8

function formatMs(ms: number): string {
  return ms >= 1000 ? `${(ms / 1000).toFixed(1)} s` : `${ms} ms`
}

/** How many of the largest tables the readout lists. */
const TOP_TABLES = 8

function formatBytes(bytes: number): string {
  if (bytes >= 1e9) return `${(bytes / 1e9).toFixed(1)} GB`
  if (bytes >= 1e6) return `${(bytes / 1e6).toFixed(1)} MB`
  return `${Math.ceil(bytes / 1e3)} KB`
}

function rowCount(n: number): string {
  return `${n.toLocaleString()} ${n === 1 ? 'row' : 'rows'}`
}

async function errorText(res: Response, fallback: string): Promise<string> {
  try {
    const body = (await res.json()) as { error?: string }
    if (body.error) return body.error
  } catch {
    // no JSON body
  }
  return fallback
}

/** The size readout, or `null` when it cannot be read. It is informational:
 *  the actions still work without it. */
async function fetchStats(): Promise<Stats | null> {
  try {
    const res = await authedFetch('/api/settings/database/stats')
    return res.ok ? ((await res.json()) as Stats) : null
  } catch {
    return null
  }
}

async function fetchSlow(): Promise<SlowReport | null> {
  try {
    const res = await authedFetch('/api/settings/database/slow')
    return res.ok ? ((await res.json()) as SlowReport) : null
  } catch {
    return null
  }
}
type Busy = 'preview' | 'cleanup' | 'compact' | 'check' | null

/**
 * Admin-only database upkeep (`service::db_maintenance`): how big the
 * database is, a cleanup of rows nothing points at anymore (always previewed
 * first), compaction, and an integrity check. Usage history, card history
 * and the approvals log are never part of the cleanup.
 */
export default function DatabaseMaintenanceSection() {
  const [stats, setStats] = useState<Stats | null>(null)
  const [busy, setBusy] = useState<Busy>(null)
  const [error, setError] = useState<string | null>(null)
  const [preview, setPreview] = useState<CleanupReport | null>(null)
  const [cleaned, setCleaned] = useState<CleanupReport | null>(null)
  const [confirm, setConfirm] = useState<'cleanup' | 'compact' | null>(null)
  const [dialogError, setDialogError] = useState<string | null>(null)
  const [compacted, setCompacted] = useState<CompactReport | null>(null)
  const [checked, setChecked] = useState<CheckReport | null>(null)
  const [slow, setSlow] = useState<SlowReport | null>(null)

  const loadStats = async () => {
    const next = await fetchStats()
    if (next) setStats(next)
  }

  useEffect(() => {
    let cancelled = false
    void fetchStats().then((next) => {
      if (!cancelled && next) setStats(next)
    })
    void fetchSlow().then((next) => {
      if (!cancelled && next) setSlow(next)
    })
    return () => {
      cancelled = true
    }
  }, [])

  const runPreview = async () => {
    setBusy('preview')
    setError(null)
    setCleaned(null)
    try {
      const res = await authedFetch('/api/settings/database/cleanup/preview', { method: 'POST' })
      if (!res.ok) throw new Error(await errorText(res, 'Could not work out what can be removed.'))
      setPreview((await res.json()) as CleanupReport)
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not work out what can be removed.')
    } finally {
      setBusy(null)
    }
  }

  const runCleanup = async () => {
    setBusy('cleanup')
    setDialogError(null)
    try {
      const res = await authedFetch('/api/settings/database/cleanup', { method: 'POST' })
      if (!res.ok) throw new Error(await errorText(res, 'The cleanup failed.'))
      setCleaned((await res.json()) as CleanupReport)
      setPreview(null)
      setConfirm(null)
      void loadStats()
    } catch (e) {
      setDialogError(e instanceof Error ? e.message : 'The cleanup failed.')
    } finally {
      setBusy(null)
    }
  }

  const runCompact = async () => {
    setBusy('compact')
    setDialogError(null)
    try {
      const res = await authedFetch('/api/settings/database/compact', { method: 'POST' })
      if (!res.ok) throw new Error(await errorText(res, 'Compacting failed.'))
      setCompacted((await res.json()) as CompactReport)
      setConfirm(null)
      void loadStats()
    } catch (e) {
      setDialogError(e instanceof Error ? e.message : 'Compacting failed.')
    } finally {
      setBusy(null)
    }
  }

  const runCheck = async () => {
    setBusy('check')
    setError(null)
    setChecked(null)
    try {
      const res = await authedFetch('/api/settings/database/check', { method: 'POST' })
      if (!res.ok) throw new Error(await errorText(res, 'The check could not run.'))
      setChecked((await res.json()) as CheckReport)
    } catch (e) {
      setError(e instanceof Error ? e.message : 'The check could not run.')
    } finally {
      setBusy(null)
    }
  }

  const closeConfirm = () => {
    setConfirm(null)
    setDialogError(null)
  }

  const found = preview?.items.filter((i) => i.rows > 0) ?? []
  const removed = cleaned?.items.filter((i) => i.rows > 0) ?? []

  return (
    <section className="settings-section" data-testid="database-maintenance-section">
      <h3>Database</h3>
      <p className="form-hint">
        Remove rows that nothing points at anymore, give unused space back to the disk, and check
        the database file for damage. Usage history, card history and the approvals log are never
        removed here.
      </p>

      {error && (
        <p className="form-error" role="alert" data-testid="database-error">
          {error}
        </p>
      )}

      {stats && (
        <div className="form-hint" data-testid="database-stats">
          <p data-testid="database-size">
            Size on disk: {formatBytes(stats.file_bytes)}.{' '}
            {stats.reclaimable_bytes > 0
              ? `Compacting would give back about ${formatBytes(stats.reclaimable_bytes)}.`
              : 'There is no unused space to give back.'}
          </p>
          <p>Largest tables:</p>
          <ul>
            {stats.tables
              .filter((t) => t.rows > 0)
              .slice(0, TOP_TABLES)
              .map((t) => (
                <li key={t.name}>
                  {t.name}: {rowCount(t.rows)}
                </li>
              ))}
          </ul>
        </div>
      )}

      {slow && (
        <div className="form-hint" data-testid="database-slow">
          {slow.operations.length === 0 ? (
            <p>
              No database operation took longer than {formatMs(slow.threshold_ms)} since the server
              started.
            </p>
          ) : (
            <>
              <p>
                Slowest database operations since the server started (everything else waits while
                one runs):
              </p>
              <ul>
                {slow.operations.slice(0, TOP_SLOW).map((op) => (
                  <li key={op.site}>
                    <code>{op.site}</code>: {op.count.toLocaleString()}{' '}
                    {op.count === 1 ? 'time' : 'times'}, longest {formatMs(op.max_run_ms)}, in total{' '}
                    {formatMs(op.total_run_ms)}
                    {op.max_wait_ms >= slow.threshold_ms &&
                      `; waited up to ${formatMs(op.max_wait_ms)} for its turn`}
                  </li>
                ))}
              </ul>
            </>
          )}
        </div>
      )}

      <div className="settings-row">
        <button
          type="button"
          className="btn-secondary"
          onClick={() => void runPreview()}
          disabled={busy !== null}
          data-testid="database-cleanup-preview"
        >
          {busy === 'preview' ? 'Checking…' : 'Preview cleanup'}
        </button>
        <button
          type="button"
          className="btn-secondary"
          onClick={() => setConfirm('compact')}
          disabled={busy !== null}
          data-testid="database-compact"
        >
          Compact database
        </button>
        <button
          type="button"
          className="btn-secondary"
          onClick={() => void runCheck()}
          disabled={busy !== null}
          data-testid="database-check"
        >
          {busy === 'check' ? 'Checking…' : 'Check database'}
        </button>
      </div>

      {preview && (
        <div className="form-hint" data-testid="database-cleanup-result">
          {found.length === 0 ? (
            <p>Nothing to clean up.</p>
          ) : (
            <>
              <p>A cleanup would remove {rowCount(preview.total)}:</p>
              <ul>
                {found.map((i) => (
                  <li key={i.key} data-testid={`database-cleanup-item-${i.key}`}>
                    {i.label}: {i.rows.toLocaleString()}
                  </li>
                ))}
              </ul>
              <p>Nothing has been removed yet.</p>
            </>
          )}
        </div>
      )}
      {preview && found.length > 0 && (
        <div className="settings-row">
          <button
            type="button"
            className="btn-secondary"
            onClick={() => setConfirm('cleanup')}
            disabled={busy !== null}
            data-testid="database-cleanup-run"
          >
            Clean up
          </button>
        </div>
      )}

      {cleaned && (
        <div className="form-hint" role="status" data-testid="database-cleanup-done">
          {removed.length === 0 ? (
            <p>Nothing needed removing.</p>
          ) : (
            <>
              <p>Removed {rowCount(cleaned.total)}:</p>
              <ul>
                {removed.map((i) => (
                  <li key={i.key}>
                    {i.label}: {i.rows.toLocaleString()}
                  </li>
                ))}
              </ul>
              <p>The file keeps its size until you compact the database.</p>
            </>
          )}
        </div>
      )}

      {compacted && (
        <p className="form-hint" role="status" data-testid="database-compact-done">
          Compacted: {formatBytes(compacted.before_bytes)} before,{' '}
          {formatBytes(compacted.after_bytes)} now.
        </p>
      )}

      {checked &&
        (checked.ok ? (
          <p className="form-hint" role="status" data-testid="database-check-result">
            No problems found.
          </p>
        ) : (
          <div className="form-error" role="alert" data-testid="database-check-result">
            <p>
              The check found problems. Download a backup before doing anything else with this
              database.
            </p>
            <ul>
              {checked.problems.slice(0, 10).map((line) => (
                <li key={line}>{line}</li>
              ))}
            </ul>
          </div>
        ))}

      {confirm === 'cleanup' && preview && (
        <ConfirmDialog
          title="Clean up the database?"
          message={`This removes ${rowCount(preview.total)} that nothing points at anymore. It cannot be undone.`}
          confirmLabel="Clean up"
          danger
          busy={busy === 'cleanup'}
          busyLabel="Cleaning up…"
          error={dialogError}
          testId="database-cleanup-confirm"
          onConfirm={() => void runCleanup()}
          onCancel={closeConfirm}
        />
      )}
      {confirm === 'compact' && (
        <ConfirmDialog
          title="Compact the database?"
          message="The database file is rewritten without its unused space. Peckboard stops responding while this runs: a few seconds for a small database, minutes for one of several gigabytes. Pick a moment when no agent is working."
          confirmLabel="Compact"
          busy={busy === 'compact'}
          busyLabel="Compacting…"
          error={dialogError}
          testId="database-compact-confirm"
          onConfirm={() => void runCompact()}
          onCancel={closeConfirm}
        />
      )}
    </section>
  )
}

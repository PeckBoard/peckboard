import { useEffect, useState } from 'react'
import { formatRelativeTime } from '../../../lib/review'
import {
  formatElapsed,
  taskStatusLabel,
  useBackgroundStore,
  type BackgroundTaskStatus,
} from '../../../store/background'
import WidgetFrame from '../WidgetFrame'
import { DashCount, DashEmpty, DashError, DashLoading } from './DashParts'
import { useDashboardData } from './useDashboardData'
import type { InfoWidgetProps } from './types'
import '../../../styles/dashboard-info.css'

/** One row of `GET /api/dashboard/background`. */
interface DashBackgroundTask {
  id: string
  session_id: string
  session_name: string
  label: string
  program: string
  status: BackgroundTaskStatus
  exit_code: number | null
  started_at: string
  finished_at: string | null
  stopping: boolean
}

/** While something runs, refresh faster than the hook's 30s poll so a
 *  finish shows up promptly. */
const RUNNING_POLL_MS = 5_000

function tone(t: DashBackgroundTask): string {
  if (t.status === 'running') return t.stopping ? 'warn' : 'run'
  if (t.status === 'succeeded') return 'ok'
  if (t.status === 'stopped') return 'neutral'
  return 'danger'
}

/** Background tasks (`run_background`) across the caller's sessions:
 *  running first with live elapsed time and a Stop button, then the last
 *  day's finished ones. A row opens its session. */
export default function BackgroundWidget({
  widget,
  ctx,
  menuItems,
  onOpenSession,
}: InfoWidgetProps) {
  const { data, error, reload } = useDashboardData<{ tasks: DashBackgroundTask[] }>(
    '/api/dashboard/background',
    null,
    ['session-updated'],
  )
  const stopTask = useBackgroundStore((s) => s.stopTask)
  // A `background_task` WS frame for any subscribed session lands here.
  const storeTick = useBackgroundStore((s) => s.tasksBySession)
  const [stopping, setStopping] = useState<Record<string, boolean>>({})
  const [stopError, setStopError] = useState<string | null>(null)
  const [now, setNow] = useState(() => Date.now())

  const tasks = data?.tasks ?? []
  const running = tasks.filter((t) => t.status === 'running').length

  useEffect(() => {
    void reload()
  }, [storeTick, reload])
  useEffect(() => {
    if (running === 0) return
    const tick = window.setInterval(() => setNow(Date.now()), 1000)
    const poll = window.setInterval(() => {
      if (document.visibilityState === 'visible') void reload()
    }, RUNNING_POLL_MS)
    return () => {
      window.clearInterval(tick)
      window.clearInterval(poll)
    }
  }, [running, reload])

  const stop = (id: string) => {
    setStopping((s) => ({ ...s, [id]: true }))
    setStopError(null)
    stopTask(id)
      .catch((e: unknown) => setStopError(e instanceof Error ? e.message : 'Failed to stop'))
      .finally(() => {
        setStopping((s) => ({ ...s, [id]: false }))
        void reload()
      })
  }

  let body
  if (error && !data) body = <DashError message={error} onRetry={() => void reload()} />
  else if (!data) body = <DashLoading />
  else if (tasks.length === 0)
    body = (
      <DashEmpty testId="dash-background-empty">
        <p>No background tasks in the last day</p>
      </DashEmpty>
    )
  else
    body = (
      <div className="dash-scroll">
        {stopError && (
          <p className="form-error dash-inline-error" role="alert">
            {stopError}
          </p>
        )}
        <ul className="dash-items" data-testid="dash-background-list">
          {tasks.map((t) => {
            const end = t.finished_at ? new Date(t.finished_at).getTime() : now
            const elapsed = formatElapsed(end - new Date(t.started_at).getTime())
            const isRunning = t.status === 'running'
            return (
              <li key={t.id} className="dash-item" data-task-id={t.id} data-status={t.status}>
                <button
                  type="button"
                  className="dash-item-main"
                  onClick={() => onOpenSession(t.session_id)}
                  title={`Open ${t.session_name || 'session'}`}
                >
                  <span className={`dash-pill dash-pill-${tone(t)}`}>
                    {taskStatusLabel(t)}
                    {t.exit_code !== null && t.status === 'failed' ? ` ${t.exit_code}` : ''}
                  </span>
                  <span className="dash-item-text">
                    <span className="dash-item-title dash-mono">{t.label || t.program}</span>
                    <span className="dash-item-sub">
                      {t.session_name || 'Session'}
                      {!isRunning && t.finished_at && ` · ${formatRelativeTime(t.finished_at)}`}
                    </span>
                  </span>
                  <span className="dash-item-num">{elapsed}</span>
                </button>
                {isRunning && (
                  <button
                    type="button"
                    className="btn-secondary btn-sm dash-item-action"
                    disabled={t.stopping || stopping[t.id]}
                    onClick={() => stop(t.id)}
                    aria-label={`Stop ${t.label || t.program}`}
                    data-testid="dash-background-stop"
                  >
                    {t.stopping || stopping[t.id] ? 'Stopping…' : 'Stop'}
                  </button>
                )}
              </li>
            )
          })}
        </ul>
      </div>
    )

  return (
    <WidgetFrame
      kind="background"
      widgetId={widget.id}
      title="Background Tasks"
      statusSlot={data && <DashCount n={running} label={`${running} running`} />}
      menuItems={menuItems}
      ctx={ctx}
    >
      {body}
    </WidgetFrame>
  )
}

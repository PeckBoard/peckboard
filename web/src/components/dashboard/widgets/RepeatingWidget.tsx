import { useEffect, useMemo, useState } from 'react'
import { authedFetch } from '../../../store/auth'
import { formatRelativeTime } from '../../../lib/review'
import { useRepeatingTasksStore } from '../../../store/repeatingTasks'
import type { RepeatingTask, RepeatingTaskRun } from '../../../types/api'
import { describeSchedule } from '../../../utils/repeatingSchedule'
import WidgetFrame from '../WidgetFrame'
import { DashCount, DashEmpty, DashError, DashLoading } from './DashParts'
import { useDashboardData } from './useDashboardData'
import type { InfoWidgetProps } from './types'
import '../../../styles/dashboard-info.css'

/** Run histories fetched per refresh — one request each, so keep it small. */
const MAX_OUTCOMES = 12

const OUTCOME: Record<string, { label: string; tone: string }> = {
  spawned: { label: 'Ran', tone: 'ok' },
  already_running: { label: 'Skipped', tone: 'neutral' },
  throttled: { label: 'Throttled', tone: 'warn' },
  failed: { label: 'Failed', tone: 'danger' },
  corrupt_schedule: { label: 'Bad schedule', tone: 'danger' },
  consumed_once: { label: 'Done', tone: 'neutral' },
}

/** "in 5m" / "3h ago" — handles both directions, unlike formatRelativeTime. */
function relative(iso: string | null, now: number): string {
  if (!iso) return '—'
  const t = new Date(iso).getTime()
  if (Number.isNaN(t)) return iso
  const diff = t - now
  const abs = Math.abs(diff)
  const past = diff < 0
  const m = Math.floor(abs / 60_000)
  if (m < 1) return past ? 'just now' : 'in <1m'
  const fmt = m < 60 ? `${m}m` : m < 1440 ? `${Math.floor(m / 60)}h` : `${Math.floor(m / 1440)}d`
  return past ? `${fmt} ago` : `in ${fmt}`
}

/** Repeating tasks: schedule, next run, last outcome, and Run now. */
export default function RepeatingWidget({ widget, ctx, menuItems }: InfoWidgetProps) {
  const { data, error, reload } = useDashboardData<RepeatingTask[]>(
    '/api/repeating-tasks',
    null,
    [],
  )
  const runNow = useRepeatingTasksStore((s) => s.runNow)
  const [outcomes, setOutcomes] = useState<Record<string, RepeatingTaskRun | null>>({})
  const [busy, setBusy] = useState<Record<string, boolean>>({})
  const [note, setNote] = useState<{ id: string; text: string } | null>(null)
  const [now, setNow] = useState(() => Date.now())

  useEffect(() => {
    const t = window.setInterval(() => setNow(Date.now()), 30_000)
    const onEvent = () => void reload()
    window.addEventListener('peckboard:repeating-task-changed', onEvent)
    window.addEventListener('peckboard:repeating-task-run', onEvent)
    return () => {
      window.clearInterval(t)
      window.removeEventListener('peckboard:repeating-task-changed', onEvent)
      window.removeEventListener('peckboard:repeating-task-run', onEvent)
    }
  }, [reload])

  // Enabled first, soonest next run first; paused ones trail.
  const tasks = useMemo(() => {
    const list = Array.isArray(data) ? data.slice() : []
    const key = (t: RepeatingTask) =>
      t.enabled && t.next_run_at ? new Date(t.next_run_at).getTime() : Infinity
    return list.sort((a, b) => key(a) - key(b) || a.name.localeCompare(b.name))
  }, [data])

  // Last outcome per task, refetched only when a task's last run changes.
  const runKey = tasks
    .slice(0, MAX_OUTCOMES)
    .map((t) => `${t.id}@${t.last_run_at ?? ''}`)
    .join(',')
  useEffect(() => {
    if (!runKey) return
    let cancelled = false
    for (const entry of runKey.split(',')) {
      const id = entry.slice(0, entry.indexOf('@'))
      authedFetch(`/api/repeating-tasks/${encodeURIComponent(id)}/runs`)
        .then((res) => (res.ok ? (res.json() as Promise<RepeatingTaskRun[]>) : null))
        .then((runs) => {
          if (cancelled || !Array.isArray(runs)) return
          setOutcomes((o) => ({ ...o, [id]: runs[0] ?? null }))
        })
        .catch(() => {})
    }
    return () => {
      cancelled = true
    }
  }, [runKey])

  const run = (t: RepeatingTask) => {
    setBusy((b) => ({ ...b, [t.id]: true }))
    setNote(null)
    runNow(t.id)
      .then((status) =>
        setNote({
          id: t.id,
          text:
            status === 'spawned'
              ? 'Started'
              : status === 'already_running'
                ? 'Already running'
                : 'Disabled',
        }),
      )
      .catch((e: unknown) =>
        setNote({ id: t.id, text: e instanceof Error ? e.message : 'Run failed' }),
      )
      .finally(() => {
        setBusy((b) => ({ ...b, [t.id]: false }))
        void reload()
      })
  }

  const enabled = tasks.filter((t) => t.enabled).length
  let body
  if (error && !data) body = <DashError message={error} onRetry={() => void reload()} />
  else if (!data) body = <DashLoading />
  else if (tasks.length === 0)
    body = (
      <DashEmpty testId="dash-repeating-empty">
        <p>No repeating tasks</p>
      </DashEmpty>
    )
  else
    body = (
      <div className="dash-scroll">
        <ul className="dash-items" data-testid="dash-repeating-list">
          {tasks.map((t) => {
            const last = outcomes[t.id]
            const o = last
              ? (OUTCOME[last.status] ?? { label: last.status, tone: 'neutral' })
              : null
            return (
              <li
                key={t.id}
                className={`dash-item${t.enabled ? '' : ' dash-item-muted'}`}
                data-task-id={t.id}
              >
                <div className="dash-item-main">
                  <span className="dash-item-text">
                    <span className="dash-item-title" title={t.description || t.name}>
                      {t.name}
                    </span>
                    <span className="dash-item-sub">
                      {describeSchedule(t.schedule_kind, t.schedule_value, t.timezone)}
                      {note?.id === t.id && ` · ${note.text}`}
                    </span>
                  </span>
                  {o && (
                    <span
                      className={`dash-pill dash-pill-${o.tone}`}
                      title={`Last run ${formatRelativeTime(last!.started_at)}${last!.detail ? ` — ${last!.detail}` : ''}`}
                    >
                      {o.label}
                    </span>
                  )}
                  <span
                    className="dash-item-num"
                    title={t.next_run_at ? new Date(t.next_run_at).toLocaleString() : undefined}
                  >
                    {t.enabled ? relative(t.next_run_at, now) : 'Paused'}
                  </span>
                </div>
                <button
                  type="button"
                  className="btn-secondary btn-sm dash-item-action"
                  disabled={busy[t.id]}
                  onClick={() => run(t)}
                  aria-label={`Run ${t.name} now`}
                  data-testid="dash-repeating-run"
                >
                  {busy[t.id] ? 'Running…' : 'Run'}
                </button>
              </li>
            )
          })}
        </ul>
      </div>
    )

  return (
    <WidgetFrame
      kind="repeating"
      widgetId={widget.id}
      title="Repeating Tasks"
      statusSlot={data && <DashCount n={enabled} label={`${enabled} enabled`} />}
      menuItems={menuItems}
      ctx={ctx}
    >
      {body}
    </WidgetFrame>
  )
}

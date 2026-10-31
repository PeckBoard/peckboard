import { useEffect, useMemo, useState } from 'react'
import type { RepeatingTask } from '../../types/api'
import { useRepeatingTasksStore } from '../../store/repeatingTasks'
import { describeSchedule } from '../../utils/repeatingSchedule'
import ConfirmDialog from '../ConfirmDialog'
import type { MenuItem } from '../Dropdown'
import List from '../List'
import { openInApp } from './openInApp'
import { upcomingAssistantTasks } from './upcoming'

/** Relative times re-render this often. */
const CLOCK_TICK_MS = 30_000

const userTimeZone = () => Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC'
/** `d` as `YYYY-MM-DDTHH:MM` wall-clock time in `tz` (a `once` schedule's `at`). */
function wallClock(d: Date, tz: string): string {
  const parts = Object.fromEntries(
    new Intl.DateTimeFormat('en-CA', {
      timeZone: tz,
      year: 'numeric',
      month: '2-digit',
      day: '2-digit',
      hour: '2-digit',
      minute: '2-digit',
      hourCycle: 'h23',
    })
      .formatToParts(d)
      .map((p) => [p.type, p.value]),
  )
  return `${parts.year}-${parts.month}-${parts.day}T${parts.hour}:${parts.minute}`
}

/** Tomorrow at 09:00 in `tz`, as a `once` schedule's `at`. */
function tomorrowNine(now: Date, tz: string): string {
  const [y, m, d] = wallClock(now, tz).slice(0, 10).split('-').map(Number)
  const next = new Date(Date.UTC(y, m - 1, d + 1))
  return `${next.toISOString().slice(0, 10)}T09:00`
}

function relative(iso: string, now: number): string {
  const minutes = Math.round((Date.parse(iso) - now) / 60_000)
  if (Math.abs(minutes) < 1) return 'now'
  const rtf = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' })
  if (Math.abs(minutes) < 60) return rtf.format(minutes, 'minute')
  const hours = Math.round(minutes / 60)
  if (Math.abs(hours) < 24) return rtf.format(hours, 'hour')
  return rtf.format(Math.round(hours / 24), 'day')
}

function absolute(iso: string): string {
  return new Date(iso).toLocaleString([], { weekday: 'short', hour: 'numeric', minute: '2-digit' })
}

/**
 * Upcoming: the tasks the Assistant will perform itself (Assistant-target
 * repeating tasks), soonest first, with the user's local time. A side panel
 * on desktop and a bottom sheet on phones.
 */
export default function AssistantUpcoming({
  open,
  onClose,
}: {
  open: boolean
  onClose: () => void
}) {
  const tasks = useRepeatingTasksStore((s) => s.tasks)
  const [now, setNow] = useState(() => Date.now())
  const [status, setStatus] = useState<{ ok: boolean; text: string } | null>(null)
  const [confirmDelete, setConfirmDelete] = useState<RepeatingTask | null>(null)
  const [deleteError, setDeleteError] = useState<string | null>(null)
  const [deleting, setDeleting] = useState(false)

  useEffect(() => {
    if (!open) return
    void useRepeatingTasksStore
      .getState()
      .fetchTasks()
      .catch(() => undefined)
    const t = setInterval(() => setNow(Date.now()), CLOCK_TICK_MS)
    return () => clearInterval(t)
  }, [open])

  const upcoming = useMemo(() => upcomingAssistantTasks(tasks), [tasks])

  const act = async (what: string, run: () => Promise<unknown>) => {
    setStatus(null)
    try {
      await run()
      setStatus({ ok: true, text: what })
      setNow(Date.now())
    } catch (e) {
      setStatus({ ok: false, text: e instanceof Error ? e.message : 'That failed.' })
    }
  }

  const snooze = (task: RepeatingTask, at: string, label: string) =>
    act(`“${task.name}” moved to ${label}.`, () =>
      useRepeatingTasksStore.getState().updateTask(task.id, {
        schedule_value: { at },
        timezone: userTimeZone(),
      }),
    )

  const menuFor = (task: RepeatingTask): MenuItem[] => {
    const store = useRepeatingTasksStore.getState()
    const tz = userTimeZone()
    const once = task.schedule_kind === 'once'
    // Snoozing pushes the task later than both now and its current time.
    const base = () => new Date(Math.max(Date.now(), Date.parse(task.next_run_at ?? '') || 0))
    const later = (minutes: number) => wallClock(new Date(base().getTime() + minutes * 60_000), tz)
    return [
      {
        label: 'Run now',
        testId: 'assistant-upcoming-run',
        onSelect: () =>
          void act(`“${task.name}” handed to the Assistant.`, () => store.runNow(task.id)),
      },
      { divider: true },
      {
        label: 'Snooze 15 minutes',
        hidden: !once,
        onSelect: () => void snooze(task, later(15), 'later'),
      },
      {
        label: 'Snooze 1 hour',
        hidden: !once,
        onSelect: () => void snooze(task, later(60), 'later'),
      },
      {
        label: 'Snooze until tomorrow 9 AM',
        hidden: !once,
        onSelect: () => void snooze(task, tomorrowNine(new Date(), tz), 'tomorrow at 9 AM'),
      },
      {
        label: task.enabled ? 'Pause' : 'Resume',
        hidden: once,
        testId: 'assistant-upcoming-pause',
        onSelect: () =>
          void act(`“${task.name}” ${task.enabled ? 'paused' : 'resumed'}.`, () =>
            store.updateTask(task.id, { enabled: !task.enabled }),
          ),
      },
      { divider: true },
      {
        label: 'Delete',
        danger: true,
        testId: 'assistant-upcoming-delete',
        onSelect: () => {
          setDeleteError(null)
          setConfirmDelete(task)
        },
      },
    ]
  }

  const doDelete = async () => {
    if (!confirmDelete) return
    setDeleting(true)
    try {
      await useRepeatingTasksStore.getState().deleteTask(confirmDelete.id)
      setConfirmDelete(null)
    } catch (e) {
      setDeleteError(e instanceof Error ? e.message : 'Failed to delete it.')
    } finally {
      setDeleting(false)
    }
  }

  return (
    <>
      <aside
        className="assistant-upcoming"
        data-testid="assistant-upcoming"
        data-open={open ? 'true' : 'false'}
        aria-label="Upcoming"
        aria-hidden={!open}
        inert={!open}
      >
        <header className="assistant-upcoming-header">
          <h2>Upcoming</h2>
          <button
            type="button"
            className="assistant-upcoming-close"
            aria-label="Close upcoming"
            onClick={onClose}
          >
            ×
          </button>
        </header>
        <List
          items={upcoming}
          getKey={(t) => t.id}
          bodyClassName="assistant-upcoming-body"
          onActivate={(t) => openInApp('repeating_task', t.id)}
          getMenuItems={menuFor}
          emptyState={
            <p className="assistant-upcoming-empty">
              Nothing scheduled. Ask the Assistant &ldquo;remind me at five&rdquo; or &ldquo;check
              the build in an hour&rdquo;.
            </p>
          }
          renderItem={(t) => (
            <span className="assistant-upcoming-row" data-testid="assistant-upcoming-row">
              <span className="assistant-upcoming-name">{t.name}</span>
              <span className="assistant-upcoming-when">
                {t.enabled && t.next_run_at ? (
                  <>
                    <strong>{relative(t.next_run_at, now)}</strong> · {absolute(t.next_run_at)}
                  </>
                ) : (
                  <strong>Paused</strong>
                )}
              </span>
              <span className="assistant-upcoming-schedule">
                {describeSchedule(t.schedule_kind, t.schedule_value, t.timezone)}
              </span>
            </span>
          )}
        />
        {status && (
          <p
            className={status.ok ? 'assistant-upcoming-status' : 'assistant-upcoming-status error'}
            role={status.ok ? 'status' : 'alert'}
          >
            {status.text}
          </p>
        )}
      </aside>
      {confirmDelete && (
        <ConfirmDialog
          title="Delete scheduled task?"
          message={`“${confirmDelete.name}” will not run again.`}
          confirmLabel="Delete"
          danger
          busy={deleting}
          error={deleteError}
          onConfirm={() => void doDelete()}
          onCancel={() => setConfirmDelete(null)}
        />
      )}
    </>
  )
}

import ConfirmDialog from './ConfirmDialog'
import type { RestartActivity, RestartKind } from '../store/restart'

/** "42s", "3m 05s", "1h 02m". */
function formatRuntime(secs: number | null): string {
  if (secs == null) return ''
  if (secs < 60) return `${secs}s`
  const m = Math.floor(secs / 60)
  if (m < 60) return `${m}m ${String(secs % 60).padStart(2, '0')}s`
  return `${Math.floor(m / 60)}h ${String(m % 60).padStart(2, '0')}m`
}

interface Row {
  key: string
  name: string
  context: string
  secs: number | null
}

function rowsOf(a: RestartActivity): { id: string; label: string; rows: Row[] }[] {
  return [
    {
      id: 'sessions',
      label: 'Sessions mid-turn',
      rows: a.sessions.map((s) => ({
        key: s.session_id,
        name: s.name,
        context: [s.project_name, s.folder_name].filter(Boolean).join(' · '),
        secs: s.running_secs,
      })),
    },
    {
      id: 'workers',
      label: 'Card workers',
      rows: a.workers.map((w) => ({
        key: w.session_id,
        name: w.card_title ?? w.session_name,
        context: [w.project_name, w.step].filter(Boolean).join(' · '),
        secs: w.running_secs,
      })),
    },
    {
      id: 'subagents',
      label: 'Subagents',
      rows: a.subagents.map((s) => ({
        key: s.session_id,
        name: s.name,
        context: s.parent_name ? `for ${s.parent_name}` : '',
        secs: s.running_secs,
      })),
    },
    {
      id: 'background',
      label: 'Background tasks',
      rows: a.background_tasks.map((t) => ({
        key: t.task_id,
        name: t.label || t.command,
        context: [t.session_name, t.label ? t.command : null].filter(Boolean).join(' · '),
        secs: t.running_secs,
      })),
    },
  ].filter((g) => g.rows.length > 0)
}

function ActivityList({ activity }: { activity: RestartActivity }) {
  return (
    <div className="restart-activity" data-testid="restart-activity">
      {rowsOf(activity).map((g) => (
        <details
          key={g.id}
          className="restart-activity-group"
          data-testid={`restart-group-${g.id}`}
        >
          <summary>
            {g.label} <span className="restart-activity-count">{g.rows.length}</span>
          </summary>
          <ul>
            {g.rows.map((r) => (
              <li key={r.key}>
                <span className="restart-activity-name">{r.name}</span>
                {r.context && <span className="restart-activity-context">{r.context}</span>}
                {r.secs != null && (
                  <span className="restart-activity-time">{formatRuntime(r.secs)}</span>
                )}
              </li>
            ))}
          </ul>
        </details>
      ))}
    </div>
  )
}

/**
 * Confirmation shown before a restart (plain or update-and-restart) while
 * work is still running: lists what would be interrupted and offers
 * "Restart anyway", "Restart when idle", "Cancel". Driven by
 * `useGuardedRestart`, which skips it entirely when nothing is running.
 */
export default function RestartConfirm({
  kind,
  version,
  activity,
  busy,
  error,
  onNow,
  onIdle,
  onCancel,
}: {
  kind: RestartKind
  version?: string | null
  activity: RestartActivity
  busy: boolean
  error: string | null
  onNow: () => void
  onIdle: () => void
  onCancel: () => void
}) {
  const target = kind === 'update' ? `Upgrade to ${version ?? 'the latest release'}` : 'Restart'
  const n = activity.total
  return (
    <ConfirmDialog
      testId="restart-confirm"
      danger
      wide
      title={`${target} while work is running?`}
      message={`${n} ${n === 1 ? 'item is' : 'items are'} still running and would be interrupted. “Restart when idle” waits until everything has finished${kind === 'update' ? ' (the update downloads now)' : ''}.`}
      confirmLabel="Restart anyway"
      confirmTestId="restart-confirm-now"
      secondaryAction={{
        label: 'Restart when idle',
        onSelect: onIdle,
        testId: 'restart-confirm-idle',
      }}
      cancelLabel="Cancel"
      error={error}
      busy={busy}
      busyLabel="Working…"
      onConfirm={onNow}
      onCancel={onCancel}
    >
      <ActivityList activity={activity} />
    </ConfirmDialog>
  )
}

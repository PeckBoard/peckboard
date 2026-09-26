import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import List from '../List'
import ConfirmDialog from '../ConfirmDialog'
import type { MenuItem } from '../Dropdown'
import {
  EMPTY_BACKGROUND_TASKS,
  formatElapsed,
  taskCommandLine,
  taskStatusLabel,
  useBackgroundStore,
  type BackgroundTask,
} from '../../store/background'
import { formatTime, type DisplayItem } from './events'

/** How often an expanded running task's output tail is re-fetched. There is
 *  no live output push — the WS event only carries lifecycle changes. */
const LOG_POLL_MS = 2000
const LOG_LINES = 200

/** Display name: the agent's label, else the command line. */
function taskName(t: BackgroundTask): string {
  return t.label?.trim() || taskCommandLine(t)
}

/** Current time, re-read every second while `active`. */
function useNow(active: boolean): number {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    if (!active) return
    const id = setInterval(() => setNow(Date.now()), 1000)
    return () => clearInterval(id)
  }, [active])
  return now
}

function elapsedMs(t: BackgroundTask, now: number): number {
  const start = Date.parse(t.started_at)
  const end = t.finished_at ? Date.parse(t.finished_at) : now
  return Number.isFinite(start) && Number.isFinite(end) ? end - start : 0
}

function StatusBadge({ task }: { task: BackgroundTask }) {
  const cls = task.status === 'running' && task.stopping ? 'stopping' : task.status
  return (
    <span
      className={`status-badge bg-task-status bg-task-status-${cls}`}
      data-testid="bg-task-status"
    >
      {taskStatusLabel(task)}
    </span>
  )
}
/** Toolbar chip: running count (or total) — toggles the panel. Hidden until
 *  the session has started at least one background task. */
export function BackgroundTasksButton({
  total,
  running,
  open,
  onToggle,
}: {
  total: number
  running: number
  open: boolean
  onToggle: () => void
}) {
  if (total === 0) return null
  return (
    <button
      type="button"
      className={`bg-tasks-toggle${running > 0 ? ' bg-tasks-toggle-running' : ''}`}
      aria-expanded={open}
      aria-controls="bg-tasks-panel"
      title={
        running > 0
          ? `${running} background task${running === 1 ? '' : 's'} running — show background tasks`
          : 'Show background tasks'
      }
      data-testid="bg-tasks-toggle"
      data-running={running}
      onClick={onToggle}
    >
      {running > 0 && <span className="bg-tasks-toggle-dot" aria-hidden="true" />}
      <span>Background</span>
      <span className="bg-tasks-toggle-count" data-testid="bg-tasks-toggle-count">
        {running > 0 ? `${running} running` : total}
      </span>
    </button>
  )
}

/**
 * The session's background tasks, newest first, pinned below the toolbar
 * like the todo panel. Clicking a row shows its output tail underneath
 * (polled while the task runs); running tasks can be stopped via a
 * confirmation.
 */
export function BackgroundTasksPanel({
  tasks,
  selectedId,
  onSelect,
  onClose,
}: {
  tasks: BackgroundTask[]
  selectedId: string | null
  onSelect: (id: string | null) => void
  onClose: () => void
}) {
  const stopTask = useBackgroundStore((s) => s.stopTask)
  const [confirmStop, setConfirmStop] = useState<BackgroundTask | null>(null)
  const [stopBusy, setStopBusy] = useState(false)
  const [stopError, setStopError] = useState<string | null>(null)
  const newestFirst = useMemo(() => [...tasks].reverse(), [tasks])
  const anyRunning = tasks.some((t) => t.status === 'running')
  const now = useNow(anyRunning)
  const selected = tasks.find((t) => t.id === selectedId) ?? null
  const running = tasks.filter((t) => t.status === 'running').length

  const askStop = (t: BackgroundTask) => {
    setStopError(null)
    setConfirmStop(t)
  }

  const runStop = async () => {
    if (!confirmStop) return
    setStopBusy(true)
    setStopError(null)
    try {
      await stopTask(confirmStop.id)
      setConfirmStop(null)
    } catch (err) {
      setStopError(err instanceof Error ? err.message : 'Failed to stop the task')
    } finally {
      setStopBusy(false)
    }
  }

  const menuFor = (t: BackgroundTask): MenuItem[] => [
    {
      label: t.id === selectedId ? 'Hide output' : 'Show output',
      onSelect: () => onSelect(t.id === selectedId ? null : t.id),
    },
    {
      label: 'Stop',
      danger: true,
      hidden: t.status !== 'running' || t.stopping,
      onSelect: () => askStop(t),
      testId: 'bg-task-menu-stop',
    },
  ]

  return (
    <div className="bg-tasks-panel" id="bg-tasks-panel" data-testid="bg-tasks-panel">
      <div className="bg-tasks-panel-header">
        <span className="bg-tasks-panel-title">Background Tasks</span>
        <span className="bg-tasks-panel-count">
          {running > 0 ? `${running} running · ` : ''}
          {tasks.length} total
        </span>
        <button
          type="button"
          className="bg-tasks-panel-close"
          aria-label="Close background tasks"
          data-testid="bg-tasks-close"
          onClick={onClose}
        >
          ×
        </button>
      </div>
      <List
        items={newestFirst}
        getKey={(t) => t.id}
        activeId={selectedId}
        onActivate={(t) => onSelect(t.id === selectedId ? null : t.id)}
        getMenuItems={menuFor}
        bodyClassName="bg-tasks-list"
        emptyState={<div className="list-view-empty">No background tasks.</div>}
        renderItem={(t) => (
          <span className="bg-task-row" data-testid="bg-task-row" data-task-id={t.id}>
            <StatusBadge task={t} />
            <span className="list-view-name bg-task-name" title={taskCommandLine(t)}>
              {taskName(t)}
            </span>
            <span className="list-view-meta">
              {t.exit_code !== null && (
                <span
                  className={`list-view-tag bg-task-exit${t.exit_code !== 0 ? ' bg-task-exit-bad' : ''}`}
                  data-testid="bg-task-exit"
                >
                  exit {t.exit_code}
                </span>
              )}
              <span className="list-view-time" data-testid="bg-task-elapsed">
                {formatElapsed(elapsedMs(t, now))}
              </span>
            </span>
          </span>
        )}
      />
      {selected && (
        <BackgroundTaskOutput
          key={selected.id}
          task={selected}
          onStop={() => askStop(selected)}
          onClose={() => onSelect(null)}
        />
      )}
      {confirmStop && (
        <ConfirmDialog
          title="Stop background task?"
          message={`"${taskName(confirmStop)}" will be sent SIGTERM (SIGKILL after 5 seconds). The agent is notified that it stopped.`}
          confirmLabel="Stop task"
          busyLabel="Stopping…"
          danger
          busy={stopBusy}
          error={stopError}
          testId="bg-task-stop-confirm"
          onConfirm={() => void runStop()}
          onCancel={() => setConfirmStop(null)}
        />
      )}
    </div>
  )
}

/** Output tail of one task: fetched on open, re-polled every 2s while the
 *  task runs, and once more when it finishes so the final lines land. */
function BackgroundTaskOutput({
  task,
  onStop,
  onClose,
}: {
  task: BackgroundTask
  onStop: () => void
  onClose: () => void
}) {
  const fetchLog = useBackgroundStore((s) => s.fetchLog)
  const [lines, setLines] = useState<string[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const preRef = useRef<HTMLPreElement>(null)
  const stickRef = useRef(true)
  const isRunning = task.status === 'running'

  const load = useCallback(
    async (isCancelled: () => boolean) => {
      try {
        const res = await fetchLog(task.id, LOG_LINES)
        // A poll that was in flight when the task finished (or the panel
        // switched tasks) must not overwrite the newer tail.
        if (isCancelled()) return
        setLines(res.lines)
        setError(null)
      } catch (err) {
        if (isCancelled()) return
        setError(err instanceof Error ? err.message : 'Failed to load output')
      }
    },
    [fetchLog, task.id],
  )

  // Fetch now, and again every LOG_POLL_MS while running. Re-runs when the
  // task flips to finished, which picks up the final output.
  useEffect(() => {
    let cancelled = false
    const isCancelled = () => cancelled
    const tick = () => {
      if (!cancelled) void load(isCancelled)
    }
    tick()
    const id = isRunning ? setInterval(tick, LOG_POLL_MS) : null
    return () => {
      cancelled = true
      if (id !== null) clearInterval(id)
    }
  }, [load, isRunning])

  // Follow the tail unless the user scrolled up to read.
  useLayoutEffect(() => {
    const el = preRef.current
    if (el && stickRef.current) el.scrollTop = el.scrollHeight
  }, [lines])

  return (
    <div className="bg-task-output" data-testid="bg-task-output">
      <div className="bg-task-output-header">
        <StatusBadge task={task} />
        <span className="bg-task-output-name">{taskName(task)}</span>
        <code className="bg-task-output-cmd" title={task.cwd}>
          {taskCommandLine(task)}
        </code>
        {isRunning && !task.stopping && (
          <button
            type="button"
            className="btn-secondary btn-sm bg-task-stop"
            data-testid="bg-task-stop"
            onClick={onStop}
          >
            Stop
          </button>
        )}
        <button
          type="button"
          className="bg-tasks-panel-close"
          aria-label="Hide output"
          data-testid="bg-task-output-close"
          onClick={onClose}
        >
          ×
        </button>
      </div>
      {error && (
        <div className="bg-task-output-error" role="alert">
          {error}
        </div>
      )}
      <pre
        ref={preRef}
        className="bg-task-output-pre"
        data-testid="bg-task-output-pre"
        tabIndex={0}
        aria-label="Task output"
        onScroll={(e) => {
          const el = e.currentTarget
          stickRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24
        }}
      >
        {lines === null
          ? 'Loading output…'
          : lines.length === 0
            ? isRunning
              ? '(no output yet)'
              : '(no output)'
            : lines.join('\n')}
      </pre>
      <div className="bg-task-output-foot">
        {isRunning ? `Refreshing every ${LOG_POLL_MS / 1000}s · ` : ''}
        last {LOG_LINES} lines{task.log_truncated ? ' · log truncated' : ''} ·{' '}
        <span title="Full log on the server">{task.log_path}</span>
      </div>
    </div>
  )
}

const NOTICE_VERB: Record<string, string> = {
  succeeded: 'succeeded',
  failed: 'failed',
  timed_out: 'timed out',
  stopped: 'was stopped',
}

/**
 * Chat row for the report peckboard injects when a background task exits.
 * A compact status-coloured notice instead of a user bubble (the user never
 * typed it); the full report the agent received expands below, and the
 * notice opens the task in the Background Tasks panel while it's still
 * tracked (finished tasks are forgotten after 24h).
 */
export function BackgroundTaskNotice({
  item,
  sessionId,
}: {
  item: Extract<DisplayItem, { type: 'background-task' }>
  sessionId: string
}) {
  const known = useBackgroundStore((s) =>
    (s.tasksBySession[sessionId] ?? EMPTY_BACKGROUND_TASKS).some((t) => t.id === item.taskId),
  )
  const requestFocus = useBackgroundStore((s) => s.requestFocus)
  const tone = item.status === 'succeeded' ? 'ok' : item.status === 'stopped' ? 'neutral' : 'bad'
  const name = item.label || 'Background task'
  return (
    <div className="chat-row chat-row-system">
      <div
        className={`chat-bg-notice chat-bg-notice-${tone}`}
        data-testid="chat-bg-notice"
        data-status={item.status}
      >
        <div className="chat-bg-notice-line">
          <span className="chat-bg-notice-dot" aria-hidden="true" />
          <span className="chat-bg-notice-label">
            Background task {NOTICE_VERB[item.status] ?? item.status}
          </span>
          {known ? (
            <button
              type="button"
              className="chat-bg-notice-name"
              title="Show in Background Tasks"
              data-testid="chat-bg-notice-open"
              onClick={() => requestFocus(sessionId, item.taskId)}
            >
              {name}
            </button>
          ) : (
            <span className="chat-bg-notice-name chat-bg-notice-name-static">{name}</span>
          )}
          {item.exitCode !== null && (
            <span className="chat-bg-notice-exit">exit {item.exitCode}</span>
          )}
          <span className="chat-agent-start-time">{formatTime(item.ts)}</span>
        </div>
        {item.text && (
          <details className="chat-bg-notice-details">
            <summary>Report sent to the agent</summary>
            <pre className="chat-bg-notice-text">{item.text}</pre>
          </details>
        )}
      </div>
    </div>
  )
}

import { useEffect, useMemo, useState } from 'react'
import { useFoldersStore } from '../store/folders'
import { useRepeatingTasksStore } from '../store/repeatingTasks'
import type { RepeatingTask } from '../types/api'
import ConfirmDialog from './ConfirmDialog'
import NewRepeatingTaskModal from './NewRepeatingTaskModal'
import { describeSchedule } from '../utils/repeatingSchedule'

interface Props {
  activeTaskId: string | null
  onNavigate: (taskId: string | null) => void
  onOpenSession: (sessionId: string) => void
}

function formatRelative(dateStr: string | null): string {
  if (!dateStr) return 'never'
  const now = Date.now()
  const then = new Date(dateStr).getTime()
  if (Number.isNaN(then)) return dateStr
  const diffMs = then - now
  const absMs = Math.abs(diffMs)
  const past = diffMs < 0
  const seconds = Math.floor(absMs / 1000)
  if (seconds < 60) return past ? 'just now' : 'in <1 min'
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return past ? `${minutes}m ago` : `in ${minutes}m`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return past ? `${hours}h ago` : `in ${hours}h`
  const days = Math.floor(hours / 24)
  if (days < 30) return past ? `${days}d ago` : `in ${days}d`
  return new Date(dateStr).toLocaleString()
}

export default function RepeatingTasksView({ activeTaskId, onNavigate, onOpenSession }: Props) {
  const tasks = useRepeatingTasksStore((s) => s.tasks)
  const loaded = useRepeatingTasksStore((s) => s.loaded)
  const fetchTasks = useRepeatingTasksStore((s) => s.fetchTasks)
  const deleteTask = useRepeatingTasksStore((s) => s.deleteTask)
  const updateTask = useRepeatingTasksStore((s) => s.updateTask)
  const runNow = useRepeatingTasksStore((s) => s.runNow)
  const applyChange = useRepeatingTasksStore((s) => s.applyChange)
  const sessionsByTask = useRepeatingTasksStore((s) => s.sessionsByTask)
  const fetchSessionsForTask = useRepeatingTasksStore((s) => s.fetchSessionsForTask)
  const folders = useFoldersStore((s) => s.folders)
  const fetchFolders = useFoldersStore((s) => s.fetchFolders)

  const folderMap = useMemo(() => {
    const m = new Map<string, string>()
    for (const f of folders) m.set(f.id, f.name)
    return m
  }, [folders])

  const [showCreate, setShowCreate] = useState(false)
  const [editingTask, setEditingTask] = useState<RepeatingTask | null>(null)
  const [confirmDeleteId, setConfirmDeleteId] = useState<string | null>(null)
  const [runStatus, setRunStatus] = useState<string | null>(null)
  const [runStatusTaskId, setRunStatusTaskId] = useState<string | null>(null)
  const [contextOpenId, setContextOpenId] = useState<string | null>(null)

  useEffect(() => {
    fetchTasks().catch(() => {})
    fetchFolders()
  }, [fetchTasks, fetchFolders])

  // Live update on WS task changes
  useEffect(() => {
    const onChange = (e: Event) => {
      const detail = (e as CustomEvent).detail as {
        data?: { action?: string; id?: string; task?: RepeatingTask }
      }
      if (!detail?.data) return
      applyChange(detail.data.action ?? 'updated', {
        id: detail.data.id,
        task: detail.data.task,
      })
    }
    const onRun = (e: Event) => {
      const detail = (e as CustomEvent).detail as {
        data?: { taskId?: string }
      }
      const taskId = detail?.data?.taskId
      if (taskId) {
        // Refresh sessions for the task so the new run shows up.
        fetchSessionsForTask(taskId).catch(() => {})
      }
      // Also refresh the task itself so last_run_at / next_run_at are current.
      fetchTasks().catch(() => {})
    }
    window.addEventListener('peckboard:repeating-task-changed', onChange)
    window.addEventListener('peckboard:repeating-task-run', onRun)
    return () => {
      window.removeEventListener('peckboard:repeating-task-changed', onChange)
      window.removeEventListener('peckboard:repeating-task-run', onRun)
    }
  }, [applyChange, fetchSessionsForTask, fetchTasks])

  // Fetch sessions when a detail view is opened
  useEffect(() => {
    if (activeTaskId) {
      fetchSessionsForTask(activeTaskId).catch(() => {})
    }
  }, [activeTaskId, fetchSessionsForTask])

  const handleToggleEnabled = async (task: RepeatingTask) => {
    try {
      await updateTask(task.id, { enabled: !task.enabled })
    } catch (err) {
      setRunStatus(err instanceof Error ? err.message : 'Failed to update task')
      setRunStatusTaskId(task.id)
    }
  }

  const handleRun = async (id: string) => {
    setRunStatus(null)
    setRunStatusTaskId(id)
    try {
      const status = await runNow(id)
      const text =
        status === 'spawned'
          ? 'Spawned a new session.'
          : status === 'already_running'
            ? 'Already running — no new session was created.'
            : 'Task is disabled.'
      setRunStatus(text)
      // Refresh so last_run_at lands in the UI.
      fetchTasks().catch(() => {})
    } catch (err) {
      setRunStatus(err instanceof Error ? err.message : 'Run failed')
    }
  }

  const confirmDelete = async () => {
    if (!confirmDeleteId) return
    const id = confirmDeleteId
    setConfirmDeleteId(null)
    try {
      await deleteTask(id)
      if (activeTaskId === id) onNavigate(null)
    } catch {
      /* ignore */
    }
  }

  if (activeTaskId) {
    const task = tasks.find((t) => t.id === activeTaskId)
    if (!loaded) {
      return <div className="list-view-empty">Loading…</div>
    }
    if (!task) {
      return (
        <div className="list-view-empty">
          <p>Task not found.</p>
          <button className="list-view-empty-action" onClick={() => onNavigate(null)}>
            Back to list
          </button>
        </div>
      )
    }
    const taskSessions = sessionsByTask[task.id] ?? []
    return (
      <div className="list-view">
        <div className="list-view-header">
          <div style={{ display: 'flex', alignItems: 'center', gap: 12, flex: 1, minWidth: 0 }}>
            <button className="btn-secondary" onClick={() => onNavigate(null)}>
              ← Back
            </button>
            <h2 className="list-view-title" style={{ flex: 1, minWidth: 0 }}>
              {task.name}
            </h2>
          </div>
          <div style={{ display: 'flex', gap: 8 }}>
            <button className="btn-secondary" onClick={() => handleToggleEnabled(task)}>
              {task.enabled ? 'Pause' : 'Resume'}
            </button>
            <button className="btn-secondary" onClick={() => setEditingTask(task)}>
              Edit
            </button>
            <button className="btn-primary" onClick={() => handleRun(task.id)}>
              Run now
            </button>
          </div>
        </div>
        <div className="list-view-body">
          <div className="repeating-task-detail">
            {task.description && <p className="repeating-task-desc">{task.description}</p>}
            <dl className="repeating-task-meta">
              <dt>Folder</dt>
              <dd>{folderMap.get(task.folder_id) ?? task.folder_id}</dd>
              <dt>Schedule</dt>
              <dd>{describeSchedule(task.schedule_kind, task.schedule_value)}</dd>
              <dt>Enabled</dt>
              <dd>{task.enabled ? 'Yes' : 'No'}</dd>
              <dt>Last run</dt>
              <dd>{formatRelative(task.last_run_at)}</dd>
              <dt>Next run</dt>
              <dd>{task.enabled ? formatRelative(task.next_run_at) : '— (disabled)'}</dd>
            </dl>
            <details className="repeating-task-prompt">
              <summary>Prompt</summary>
              <pre>{task.prompt}</pre>
            </details>

            {runStatusTaskId === task.id && runStatus && (
              <div className="repeating-task-run-status">{runStatus}</div>
            )}

            <h3>Sessions ({taskSessions.length})</h3>
            {taskSessions.length === 0 ? (
              <p className="form-help">No sessions yet. Click &quot;Run now&quot; to start one.</p>
            ) : (
              <ul className="repeating-task-sessions">
                {taskSessions.map((s) => (
                  <li key={s.id}>
                    <button
                      className="list-view-item"
                      onClick={() => onOpenSession(s.id)}
                      title="Open session"
                    >
                      <span className="list-view-name">{s.name}</span>
                      <span className="list-view-meta">
                        <span className="list-view-time">{formatRelative(s.last_activity)}</span>
                      </span>
                    </button>
                  </li>
                ))}
              </ul>
            )}
          </div>
        </div>
        {editingTask && (
          <NewRepeatingTaskModal
            initial={editingTask}
            onClose={() => setEditingTask(null)}
            onSaved={() => setEditingTask(null)}
          />
        )}
      </div>
    )
  }

  return (
    <div className="list-view">
      <div className="list-view-header">
        <h2 className="list-view-title">Repeating Tasks</h2>
        <button className="list-view-action" onClick={() => setShowCreate(true)}>
          + New task
        </button>
      </div>
      <div className="list-view-body">
        {!loaded ? (
          <div className="list-view-empty">Loading…</div>
        ) : tasks.length === 0 ? (
          <div className="list-view-empty">
            <p>No repeating tasks yet</p>
            <button className="list-view-empty-action" onClick={() => setShowCreate(true)}>
              Create your first task
            </button>
          </div>
        ) : (
          tasks.map((t) => (
            <div key={t.id} className="list-view-row">
              <button className="list-view-item" onClick={() => onNavigate(t.id)}>
                {!t.enabled && <span className="status-badge status-paused">paused</span>}
                <span className="list-view-name">{t.name}</span>
                <span className="list-view-meta">
                  {folderMap.get(t.folder_id) && (
                    <span className="list-view-tag">{folderMap.get(t.folder_id)}</span>
                  )}
                  <span className="list-view-tag">
                    {describeSchedule(t.schedule_kind, t.schedule_value)}
                  </span>
                  <span className="list-view-time">
                    {t.enabled
                      ? `next ${formatRelative(t.next_run_at)}`
                      : `last ${formatRelative(t.last_run_at)}`}
                  </span>
                </span>
              </button>
              <button
                className="list-view-menu"
                onClick={(e) => {
                  e.stopPropagation()
                  setContextOpenId(contextOpenId === t.id ? null : t.id)
                }}
                aria-label="Task menu"
              >
                ···
              </button>
              {contextOpenId === t.id && (
                <div className="list-view-dropdown">
                  <button
                    onClick={() => {
                      setContextOpenId(null)
                      handleRun(t.id)
                    }}
                  >
                    Run now
                  </button>
                  <button
                    onClick={() => {
                      setContextOpenId(null)
                      handleToggleEnabled(t)
                    }}
                  >
                    {t.enabled ? 'Pause' : 'Resume'}
                  </button>
                  <button
                    onClick={() => {
                      setContextOpenId(null)
                      setEditingTask(t)
                    }}
                  >
                    Edit
                  </button>
                  <button
                    onClick={() => {
                      setContextOpenId(null)
                      setConfirmDeleteId(t.id)
                    }}
                  >
                    Delete
                  </button>
                </div>
              )}
            </div>
          ))
        )}
      </div>

      {showCreate && (
        <NewRepeatingTaskModal
          onClose={() => setShowCreate(false)}
          onSaved={(t) => {
            setShowCreate(false)
            onNavigate(t.id)
          }}
        />
      )}
      {editingTask && (
        <NewRepeatingTaskModal
          initial={editingTask}
          onClose={() => setEditingTask(null)}
          onSaved={() => setEditingTask(null)}
        />
      )}
      {confirmDeleteId && (
        <ConfirmDialog
          title="Delete repeating task"
          message="Delete this task? Previously spawned sessions are kept but the schedule will stop."
          confirmLabel="Delete"
          cancelLabel="Cancel"
          danger
          onConfirm={confirmDelete}
          onCancel={() => setConfirmDeleteId(null)}
        />
      )}
    </div>
  )
}

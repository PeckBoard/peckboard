import { create } from 'zustand'
import { authedFetch } from './auth'

/** Lifecycle of a peckboard-managed background task (`run_background`). */
export type BackgroundTaskStatus = 'running' | 'succeeded' | 'failed' | 'timed_out' | 'stopped'

/** Task JSON as served by `/api/sessions/:id/background`, the log route, and
 *  the `background_task` WS event. */
export interface BackgroundTask {
  id: string
  session_id: string
  label: string
  program: string
  args: string[]
  cwd: string
  /** RFC 3339. */
  started_at: string
  /** RFC 3339; null while running. */
  finished_at: string | null
  status: BackgroundTaskStatus
  exit_code: number | null
  signal: number | null
  pid: number | null
  log_path: string
  log_truncated: boolean
  timeout_secs: number
  /** A stop was requested and the process hasn't exited yet. */
  stopping: boolean
}

/** A request to open a session's panel on one task (e.g. from the chat's
 *  completion notice). `nonce` makes a repeat click on the same task fire. */
export interface BackgroundFocusRequest {
  sessionId: string
  taskId: string
  nonce: number
}

const EMPTY: BackgroundTask[] = []

async function errorOf(res: Response, fallback: string): Promise<Error> {
  const data = await res.json().catch(() => null)
  return new Error((data && typeof data.error === 'string' && data.error) || fallback)
}

/** A finished task never goes back to running: a response that was in
 *  flight when the `finished` WS event landed carries a stale snapshot. */
function isStale(prev: BackgroundTask | undefined, next: BackgroundTask): boolean {
  return !!prev && prev.status !== 'running' && next.status === 'running'
}

/** Replace (or insert) `task` in a session's list, keeping it oldest first. */
function upsert(list: BackgroundTask[], task: BackgroundTask): BackgroundTask[] {
  const idx = list.findIndex((t) => t.id === task.id)
  if (idx >= 0) {
    if (isStale(list[idx], task)) return list
    const next = list.slice()
    next[idx] = task
    return next
  }
  return [...list, task]
}

interface BackgroundState {
  /** Tasks per session, oldest first (the server's order). */
  tasksBySession: Record<string, BackgroundTask[]>
  focusRequest: BackgroundFocusRequest | null
  fetchTasks: (sessionId: string) => Promise<void>
  /** Apply a `background_task` WS frame (`{action, task}`). */
  applyWsEvent: (sessionId: string, data: unknown) => void
  /** Output tail for one task; also refreshes the stored task. */
  fetchLog: (taskId: string, lines?: number) => Promise<{ task: BackgroundTask; lines: string[] }>
  stopTask: (taskId: string) => Promise<BackgroundTask>
  requestFocus: (sessionId: string, taskId: string) => void
}

export const useBackgroundStore = create<BackgroundState>((set, get) => {
  const store = (task: BackgroundTask) =>
    set((s) => {
      const list = s.tasksBySession[task.session_id] ?? EMPTY
      const next = upsert(list, task)
      return next === list
        ? s
        : { tasksBySession: { ...s.tasksBySession, [task.session_id]: next } }
    })
  /** The stored (possibly newer) copy of `task`, after `store(task)`. */
  const current = (task: BackgroundTask): BackgroundTask =>
    get().tasksBySession[task.session_id]?.find((t) => t.id === task.id) ?? task

  return {
    tasksBySession: {},
    focusRequest: null,

    fetchTasks: async (sessionId) => {
      try {
        const res = await authedFetch(`/api/sessions/${encodeURIComponent(sessionId)}/background`)
        if (!res.ok) return
        const data = (await res.json()) as { tasks?: BackgroundTask[] }
        const tasks = Array.isArray(data.tasks) ? data.tasks : EMPTY
        set((s) => {
          const prev = s.tasksBySession[sessionId] ?? EMPTY
          const merged = tasks.map((t) => {
            const old = prev.find((p) => p.id === t.id)
            return old && isStale(old, t) ? old : t
          })
          return { tasksBySession: { ...s.tasksBySession, [sessionId]: merged } }
        })
      } catch {
        // Best effort: the panel keeps whatever it last knew; the WS event
        // and the next session open fill it in.
      }
    },

    applyWsEvent: (sessionId, data) => {
      const task = (data as { task?: BackgroundTask } | null)?.task
      if (!task || typeof task.id !== 'string') return
      store({ ...task, session_id: task.session_id || sessionId })
    },

    fetchLog: async (taskId, lines) => {
      const q = lines ? `?lines=${lines}` : ''
      const res = await authedFetch(`/api/background/${encodeURIComponent(taskId)}/log${q}`)
      if (!res.ok) throw await errorOf(res, 'Failed to load output')
      const data = (await res.json()) as { task: BackgroundTask; lines: string[] }
      store(data.task)
      return { task: current(data.task), lines: Array.isArray(data.lines) ? data.lines : [] }
    },

    stopTask: async (taskId) => {
      const res = await authedFetch(`/api/background/${encodeURIComponent(taskId)}/stop`, {
        method: 'POST',
      })
      if (!res.ok) {
        throw await errorOf(
          res,
          res.status === 409 ? 'The task already finished' : 'Failed to stop the task',
        )
      }
      const data = (await res.json()) as { task: BackgroundTask }
      store(data.task)
      return current(data.task)
    },

    requestFocus: (sessionId, taskId) =>
      set({ focusRequest: { sessionId, taskId, nonce: Date.now() + Math.random() } }),
  }
})

export const EMPTY_BACKGROUND_TASKS = EMPTY

/** Human command line for a task (`npm run build`). */
export function taskCommandLine(t: Pick<BackgroundTask, 'program' | 'args'>): string {
  const prog = t.program.split('/').pop() || t.program
  return [prog, ...t.args].join(' ')
}

/** Short status label for badges. */
export function taskStatusLabel(t: Pick<BackgroundTask, 'status' | 'stopping'>): string {
  switch (t.status) {
    case 'running':
      return t.stopping ? 'Stopping' : 'Running'
    case 'succeeded':
      return 'Succeeded'
    case 'failed':
      return 'Failed'
    case 'timed_out':
      return 'Timed out'
    case 'stopped':
      return 'Stopped'
  }
}

/** `1m 12s` / `3s` / `1h 4m`. */
export function formatElapsed(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000))
  if (s < 60) return `${s}s`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m ${s % 60}s`
  return `${Math.floor(m / 60)}h ${m % 60}m`
}

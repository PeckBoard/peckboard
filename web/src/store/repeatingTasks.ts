import { create } from 'zustand'
import type { RepeatingScheduleKind, RepeatingTask, RepeatingTaskRun, Session } from '../types/api'
import { authedFetch } from './auth'
import { useTabsStore } from './tabs'

export type ScheduleValue =
  | { kind: 'interval'; minutes: number }
  | { kind: 'daily'; hour: number; minute: number }
  | { kind: 'weekly'; weekday: number; hour: number; minute: number }
  | { kind: 'monthly'; day: number; hour: number; minute: number }
  | { kind: 'once'; at: string }

export interface CreateRepeatingTaskInput {
  name: string
  description?: string
  folder_id: string
  prompt: string
  schedule_kind: RepeatingScheduleKind
  schedule_value: Record<string, number | string>
  model?: string | null
  effort?: string | null
  enabled?: boolean
  /** IANA zone name, or null/omitted for UTC. */
  timezone?: string | null
}

export interface UpdateRepeatingTaskInput {
  name?: string
  description?: string
  prompt?: string
  schedule_kind?: RepeatingScheduleKind
  schedule_value?: Record<string, number | string>
  model?: string | null
  effort?: string | null
  enabled?: boolean
  timezone?: string | null
}

interface RepeatingTasksState {
  tasks: RepeatingTask[]
  loaded: boolean
  sessionsByTask: Record<string, Session[]>
  runsByTask: Record<string, RepeatingTaskRun[]>
  fetchTasks: () => Promise<void>
  fetchSessionsForTask: (taskId: string) => Promise<void>
  fetchRunsForTask: (taskId: string) => Promise<void>
  createTask: (input: CreateRepeatingTaskInput) => Promise<RepeatingTask>
  updateTask: (id: string, input: UpdateRepeatingTaskInput) => Promise<RepeatingTask>
  deleteTask: (id: string) => Promise<void>
  /** Force-run. Returns the backend status string. */
  runNow: (id: string) => Promise<'spawned' | 'already_running' | 'disabled'>
  /** Apply a WS-driven mutation event (`repeating-task-changed`). */
  applyChange: (action: string, payload: { id?: string; task?: RepeatingTask }) => void
}

export const useRepeatingTasksStore = create<RepeatingTasksState>((set, get) => ({
  tasks: [],
  loaded: false,
  sessionsByTask: {},
  runsByTask: {},

  fetchTasks: async () => {
    const res = await authedFetch('/api/repeating-tasks')
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to load tasks' }))
      throw new Error(err.error || 'Failed to load tasks')
    }
    const tasks: RepeatingTask[] = await res.json()
    set({ tasks, loaded: true })
  },

  fetchSessionsForTask: async (taskId: string) => {
    const res = await authedFetch(`/api/repeating-tasks/${taskId}/sessions`)
    if (!res.ok) {
      // 404 means the task itself was deleted — just drop the cache.
      if (res.status === 404) {
        set((s) => {
          const next = { ...s.sessionsByTask }
          delete next[taskId]
          return { sessionsByTask: next }
        })
        return
      }
      const err = await res.json().catch(() => ({ error: 'Failed to load sessions' }))
      throw new Error(err.error || 'Failed to load sessions')
    }
    const sessions: Session[] = await res.json()
    set((s) => ({ sessionsByTask: { ...s.sessionsByTask, [taskId]: sessions } }))
  },

  fetchRunsForTask: async (taskId: string) => {
    const res = await authedFetch(`/api/repeating-tasks/${taskId}/runs`)
    if (!res.ok) {
      if (res.status === 404) {
        set((s) => {
          const next = { ...s.runsByTask }
          delete next[taskId]
          return { runsByTask: next }
        })
        return
      }
      const err = await res.json().catch(() => ({ error: 'Failed to load run history' }))
      throw new Error(err.error || 'Failed to load run history')
    }
    const runs: RepeatingTaskRun[] = await res.json()
    set((s) => ({ runsByTask: { ...s.runsByTask, [taskId]: runs } }))
  },

  createTask: async (input: CreateRepeatingTaskInput) => {
    const res = await authedFetch('/api/repeating-tasks', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(input),
    })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to create task' }))
      throw new Error(err.error || 'Failed to create task')
    }
    const task: RepeatingTask = await res.json()
    set((s) => ({ tasks: [...s.tasks, task] }))
    return task
  },

  updateTask: async (id: string, input: UpdateRepeatingTaskInput) => {
    const res = await authedFetch(`/api/repeating-tasks/${id}`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(input),
    })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to update task' }))
      throw new Error(err.error || 'Failed to update task')
    }
    const task: RepeatingTask = await res.json()
    set((s) => ({ tasks: s.tasks.map((t) => (t.id === id ? task : t)) }))
    return task
  },

  deleteTask: async (id: string) => {
    const res = await authedFetch(`/api/repeating-tasks/${id}`, { method: 'DELETE' })
    if (!res.ok && res.status !== 404) {
      const err = await res.json().catch(() => ({ error: 'Failed to delete task' }))
      throw new Error(err.error || 'Failed to delete task')
    }
    set((s) => {
      const sessions = { ...s.sessionsByTask }
      delete sessions[id]
      const runs = { ...s.runsByTask }
      delete runs[id]
      return {
        tasks: s.tasks.filter((t) => t.id !== id),
        sessionsByTask: sessions,
        runsByTask: runs,
      }
    })
    // Drop the strip chip immediately. The server-side cascade in
    // delete_repeating_task removed the row already; this is the
    // local-mirror cleanup + the in-flight `openTab` tombstone guard
    // that stops a racing POST from resurrecting a phantom chip.
    useTabsStore.getState().removeTabsForItem('repeating_task', id)
  },

  runNow: async (id: string) => {
    const res = await authedFetch(`/api/repeating-tasks/${id}/run`, { method: 'POST' })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to run task' }))
      throw new Error(err.error || 'Failed to run task')
    }
    const body = (await res.json()) as { status: 'spawned' | 'already_running' | 'disabled' }
    // Optimistically refresh the sessions + run history so the new run
    // shows up without waiting for a navigation away and back.
    void get().fetchSessionsForTask(id)
    void get().fetchRunsForTask(id)
    return body.status
  },

  applyChange: (action: string, payload: { id?: string; task?: RepeatingTask }) => {
    if (action === 'deleted' && payload.id) {
      get()
        .deleteTask(payload.id)
        .catch(() => {
          /* already removed locally */
        })
      return
    }
    if (!payload.task) return
    const task = payload.task
    set((s) => {
      const idx = s.tasks.findIndex((t) => t.id === task.id)
      if (idx === -1) {
        return { tasks: [...s.tasks, task] }
      }
      const tasks = s.tasks.slice()
      tasks[idx] = task
      return { tasks }
    })
  },
}))

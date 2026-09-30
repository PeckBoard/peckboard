import { create } from 'zustand'
import { authedFetch } from './auth'

/** Mirrors `service::restart::Activity` from `GET /api/admin/activity`. */
export interface RestartActivity {
  sessions: {
    session_id: string
    name: string
    folder_name: string | null
    project_name: string | null
    running_since: string | null
    running_secs: number | null
  }[]
  subagents: {
    session_id: string
    name: string
    parent_session_id: string
    parent_name: string | null
    running_secs: number | null
  }[]
  workers: {
    session_id: string
    session_name: string
    card_id: string
    card_title: string | null
    project_id: string | null
    project_name: string | null
    step: string | null
    running_secs: number | null
  }[]
  background_tasks: {
    task_id: string
    label: string
    command: string
    session_id: string
    session_name: string | null
    running_secs: number | null
  }[]
  counts: { sessions: number; subagents: number; workers: number; background_tasks: number }
  total: number
}

/** A plain restart, or an update (binary already swapped) awaiting its re-exec. */
export type RestartKind = 'restart' | 'update'

/** Mirrors `service::restart::PendingRestart`. */
export interface PendingRestart {
  kind: RestartKind
  version: string | null
  requested_at: string
  /** In-flight items at the server's last poll. */
  remaining: number
}

async function jsonOrThrow(res: Response) {
  const data = await res.json().catch(() => ({}))
  if (!res.ok) throw new Error(data?.error || `HTTP ${res.status}`)
  return data
}

export async function fetchRestartActivity(): Promise<RestartActivity> {
  return (await jsonOrThrow(await authedFetch('/api/admin/activity'))) as RestartActivity
}

/** Restart now, or park it until idle. An update also downloads + swaps the
 *  binary first, so it is the update's own apply route. */
export async function requestRestart(kind: RestartKind, when: 'now' | 'idle'): Promise<void> {
  const base = kind === 'update' ? '/api/update/apply' : '/api/admin/restart'
  const url = when === 'idle' ? `${base}?when=idle` : base
  await jsonOrThrow(await authedFetch(url, { method: 'POST' }))
}

/** After a restart the server is briefly unreachable. Poll the public health
 *  route until it answers again, then reload so the page (and, after an
 *  update, the new embedded frontend) comes back fresh. Resolves false when
 *  it gave up waiting. */
export async function waitForServerThenReload(): Promise<boolean> {
  for (let i = 0; i < 60; i++) {
    await new Promise((r) => setTimeout(r, 2000))
    try {
      const res = await fetch('/api/health', { cache: 'no-store' })
      if (res.ok) {
        window.location.reload()
        return true
      }
    } catch {
      // still restarting — keep polling
    }
  }
  return false
}

interface RestartState {
  pending: PendingRestart | null
  /** The server announced it is restarting right now. */
  restarting: boolean
  /** Apply a `restart-pending` WS frame's payload. */
  applyEvent: (data: { pending?: PendingRestart | null; restarting?: boolean }) => void
  /** Load the pending restart (admin-only route; others stay null). */
  refresh: () => Promise<void>
  cancel: () => Promise<void>
}

export const useRestartStore = create<RestartState>((set) => ({
  pending: null,
  restarting: false,
  applyEvent: (data) => {
    set({ pending: data.pending ?? null, restarting: !!data.restarting })
  },
  refresh: async () => {
    try {
      const res = await authedFetch('/api/admin/restart')
      if (!res.ok) return
      const data = await res.json()
      set({ pending: (data?.pending as PendingRestart | null) ?? null })
    } catch {
      // Not reachable / not an admin: nothing to show.
    }
  },
  cancel: async () => {
    await jsonOrThrow(await authedFetch('/api/admin/restart', { method: 'DELETE' }))
    set({ pending: null })
  },
}))

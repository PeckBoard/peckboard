import { create } from 'zustand'
import { authedFetch } from './auth'

/** Where a terminal's shell is (mirrors `crate::terminal::Phase`). `idle` =
 *  not connected right now; opening it reconnects. */
export type TerminalPhase = 'idle' | 'connecting' | 'live' | 'reconnecting' | 'ended' | 'error'

export interface TerminalStatus {
  phase: TerminalPhase
  /** Runs inside tmux (survives restarts). `null` until the first connect. */
  persistent: boolean | null
  message: string | null
}

export interface TerminalInfo {
  id: string
  name: string
  plugin_id: string
  host_id: string
  /** `user@host:port` — identity only. */
  host_label: string
  persistent: boolean
  created_at: string
  last_active_at: string
  status: TerminalStatus
}

/** A host a plugin (SSH Fleet) offers for terminals. */
export interface TerminalHost {
  plugin_id: string
  id: string
  label: string
  hostname: string
  username: string
  port: number
  tags: string[]
}

async function errorOf(res: Response, fallback: string): Promise<Error> {
  try {
    const body = (await res.json()) as { error?: string }
    return new Error(body.error || fallback)
  } catch {
    return new Error(fallback)
  }
}
export const PHASE_LABEL: Record<TerminalPhase, string> = {
  idle: 'Detached',
  connecting: 'Connecting',
  live: 'Live',
  reconnecting: 'Reconnecting',
  ended: 'Ended',
  error: 'Error',
}

interface TerminalsState {
  terminals: TerminalInfo[]
  loaded: boolean
  fetchTerminals: () => Promise<void>
  fetchHosts: () => Promise<TerminalHost[]>
  /** Open a new terminal on a plugin's host. */
  create: (pluginId: string, hostId: string) => Promise<TerminalInfo>
  rename: (id: string, name: string) => Promise<void>
  close: (id: string) => Promise<void>
}

export const useTerminalsStore = create<TerminalsState>((set, get) => ({
  terminals: [],
  loaded: false,

  fetchTerminals: async () => {
    try {
      const res = await authedFetch('/api/terminals')
      if (!res.ok) return
      set({ terminals: (await res.json()) as TerminalInfo[], loaded: true })
    } catch {
      // Non-fatal: the list keeps what it had.
    }
  },

  fetchHosts: async () => {
    const res = await authedFetch('/api/terminals/hosts')
    if (!res.ok) throw await errorOf(res, "Couldn't load hosts.")
    return (await res.json()) as TerminalHost[]
  },

  create: async (pluginId, hostId) => {
    const res = await authedFetch('/api/terminals', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ plugin_id: pluginId, host_id: hostId }),
    })
    if (!res.ok) throw await errorOf(res, "Couldn't open a terminal.")
    const t = (await res.json()) as TerminalInfo
    set((s) => ({ terminals: [t, ...s.terminals.filter((x) => x.id !== t.id)] }))
    return t
  },

  rename: async (id, name) => {
    const res = await authedFetch(`/api/terminals/${encodeURIComponent(id)}`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name }),
    })
    if (!res.ok) throw await errorOf(res, 'Rename failed')
    const t = (await res.json()) as TerminalInfo
    set((s) => ({ terminals: s.terminals.map((x) => (x.id === id ? t : x)) }))
  },

  close: async (id) => {
    const res = await authedFetch(`/api/terminals/${encodeURIComponent(id)}`, {
      method: 'DELETE',
    })
    if (!res.ok && res.status !== 404) throw await errorOf(res, "Couldn't close the terminal.")
    set({ terminals: get().terminals.filter((x) => x.id !== id) })
  },
}))

/** Open a terminal in its own chrome-less browser window (same live shell). */
export function popOutTerminal(id: string) {
  window.open(
    `/terminal/${encodeURIComponent(id)}`,
    `peckboard-terminal-${id}`,
    'popup,width=1000,height=640',
  )
}

import { create } from 'zustand'
import type { Session } from '../types/api'
import { authedFetch } from './auth'

interface SessionsState {
  sessions: Session[]
  activeSessionId: string | null
  fetchSessions: () => Promise<void>
  createSession: (name: string, folderId: string) => Promise<Session>
  deleteSession: (id: string) => Promise<void>
  setActiveSession: (id: string | null) => void
}

export const useSessionsStore = create<SessionsState>((set) => ({
  sessions: [],
  activeSessionId: null,

  fetchSessions: async () => {
    const res = await authedFetch('/api/sessions')
    if (res.ok) {
      const sessions: Session[] = await res.json()
      set({ sessions })
    }
  },

  createSession: async (name: string, folderId: string) => {
    const res = await authedFetch('/api/sessions', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name, folder_id: folderId }),
    })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to create session' }))
      throw new Error(err.error || 'Failed to create session')
    }
    const session: Session = await res.json()
    set((s) => ({ sessions: [...s.sessions, session] }))
    return session
  },

  deleteSession: async (id: string) => {
    const res = await authedFetch(`/api/sessions/${id}`, { method: 'DELETE' })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to delete session' }))
      throw new Error(err.error || 'Failed to delete session')
    }
    set((s) => ({
      sessions: s.sessions.filter((sess) => sess.id !== id),
      activeSessionId: s.activeSessionId === id ? null : s.activeSessionId,
    }))
  },

  setActiveSession: (id: string | null) => {
    set({ activeSessionId: id })
  },
}))

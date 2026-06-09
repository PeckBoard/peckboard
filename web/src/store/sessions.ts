import { create } from 'zustand'
import type { Event, Expert, Session } from '../types/api'
import { authedFetch } from './auth'
import { useTabsStore } from './tabs'

const DRAFTS_KEY = 'peckboard_drafts'

function loadDrafts(): Record<string, string> {
  try {
    const raw = localStorage.getItem(DRAFTS_KEY)
    if (raw) return JSON.parse(raw) as Record<string, string>
  } catch {
    /* ignore */
  }
  return {}
}

function saveDrafts(drafts: Record<string, string>) {
  try {
    localStorage.setItem(DRAFTS_KEY, JSON.stringify(drafts))
  } catch {
    /* ignore */
  }
}

interface SessionsState {
  sessions: Session[]
  /** True once `fetchSessions` has completed successfully at least
   *  once. Consumers that want to reason about "is this session id
   *  real?" must wait for this — otherwise the empty initial state
   *  looks identical to "every session was deleted". */
  sessionsLoaded: boolean
  activeSessionId: string | null
  inputDrafts: Record<string, string>
  processing: Set<string>
  unreadSessions: Set<string>
  eventsBySession: Record<string, Event[]>
  loadingEventsBySession: Record<string, boolean>
  /** Expert sessions (is_expert = true) from GET /api/experts. Kept
   *  separate from `sessions` because experts are deliberately hidden
   *  from the ordinary chat list and only surface in the Experts view. */
  experts: Expert[]
  expertsLoaded: boolean
  fetchSessions: () => Promise<void>
  fetchExperts: () => Promise<void>
  fetchEvents: (sessionId: string) => Promise<void>
  appendEvent: (event: Event) => void
  createSession: (
    name: string,
    folderId: string,
    model?: string,
    effort?: string,
  ) => Promise<Session>
  deleteSession: (id: string) => Promise<void>
  setActiveSession: (id: string | null) => void
  renameSession: (id: string, name: string) => Promise<void>
  clearSession: (id: string) => Promise<void>
  cancelSession: (id: string) => Promise<void>
  interruptSession: (id: string) => Promise<void>
  setDraft: (sessionId: string, text: string) => void
  getDraft: (sessionId: string) => string
  handleEvent: (event: Event) => void
  markSessionRead: (sessionId: string) => void
}

export const useSessionsStore = create<SessionsState>((set, get) => ({
  sessions: [],
  sessionsLoaded: false,
  activeSessionId: null,
  inputDrafts: loadDrafts(),
  processing: new Set<string>(),
  unreadSessions: new Set<string>(),
  eventsBySession: {},
  loadingEventsBySession: {},
  experts: [],
  expertsLoaded: false,

  fetchSessions: async () => {
    const res = await authedFetch('/api/sessions')
    if (res.ok) {
      const sessions: Session[] = await res.json()
      set({ sessions, sessionsLoaded: true })
    }
  },

  fetchExperts: async () => {
    const res = await authedFetch('/api/experts')
    if (res.ok) {
      const experts: Expert[] = await res.json()
      set({ experts, expertsLoaded: true })
    }
  },

  fetchEvents: async (sessionId: string) => {
    set((s) => ({
      loadingEventsBySession: { ...s.loadingEventsBySession, [sessionId]: true },
      eventsBySession: { ...s.eventsBySession, [sessionId]: [] },
    }))
    try {
      const res = await authedFetch(`/api/sessions/${sessionId}/events`)
      const events: Event[] = res.ok ? await res.json() : []
      set((s) => ({
        eventsBySession: { ...s.eventsBySession, [sessionId]: events },
        loadingEventsBySession: { ...s.loadingEventsBySession, [sessionId]: false },
      }))
    } catch {
      set((s) => ({
        loadingEventsBySession: { ...s.loadingEventsBySession, [sessionId]: false },
      }))
    }
  },

  appendEvent: (event: Event) => {
    set((s) => {
      const existing = s.eventsBySession[event.session_id] ?? []
      if (existing.some((e) => e.id === event.id)) return s
      return {
        eventsBySession: {
          ...s.eventsBySession,
          [event.session_id]: [...existing, event],
        },
      }
    })
  },

  createSession: async (name: string, folderId: string, model?: string, effort?: string) => {
    const body: Record<string, string> = { name, folder_id: folderId }
    if (model) body.model = model
    if (effort && effort !== 'default') body.effort = effort
    const res = await authedFetch('/api/sessions', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
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
    set((s) => {
      const { [id]: _drop, ...remainingEvents } = s.eventsBySession
      void _drop
      return {
        sessions: s.sessions.filter((sess) => sess.id !== id),
        activeSessionId: s.activeSessionId === id ? null : s.activeSessionId,
        eventsBySession: remainingEvents,
      }
    })
    // Drop the tab for the now-deleted session so it doesn't render as
    // a ghost chip labelled "Session" (the label falls back when the
    // session row is gone). The backend also nukes its `user_tabs` row,
    // so the next cross-device refetch stays consistent.
    useTabsStore.getState().removeTabsForItem('session', id)
  },

  setActiveSession: (id: string | null) => {
    set({ activeSessionId: id })
    if (id) {
      get().markSessionRead(id)
    }
  },

  renameSession: async (id: string, name: string) => {
    const res = await authedFetch(`/api/sessions/${id}`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name }),
    })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to rename session' }))
      throw new Error(err.error || 'Failed to rename session')
    }
    set((s) => ({
      sessions: s.sessions.map((sess) => (sess.id === id ? { ...sess, name } : sess)),
    }))
  },

  clearSession: async (id: string) => {
    const res = await authedFetch(`/api/sessions/${id}/clear`, { method: 'POST' })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to clear session' }))
      throw new Error(err.error || 'Failed to clear session')
    }
    set((s) => ({
      eventsBySession: { ...s.eventsBySession, [id]: [] },
    }))
  },

  cancelSession: async (id: string) => {
    const res = await authedFetch(`/api/sessions/${id}/cancel`, { method: 'POST' })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to cancel session' }))
      throw new Error(err.error || 'Failed to cancel session')
    }
  },

  interruptSession: async (id: string) => {
    const res = await authedFetch(`/api/sessions/${id}/interrupt`, { method: 'POST' })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to interrupt session' }))
      throw new Error(err.error || 'Failed to interrupt session')
    }
  },

  setDraft: (sessionId: string, text: string) => {
    const drafts = { ...get().inputDrafts, [sessionId]: text }
    if (!text) delete drafts[sessionId]
    saveDrafts(drafts)
    set({ inputDrafts: drafts })
  },

  getDraft: (sessionId: string) => {
    return get().inputDrafts[sessionId] ?? ''
  },

  handleEvent: (event: Event) => {
    const { processing, unreadSessions, activeSessionId } = get()
    const sid = event.session_id

    if (event.kind === 'agent-start') {
      if (!processing.has(sid)) {
        const next = new Set(processing)
        next.add(sid)
        set({ processing: next })
      }
    } else if (event.kind === 'agent-end' || event.kind === 'agent-error') {
      if (processing.has(sid)) {
        const next = new Set(processing)
        next.delete(sid)
        set({ processing: next })
      }
      // Mark as unread if completed and not the currently viewed session
      const status = (event.data.status as string) ?? ''
      if (event.kind === 'agent-end' && status === 'complete' && sid !== activeSessionId) {
        const nextUnread = new Set(unreadSessions)
        nextUnread.add(sid)
        set({ unreadSessions: nextUnread })
      }
    }
  },

  markSessionRead: (sessionId: string) => {
    const { unreadSessions } = get()
    if (unreadSessions.has(sessionId)) {
      const next = new Set(unreadSessions)
      next.delete(sessionId)
      set({ unreadSessions: next })
    }
    // Fire-and-forget POST to mark read on the server
    authedFetch(`/api/sessions/${sessionId}/read`, { method: 'POST' }).catch(() => {})
  },
}))

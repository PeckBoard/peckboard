import { create } from 'zustand'
import type { Event, Session } from '../types/api'
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

/**
 * When a real `user` event arrives, remove the FIFO-first matching
 * optimistic bubble for that session (matched by exact text). Returns a
 * partial state update — caller spreads it into the next set() patch.
 *
 * Exact-text match is intentional: two identical messages sent in rapid
 * succession each get their own pending entry, and we resolve them in
 * order. Mismatched text leaves the bubble in place; it'll age out via
 * the staleness sweep in ChatView.
 */
function clearMatchingPending(
  state: { pendingUserMessages: Record<string, PendingUserMessage[]> },
  event: Event,
): Partial<{ pendingUserMessages: Record<string, PendingUserMessage[]> }> {
  if (event.kind !== 'user') return {}
  const list = state.pendingUserMessages[event.session_id]
  if (!list || list.length === 0) return {}
  const text = (event.data.text as string) ?? ''
  const idx = list.findIndex((p) => p.text === text)
  if (idx === -1) return {}
  const next = [...list.slice(0, idx), ...list.slice(idx + 1)]
  return {
    pendingUserMessages: { ...state.pendingUserMessages, [event.session_id]: next },
  }
}

/**
 * Optimistic user message: rendered in the chat as soon as the user hits
 * Send, before the WS event for the persisted `user` row arrives. Without
 * this the composer clears but the bubble doesn't appear until the round-
 * trip completes — for queued turns that gap can be hundreds of ms and
 * makes the message look lost.
 */
export interface PendingUserMessage {
  tempId: string
  text: string
  ts: number
  /** Attachments carried by this turn, so the optimistic bubble shows the
   *  same "image attached" indicator the persisted `user` event will. */
  attachments?: { filename: string; mimeType: string }[]
}

/** Cursor returned by `GET /api/sessions`. Pass back as-is to fetch
 *  the next page. `null` means we've reached the end of the list. */
export interface SessionsCursor {
  last_activity: string
  id: string
}

interface SessionsState {
  sessions: Session[]
  /** True once `fetchSessions` has completed successfully at least
   *  once. Consumers that want to reason about "is this session id
   *  real?" must wait for this — otherwise the empty initial state
   *  looks identical to "every session was deleted". */
  sessionsLoaded: boolean
  /** Cursor for the next page of plain chat sessions, or `null` if
   *  we've loaded everything. Scroll-to-bottom in the session list
   *  calls `fetchMoreSessions` while this is non-null. */
  sessionsNextCursor: SessionsCursor | null
  /** True while a `fetchMoreSessions` request is in flight. Used to
   *  debounce repeated scroll triggers — the sidebar can fire a
   *  half-dozen scroll events while waiting on the previous fetch. */
  sessionsLoadingMore: boolean
  activeSessionId: string | null
  inputDrafts: Record<string, string>
  processing: Set<string>
  unreadSessions: Set<string>
  eventsBySession: Record<string, Event[]>
  loadingEventsBySession: Record<string, boolean>
  /** True when the initial event fetch failed (network error or
   *  non-2xx). ChatView renders a retry pane instead of silently
   *  showing an empty conversation. */
  eventsErrorBySession: Record<string, boolean>
  /** True while a "Load older" request is in flight for `sessionId`.
   *  ChatView disables the button while this is set so repeated
   *  clicks can't pile up duplicate requests for the same page. */
  loadingOlderEventsBySession: Record<string, boolean>
  /** False once we know there are no more older events for a session
   *  (a page came back shorter than the requested size). Defaults to
   *  true on first load so the "Load older" button shows; flipped to
   *  false the first time a partial page arrives. */
  hasMoreOlderEventsBySession: Record<string, boolean>
  pendingUserMessages: Record<string, PendingUserMessage[]>
  fetchSessions: () => Promise<void>
  fetchMoreSessions: () => Promise<void>
  fetchEvents: (sessionId: string) => Promise<void>
  fetchOlderEvents: (sessionId: string) => Promise<void>
  appendEvent: (event: Event) => void
  /** Optimistically register a user-typed message for `sessionId`.
   *  ChatView renders it as a user bubble immediately. The matching
   *  `user` event arriving over the WS auto-clears it. Returns the
   *  `tempId` so the caller can roll back manually if the POST
   *  errors and no `user` event will ever arrive. */
  addPendingUserMessage: (
    sessionId: string,
    text: string,
    attachments?: { filename: string; mimeType: string }[],
  ) => string
  /** Remove a single pending user-message entry by id — used by the
   *  send path when the POST fails so the optimistic bubble doesn't
   *  hang around with no hope of being confirmed. */
  removePendingUserMessage: (sessionId: string, tempId: string) => void
  /** Drop pending entries that are clearly orphaned — the POST
   *  succeeded but the WS `user` event never arrived (server crashed
   *  mid-broadcast, etc.) and the entry has been sitting there for
   *  longer than `maxAgeMs`. ChatView calls this on a ~10s tick. */
  prunePendingUserMessages: (maxAgeMs: number) => void
  createSession: (
    name: string,
    folderId: string,
    model?: string,
    effort?: string,
  ) => Promise<Session>
  deleteSession: (id: string) => Promise<void>
  /** Apply the local cleanup for a session that's been deleted on the
   *  server — either by THIS client (via `deleteSession`) or another
   *  device (via the `session-deleted` WS broadcast). Drops the session
   *  row, clears `activeSessionId` if it matched, wipes cached events
   *  and pending bubbles, and closes the tab strip entry so the body
   *  switches off ChatView when the deleted session was active. */
  applySessionDeleted: (id: string) => void
  setActiveSession: (id: string | null) => void
  renameSession: (id: string, name: string) => Promise<void>
  clearSession: (id: string) => Promise<void>
  cancelSession: (id: string) => Promise<void>
  interruptSession: (id: string) => Promise<void>
  terminateAgent: (id: string) => Promise<void>
  setDraft: (sessionId: string, text: string) => void
  getDraft: (sessionId: string) => string
  handleEvent: (event: Event) => void
  markSessionRead: (sessionId: string) => void
}

/** Default page size requested from the backend. Matches the server's
 *  `DEFAULT_SESSION_PAGE_SIZE` — kept in sync explicitly because the
 *  numbers are tuned together (visible viewport + scroll-load trigger
 *  distance both assume ~100 rows per page). */
const SESSIONS_PAGE_SIZE = 100

/** Default chat events fetched on session open. Matches the server's
 *  `DEFAULT_EVENTS_PAGE_SIZE`. The "Load older messages" button pulls
 *  another page-full per click. */
const EVENTS_PAGE_SIZE = 200

export const useSessionsStore = create<SessionsState>((set, get) => ({
  sessions: [],
  sessionsLoaded: false,
  sessionsNextCursor: null,
  sessionsLoadingMore: false,
  activeSessionId: null,
  inputDrafts: loadDrafts(),
  processing: new Set<string>(),
  unreadSessions: new Set<string>(),
  eventsBySession: {},
  loadingEventsBySession: {},
  eventsErrorBySession: {},
  loadingOlderEventsBySession: {},
  hasMoreOlderEventsBySession: {},
  pendingUserMessages: {},

  fetchSessions: async () => {
    const res = await authedFetch(`/api/sessions?limit=${SESSIONS_PAGE_SIZE}`)
    if (res.ok) {
      const body: { items: Session[]; next_cursor: SessionsCursor | null } = await res.json()
      set({
        sessions: body.items,
        sessionsNextCursor: body.next_cursor,
        sessionsLoaded: true,
      })
    }
  },

  fetchMoreSessions: async () => {
    const { sessionsNextCursor, sessionsLoadingMore } = get()
    if (!sessionsNextCursor || sessionsLoadingMore) return
    set({ sessionsLoadingMore: true })
    try {
      const params = new URLSearchParams({
        limit: String(SESSIONS_PAGE_SIZE),
        cursor_la: sessionsNextCursor.last_activity,
        cursor_id: sessionsNextCursor.id,
      })
      const res = await authedFetch(`/api/sessions?${params.toString()}`)
      if (!res.ok) {
        set({ sessionsLoadingMore: false })
        return
      }
      const body: { items: Session[]; next_cursor: SessionsCursor | null } = await res.json()
      // De-dupe against rows we already have. The keyset filter
      // shouldn't produce duplicates, but a row whose last_activity
      // bumped after the previous page can land twice across a
      // boundary — drop those rather than render a duplicate chip.
      set((s) => {
        const existing = new Set(s.sessions.map((x) => x.id))
        const fresh = body.items.filter((x) => !existing.has(x.id))
        return {
          sessions: [...s.sessions, ...fresh],
          sessionsNextCursor: body.next_cursor,
          sessionsLoadingMore: false,
        }
      })
    } catch {
      set({ sessionsLoadingMore: false })
    }
  },

  fetchEvents: async (sessionId: string) => {
    set((s) => ({
      loadingEventsBySession: { ...s.loadingEventsBySession, [sessionId]: true },
      eventsErrorBySession: { ...s.eventsErrorBySession, [sessionId]: false },
      eventsBySession: { ...s.eventsBySession, [sessionId]: [] },
      // Reset older-page bookkeeping for a fresh session open. We
      // assume "more history exists" until a short page proves
      // otherwise; the "Load older" button hides immediately when
      // hasMore flips false.
      hasMoreOlderEventsBySession: {
        ...s.hasMoreOlderEventsBySession,
        [sessionId]: true,
      },
    }))
    try {
      const res = await authedFetch(`/api/sessions/${sessionId}/events?limit=${EVENTS_PAGE_SIZE}`)
      if (!res.ok) throw new Error(`events fetch failed: ${res.status}`)
      const events: Event[] = await res.json()
      set((s) => {
        // Merge rather than replace: events broadcast over the WS while
        // this fetch was in flight have already been appended to the
        // store but may post-date the HTTP snapshot — a wholesale
        // replace would silently drop them.
        const fetchedIds = new Set(events.map((e) => e.id))
        const liveExtras = (s.eventsBySession[sessionId] ?? []).filter((e) => !fetchedIds.has(e.id))
        const merged = [...events, ...liveExtras].sort((a, b) => a.seq - b.seq)
        return {
          eventsBySession: { ...s.eventsBySession, [sessionId]: merged },
          loadingEventsBySession: { ...s.loadingEventsBySession, [sessionId]: false },
          // If the first page came back short, there are no older
          // events to load — hide the button up-front.
          hasMoreOlderEventsBySession: {
            ...s.hasMoreOlderEventsBySession,
            [sessionId]: events.length >= EVENTS_PAGE_SIZE,
          },
        }
      })
    } catch {
      set((s) => ({
        loadingEventsBySession: { ...s.loadingEventsBySession, [sessionId]: false },
        eventsErrorBySession: { ...s.eventsErrorBySession, [sessionId]: true },
      }))
    }
  },

  fetchOlderEvents: async (sessionId: string) => {
    const state = get()
    if (state.loadingOlderEventsBySession[sessionId]) return
    if (state.hasMoreOlderEventsBySession[sessionId] === false) return
    const existing = state.eventsBySession[sessionId] ?? []
    if (existing.length === 0) return
    // Lowest seq we currently have is the upper bound for the next page.
    const oldestSeq = existing.reduce((min, e) => (e.seq < min ? e.seq : min), existing[0].seq)
    set((s) => ({
      loadingOlderEventsBySession: { ...s.loadingOlderEventsBySession, [sessionId]: true },
    }))
    try {
      const url = `/api/sessions/${sessionId}/events?before_seq=${oldestSeq}&limit=${EVENTS_PAGE_SIZE}`
      const res = await authedFetch(url)
      const older: Event[] = res.ok ? await res.json() : []
      set((s) => {
        const current = s.eventsBySession[sessionId] ?? []
        const seen = new Set(current.map((e) => e.id))
        const fresh = older.filter((e) => !seen.has(e.id))
        return {
          eventsBySession: {
            ...s.eventsBySession,
            // Server returned ascending; splice in front so the
            // existing list stays globally ascending too.
            [sessionId]: [...fresh, ...current],
          },
          loadingOlderEventsBySession: {
            ...s.loadingOlderEventsBySession,
            [sessionId]: false,
          },
          // A short page (less than what we asked for) is the
          // unambiguous "end of history" signal.
          hasMoreOlderEventsBySession: {
            ...s.hasMoreOlderEventsBySession,
            [sessionId]: older.length >= EVENTS_PAGE_SIZE,
          },
        }
      })
    } catch {
      set((s) => ({
        loadingOlderEventsBySession: { ...s.loadingOlderEventsBySession, [sessionId]: false },
      }))
    }
  },

  appendEvent: (event: Event) => {
    set((s) => {
      const existing = s.eventsBySession[event.session_id] ?? []
      if (existing.some((e) => e.id === event.id)) {
        // Already in the log — still clear any matching optimistic
        // bubble (the duplicate path is rare but happens on resume).
        return clearMatchingPending(s, event)
      }
      const nextEvents = {
        eventsBySession: {
          ...s.eventsBySession,
          [event.session_id]: [...existing, event],
        },
      }
      return { ...nextEvents, ...clearMatchingPending(s, event) }
    })
  },

  addPendingUserMessage: (
    sessionId: string,
    text: string,
    attachments?: { filename: string; mimeType: string }[],
  ) => {
    // Allow an attachment-only turn (image with no caption) to still show
    // an optimistic bubble — otherwise an image send looks like nothing
    // happened until the WS `user` event lands.
    if (!text && (!attachments || attachments.length === 0)) return ''
    const entry: PendingUserMessage = {
      tempId: `pending-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
      text,
      ts: Date.now(),
      attachments: attachments && attachments.length > 0 ? attachments : undefined,
    }
    set((s) => ({
      pendingUserMessages: {
        ...s.pendingUserMessages,
        [sessionId]: [...(s.pendingUserMessages[sessionId] ?? []), entry],
      },
    }))
    return entry.tempId
  },

  removePendingUserMessage: (sessionId: string, tempId: string) => {
    set((s) => {
      const list = s.pendingUserMessages[sessionId]
      if (!list) return s
      const next = list.filter((p) => p.tempId !== tempId)
      if (next.length === list.length) return s
      return {
        pendingUserMessages: { ...s.pendingUserMessages, [sessionId]: next },
      }
    })
  },

  prunePendingUserMessages: (maxAgeMs: number) => {
    const cutoff = Date.now() - maxAgeMs
    set((s) => {
      let changed = false
      const next: Record<string, PendingUserMessage[]> = {}
      for (const [sid, list] of Object.entries(s.pendingUserMessages)) {
        const kept = list.filter((p) => p.ts >= cutoff)
        if (kept.length !== list.length) changed = true
        if (kept.length > 0) next[sid] = kept
      }
      if (!changed) return s
      return { pendingUserMessages: next }
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
    // Optimistic local cleanup so the UI updates instantly instead of
    // waiting for the `session-deleted` broadcast round-trip. The
    // broadcast handler calls `applySessionDeleted` again for every
    // connected client (including this one) — it's idempotent.
    get().applySessionDeleted(id)
  },

  applySessionDeleted: (id: string) => {
    set((s) => {
      const { [id]: _drop, ...remainingEvents } = s.eventsBySession
      const { [id]: _dropPending, ...remainingPending } = s.pendingUserMessages
      void _drop
      void _dropPending
      // Clear unread + processing too — leftover keys for a deleted
      // session would otherwise stick around as a leaked Set entry the
      // next session list never overwrites.
      const nextUnread = s.unreadSessions.has(id)
        ? new Set([...s.unreadSessions].filter((sid) => sid !== id))
        : s.unreadSessions
      const nextProcessing = s.processing.has(id)
        ? new Set([...s.processing].filter((sid) => sid !== id))
        : s.processing
      return {
        sessions: s.sessions.filter((sess) => sess.id !== id),
        activeSessionId: s.activeSessionId === id ? null : s.activeSessionId,
        eventsBySession: remainingEvents,
        pendingUserMessages: remainingPending,
        unreadSessions: nextUnread,
        processing: nextProcessing,
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
    set((s) => {
      const { [id]: _drop, ...remainingPending } = s.pendingUserMessages
      void _drop
      return {
        eventsBySession: { ...s.eventsBySession, [id]: [] },
        pendingUserMessages: remainingPending,
      }
    })
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

  terminateAgent: async (id: string) => {
    const res = await authedFetch(`/api/sessions/${id}/terminate`, { method: 'POST' })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to terminate agent' }))
      throw new Error(err.error || 'Failed to terminate agent')
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

    // Optimistic bubble clearing is intentionally NOT done here.
    // handleEvent runs from the WS layer before `appendEvent`, which
    // would briefly remove the pending entry before the real event
    // lands in `eventsBySession` — that gap causes a one-frame flicker
    // where the user bubble disappears. `appendEvent` itself does the
    // pending clear atomically with the event-list update.

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

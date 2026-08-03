import { create } from 'zustand'
import type { Event } from '../types/api'
import { useUiStore } from './ui'
import { useSessionsStore } from './sessions'
import { appendEventOrdered, nextLastSeq } from './eventOrder'
import { getToken } from './auth'

const SEQ_KEY = 'peckboard_last_seq'

type EventListener = (event: Event) => void

function loadLastSeqs(): Record<string, number> {
  try {
    const raw = sessionStorage.getItem(SEQ_KEY)
    if (raw) return JSON.parse(raw)
  } catch {
    /* ignore */
  }
  return {}
}

function saveLastSeqs(seqs: Record<string, number>) {
  try {
    sessionStorage.setItem(SEQ_KEY, JSON.stringify(seqs))
  } catch {
    /* ignore */
  }
}

interface WsState {
  eventsBySession: Record<string, Event[]>
  lastSeqBySession: Record<string, number>
  subscribedSessions: Set<string>
  /** Sessions whose `subscribe`/`resume` the server refused, mapped to the
   *  reason it gave. The server used to drop a refused frame silently, which
   *  left the chat pane waiting forever on a stream that never opened — the
   *  UI reads this to show an error state instead. */
  deniedSessions: Record<string, string>
  connect: () => void
  disconnect: () => void
  subscribe: (sessionId: string) => void
  unsubscribe: (sessionId: string) => void
  resume: (sessionId: string, lastSeq: number) => void
  addEventListener: (listener: EventListener) => void
  removeEventListener: (listener: EventListener) => void
}

let socket: WebSocket | null = null
let reconnectTimer: ReturnType<typeof setTimeout> | null = null
let reconnectAttempts = 0
let intentionalClose = false
const listeners = new Set<EventListener>()
// Refcounts so a session that's subscribed from two places at once (e.g. a
// SubagentTranscript card and the full ChatView of the same child session)
// doesn't get unsubscribed out from under the other when one side closes.
const subRefCounts = new Map<string, number>()

function clearReconnectTimer() {
  if (reconnectTimer !== null) {
    clearTimeout(reconnectTimer)
    reconnectTimer = null
  }
}

function getBackoffMs(): number {
  const base = Math.min(1000 * Math.pow(2, reconnectAttempts), 30_000)
  const jitter = base * 0.25 * (Math.random() * 2 - 1) // ±25%
  return base + jitter
}

function sendJson(data: unknown) {
  if (socket && socket.readyState === WebSocket.OPEN) {
    socket.send(JSON.stringify(data))
  }
}

export const useWsStore = create<WsState>((set, get) => ({
  eventsBySession: {},
  lastSeqBySession: loadLastSeqs(),
  deniedSessions: {},
  subscribedSessions: new Set<string>(),

  connect: () => {
    intentionalClose = false
    clearReconnectTimer()

    if (
      socket &&
      (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING)
    ) {
      return
    }

    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:'
    const ws = new WebSocket(`${proto}//${location.host}/ws`)
    socket = ws

    ws.addEventListener('open', () => {
      reconnectAttempts = 0
      const token = getToken()
      if (token) {
        sendJson({ type: 'auth', token })
      }
    })

    ws.addEventListener('message', (ev: MessageEvent) => {
      // Frames from a socket we've already replaced are not ours to act on:
      // dispatching them would double every card/project/queue refetch.
      if (socket !== ws) return
      let msg: Record<string, unknown>
      try {
        msg = JSON.parse(String(ev.data))
      } catch {
        return
      }

      if (msg.type === 'auth_ok') {
        useUiStore.getState().setConnected(true)
        // Resume subscriptions for all tracked sessions
        const { subscribedSessions, lastSeqBySession } = get()
        for (const sid of subscribedSessions) {
          sendJson({ type: 'subscribe', session_id: sid })
          const lastSeq = lastSeqBySession[sid]
          if (lastSeq !== undefined) {
            sendJson({ type: 'resume', session_id: sid, last_seq: lastSeq })
          }
        }
        return
      }

      if (msg.type === 'subscribe_denied') {
        // The server refused this session's stream (today: it isn't ours and
        // we're not an admin). Record it so ChatView can say so instead of
        // leaving an optimistic bubble spinning on a stream that will never
        // deliver. Dropping the subscription too stops the reconnect path
        // from re-requesting a stream we know is refused.
        const sessionId = msg.session_id as string
        if (sessionId) {
          const { subscribedSessions, deniedSessions } = get()
          subscribedSessions.delete(sessionId)
          set({
            subscribedSessions: new Set(subscribedSessions),
            deniedSessions: {
              ...deniedSessions,
              [sessionId]: (msg.reason as string) || 'Live updates are unavailable.',
            },
          })
        }
        return
      }

      if (msg.type === 'resync') {
        // The server's broadcast slot for this client overflowed and dropped
        // events. They are unrecoverable from the socket, so re-run the
        // Resume flow from our own last-seen seq (0 = full replay) for the
        // session the server named, or for every subscribed session when it
        // named none. Dedupe-by-seq below makes the overlap with
        // already-applied events harmless.
        const { subscribedSessions, lastSeqBySession } = get()
        const named = msg.session_id as string | undefined
        const targets = named ? [named] : [...subscribedSessions]
        for (const sid of targets) {
          sendJson({ type: 'resume', session_id: sid, last_seq: lastSeqBySession[sid] ?? 0 })
        }
        // Global frames (card updates, announcements) were dropped too and
        // have no replay log — let listeners refetch.
        window.dispatchEvent(new CustomEvent('peckboard:resync', { detail: msg }))
        return
      }

      if (msg.type === 'announcement') {
        // Emit as a custom event that App.tsx can listen to
        window.dispatchEvent(new CustomEvent('peckboard:announcement', { detail: msg }))
        return
      }

      if (msg.type === 'queue') {
        window.dispatchEvent(new CustomEvent('peckboard:queue', { detail: msg }))
        return
      }

      if (msg.type === 'card-update') {
        window.dispatchEvent(new CustomEvent('peckboard:card-update', { detail: msg }))
        return
      }

      if (msg.type === 'project-update') {
        window.dispatchEvent(new CustomEvent('peckboard:project-update', { detail: msg }))
        return
      }

      if (msg.type === 'card-delete') {
        window.dispatchEvent(new CustomEvent('peckboard:card-delete', { detail: msg }))
        return
      }

      if (msg.type === 'worker-question') {
        window.dispatchEvent(new CustomEvent('peckboard:worker-question', { detail: msg }))
        return
      }

      if (msg.type === 'plugin-approval') {
        window.dispatchEvent(new CustomEvent('peckboard:plugin-approval', { detail: msg }))
        return
      }

      if (msg.type === 'repeating-task-changed') {
        window.dispatchEvent(new CustomEvent('peckboard:repeating-task-changed', { detail: msg }))
        return
      }

      if (msg.type === 'repeating-task-run') {
        window.dispatchEvent(new CustomEvent('peckboard:repeating-task-run', { detail: msg }))
        return
      }

      if (msg.type === 'pm-decisions-changed') {
        window.dispatchEvent(new CustomEvent('peckboard:pm-decisions-changed', { detail: msg }))
        return
      }

      // Document Review status / head-version changes. `session_id` carries
      // the REVIEW id (the review screen subscribes to it exactly the way a
      // session subscribes to its own stream), so the payload's `review_id`
      // is what listeners key off.
      if (msg.type === 'doc-review-update') {
        window.dispatchEvent(new CustomEvent('peckboard:doc-review-update', { detail: msg }))
        return
      }

      if (
        msg.type === 'askpass-request' ||
        msg.type === 'askpass-resolved' ||
        msg.type === 'env-unlock-request' ||
        msg.type === 'env-unlock-resolved'
      ) {
        // Password bridges: fan out to the global AskpassDialog /
        // EnvUnlockDialog.
        window.dispatchEvent(new CustomEvent(`peckboard:${msg.type}`, { detail: msg }))
        return
      }
      if (msg.type === 'session-deleted') {
        // Another device deleted this session (or the orchestrator's
        // worker-session cleanup did). Drop every trace of it locally so
        // the tab vanishes from the strip and the body switches off
        // ChatView/SessionTodosView if the deleted session was active.
        // The optimistic local cleanup in `deleteSession` already covers
        // the case where this client did the delete itself — handling
        // the broadcast a second time is idempotent.
        const sessionId = msg.session_id as string
        if (sessionId) {
          const { eventsBySession, lastSeqBySession } = get()
          const remainingSeqs = { ...lastSeqBySession }
          delete remainingSeqs[sessionId]
          saveLastSeqs(remainingSeqs)
          const { [sessionId]: _drop, ...remainingEvents } = eventsBySession
          void _drop
          set({
            eventsBySession: remainingEvents,
            lastSeqBySession: remainingSeqs,
          })
          useSessionsStore.getState().applySessionDeleted(sessionId)
        }
        return
      }

      if (msg.type === 'session-updated') {
        // A server-side change to a session row that clients should reflect
        // without a manual refetch — currently the async model-switch
        // handover flip (outgoing model → incoming model). The full updated
        // session rides in `data`; fan out for ChatView / the sessions store.
        window.dispatchEvent(new CustomEvent('peckboard:session-updated', { detail: msg }))
        return
      }

      if (msg.type === 'session-cleared') {
        // Server wiped this session's events + todos. Two event caches
        // need to drop the snapshot in lockstep — `useWsStore`'s
        // (powers the project-todos aggregator and resume-seq logic)
        // and `useSessionsStore`'s (powers ChatView, the chat-toolbar
        // Tasks badge, and the tab unread state). Also reset the
        // last-seq so a stale subscriber doesn't keep resuming from a
        // now-deleted seq, then fan out to components that hold their
        // own per-session snapshots (the todo loaders in ChatView /
        // SessionTodosView).
        const sessionId = msg.session_id as string
        if (sessionId) {
          const { eventsBySession, lastSeqBySession } = get()
          const remainingSeqs = { ...lastSeqBySession }
          delete remainingSeqs[sessionId]
          saveLastSeqs(remainingSeqs)
          set({
            eventsBySession: { ...eventsBySession, [sessionId]: [] },
            lastSeqBySession: remainingSeqs,
          })
          useSessionsStore.setState((s) => ({
            eventsBySession: { ...s.eventsBySession, [sessionId]: [] },
          }))
          window.dispatchEvent(
            new CustomEvent('peckboard:session-cleared', { detail: { sessionId } }),
          )
        }
        return
      }

      if (msg.type === 'event') {
        // Server sends { type: "event", session_id: "...", event: { id, seq, ts, kind, data } }
        const sessionId = msg.session_id as string
        const eventData = msg.event as Record<string, unknown>
        const event: Event = {
          id: eventData.id as string,
          session_id: sessionId,
          seq: eventData.seq as number,
          ts: eventData.ts as number,
          kind: eventData.kind as string,
          data: (eventData.data ?? {}) as Record<string, unknown>,
        }
        const { eventsBySession, lastSeqBySession } = get()
        const existing = eventsBySession[sessionId] ?? []
        // Dedupe by seq
        if (existing.some((e) => e.seq === event.seq)) return
        const updatedSeqs = {
          ...lastSeqBySession,
          [sessionId]: nextLastSeq(lastSeqBySession[sessionId], event.seq),
        }
        saveLastSeqs(updatedSeqs)
        set({
          eventsBySession: {
            ...eventsBySession,
            [sessionId]: appendEventOrdered(existing, event),
          },
          lastSeqBySession: updatedSeqs,
        })
        // Update processing/unread state in sessions store
        useSessionsStore.getState().handleEvent(event)

        for (const listener of listeners) {
          listener(event)
        }
      }
    })

    ws.addEventListener('close', () => {
      // A superseded socket must not speak for the live one. `disconnect()`
      // followed by `connect()` (StrictMode's mount/cleanup/mount, or a fast
      // logout/login) leaves the OLD socket's close event to fire after a new
      // socket is already installed — without this guard it nulls the pointer
      // to the new socket, flaps the UI to disconnected, and schedules a
      // reconnect that opens a THIRD socket while the second is still open
      // and unreachable, so every frame dispatches twice.
      if (socket !== ws) return
      socket = null
      useUiStore.getState().setConnected(false)

      if (!intentionalClose) {
        const delay = getBackoffMs()
        reconnectAttempts++
        reconnectTimer = setTimeout(() => {
          get().connect()
        }, delay)
      }
    })

    ws.addEventListener('error', () => {
      // Same staleness guard as `close`: nothing to do for a socket we've
      // already replaced. For the current socket the close event fires after
      // this, which handles reconnection.
      if (socket !== ws) return
    })
  },

  disconnect: () => {
    intentionalClose = true
    clearReconnectTimer()
    if (socket) {
      socket.close()
      socket = null
    }
    useUiStore.getState().setConnected(false)
  },

  subscribe: (sessionId: string) => {
    const count = subRefCounts.get(sessionId) ?? 0
    subRefCounts.set(sessionId, count + 1)
    if (count > 0) return
    const { subscribedSessions, lastSeqBySession, deniedSessions } = get()
    subscribedSessions.add(sessionId)
    // Drop any earlier refusal: an explicit re-subscribe is a fresh attempt
    // (the session may now be ours, or our role may have changed), and the
    // stale error state would otherwise outlive the reason for it.
    const { [sessionId]: _wasDenied, ...remainingDenied } = deniedSessions
    void _wasDenied
    set({ subscribedSessions: new Set(subscribedSessions), deniedSessions: remainingDenied })
    sendJson({ type: 'subscribe', session_id: sessionId })
    // Auto-resume from last known seq
    const lastSeq = lastSeqBySession[sessionId]
    if (lastSeq !== undefined) {
      sendJson({ type: 'resume', session_id: sessionId, last_seq: lastSeq })
    }
  },

  unsubscribe: (sessionId: string) => {
    const count = subRefCounts.get(sessionId) ?? 0
    if (count > 1) {
      subRefCounts.set(sessionId, count - 1)
      return
    }
    subRefCounts.delete(sessionId)
    const { subscribedSessions } = get()
    subscribedSessions.delete(sessionId)
    set({ subscribedSessions: new Set(subscribedSessions) })
    sendJson({ type: 'unsubscribe', session_id: sessionId })
  },
  resume: (sessionId: string, lastSeq: number) => {
    sendJson({ type: 'resume', session_id: sessionId, last_seq: lastSeq })
  },

  addEventListener: (listener: EventListener) => {
    listeners.add(listener)
  },

  removeEventListener: (listener: EventListener) => {
    listeners.delete(listener)
  },
}))

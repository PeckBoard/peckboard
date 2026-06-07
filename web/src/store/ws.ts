import { create } from 'zustand'
import type { Event } from '../types/api'
import { useUiStore } from './ui'
import { useSessionsStore } from './sessions'

const TOKEN_KEY = 'peckboard_token'
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
      const token = localStorage.getItem(TOKEN_KEY)
      if (token) {
        sendJson({ type: 'auth', token })
      }
    })

    ws.addEventListener('message', (ev: MessageEvent) => {
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

      if (msg.type === 'announcement') {
        // Emit as a custom event that App.tsx can listen to
        window.dispatchEvent(new CustomEvent('peckboard:announcement', { detail: msg }))
        return
      }

      if (msg.type === 'queue') {
        window.dispatchEvent(new CustomEvent('peckboard:queue', { detail: msg }))
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
        const updatedSeqs = { ...lastSeqBySession, [sessionId]: event.seq }
        saveLastSeqs(updatedSeqs)
        set({
          eventsBySession: {
            ...eventsBySession,
            [sessionId]: [...existing, event],
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
      // The close event will fire after this, which handles reconnection.
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
    const { subscribedSessions, lastSeqBySession } = get()
    subscribedSessions.add(sessionId)
    set({ subscribedSessions: new Set(subscribedSessions) })
    sendJson({ type: 'subscribe', session_id: sessionId })
    // Auto-resume from last known seq
    const lastSeq = lastSeqBySession[sessionId]
    if (lastSeq !== undefined) {
      sendJson({ type: 'resume', session_id: sessionId, last_seq: lastSeq })
    }
  },

  unsubscribe: (sessionId: string) => {
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

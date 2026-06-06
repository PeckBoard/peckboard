import { create } from 'zustand'
import type { Event } from '../types/api'
import { useUiStore } from './ui'

const TOKEN_KEY = 'peckboard_token'

type EventListener = (event: Event) => void

interface WsState {
  /** Map of sessionId -> events received over the socket */
  eventsBySession: Record<string, Event[]>
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

  connect: () => {
    intentionalClose = false
    clearReconnectTimer()

    if (socket && (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING)) {
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
        return
      }

      if (msg.type === 'event') {
        const event = msg as unknown as Event
        const { eventsBySession } = get()
        const existing = eventsBySession[event.session_id] ?? []
        set({
          eventsBySession: {
            ...eventsBySession,
            [event.session_id]: [...existing, event],
          },
        })
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
    sendJson({ type: 'subscribe', session_id: sessionId })
  },

  unsubscribe: (sessionId: string) => {
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

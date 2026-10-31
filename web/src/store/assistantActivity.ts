import { create } from 'zustand'
import { authedFetch } from './auth'

/** One relay the server's gate is holding back for the Assistant
 *  (`GET /api/voice/queue`, the `voice-queue` WS event). */
export interface HeldRelay {
  id: string
  kind: 'question' | 'update' | 'scheduled'
  summary: string
  source_name: string
  source_session_id: string | null
  question_id: string | null
}

interface QueuePayload {
  count: number
  items: HeldRelay[]
}

/** A session the Assistant handed work to that is busy right now
 *  (`GET /api/voice/linked`). */
export interface LinkedSession {
  id: string
  name: string
  folder_id: string
  project_id: string | null
  state: 'working' | 'needs-answer'
}

interface AssistantActivityState {
  queue: HeldRelay[]
  linked: LinkedSession[]
  /** A release / snooze is in flight. */
  busy: boolean
  error: string | null
  /** Start listening for queue pushes and polling linked sessions; returns
   *  the matching stop. */
  start: () => () => void
  loadQueue: () => Promise<void>
  loadLinked: () => Promise<void>
  /** "N updates waiting" tapped: deliver the next topic group now. */
  releaseNext: () => Promise<void>
  /** "Ask me later": the Assistant raises the question again in `minutes`. */
  snooze: (sessionId: string, questionId: string, minutes: number) => Promise<string | null>
  clearError: () => void
}

/** Linked sessions change with other sessions' turns, which the page has no
 *  stream for: poll while it is open. */
const LINKED_POLL_MS = 10_000

async function errorOf(res: Response, fallback: string): Promise<string> {
  const body = (await res.json().catch(() => null)) as { error?: string } | null
  return body?.error ?? `${fallback} (${res.status}).`
}

/**
 * What the Assistant has going on beyond the conversation: relays held back
 * so it stays on one topic, and sessions working on its behalf. Lives
 * beside `useAssistantTopics`, whose voice-session WS subscription carries
 * the `voice-queue` pushes.
 */
export const useAssistantActivity = create<AssistantActivityState>((set, get) => {
  const applyQueue = (p: QueuePayload) => set({ queue: Array.isArray(p.items) ? p.items : [] })

  const onQueueEvent = (ev: Event) => {
    const detail = (ev as CustomEvent<QueuePayload>).detail
    if (!detail) return
    applyQueue(detail)
    // A delivered question or a finished turn often changes who is working.
    void get().loadLinked()
  }

  return {
    queue: [],
    linked: [],
    busy: false,
    error: null,

    start: () => {
      window.addEventListener('peckboard:voice-queue', onQueueEvent)
      void get().loadQueue()
      void get().loadLinked()
      const timer = setInterval(() => void get().loadLinked(), LINKED_POLL_MS)
      return () => {
        window.removeEventListener('peckboard:voice-queue', onQueueEvent)
        clearInterval(timer)
      }
    },

    loadQueue: async () => {
      try {
        const res = await authedFetch('/api/voice/queue')
        if (res.ok) applyQueue((await res.json()) as QueuePayload)
      } catch {
        /* best-effort; the next voice-queue push refreshes it */
      }
    },

    loadLinked: async () => {
      try {
        const res = await authedFetch('/api/voice/linked')
        if (!res.ok) return
        const body = (await res.json()) as { sessions: LinkedSession[] }
        set({ linked: Array.isArray(body.sessions) ? body.sessions : [] })
      } catch {
        /* best-effort; polled */
      }
    },

    releaseNext: async () => {
      if (get().busy) return
      set({ busy: true, error: null })
      try {
        const res = await authedFetch('/api/voice/queue/next', { method: 'POST' })
        if (!res.ok) {
          set({ error: await errorOf(res, "Couldn't hand over the next update") })
          return
        }
        applyQueue((await res.json()) as QueuePayload)
      } catch {
        set({ error: "Couldn't reach the server to hand over the next update." })
      } finally {
        set({ busy: false })
      }
    },

    snooze: async (sessionId, questionId, minutes) => {
      set({ busy: true, error: null })
      try {
        const res = await authedFetch('/api/voice/snooze', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ session_id: sessionId, question_id: questionId, minutes }),
        })
        if (!res.ok) {
          const msg = await errorOf(res, "Couldn't snooze that question")
          set({ error: msg })
          return msg
        }
        set((s) => ({ queue: s.queue.filter((h) => h.question_id !== questionId) }))
        return null
      } catch {
        const msg = "Couldn't reach the server to snooze that question."
        set({ error: msg })
        return msg
      } finally {
        set({ busy: false })
      }
    },

    clearError: () => set({ error: null }),
  }
})

import { create } from 'zustand'
import { authedFetch } from './auth'
import { useVoiceStore } from './voice'
import { useWsStore } from './ws'
import type { AssistantTopic, AssistantTopicsState } from '../voice/assistantContract'

/** `GET /api/voice/topics`, the `POST .../focus` reply, and the
 *  `voice-topics` WS event all carry this — the server's whole ring. */
interface TopicsPayload {
  currentId: string | null
  topics: AssistantTopic[]
}

/** The voice session this store holds a WS subscription on (refcounted in
 *  the WS store, so the voice panel closing never drops it). */
let subscribedTo: string | null = null
let listening = false

async function errorOf(res: Response, fallback: string): Promise<string> {
  const body = (await res.json().catch(() => null)) as { error?: string } | null
  return body?.error ?? `${fallback} (${res.status}).`
}

/**
 * The full-page Assistant's topic ring. The assistant model curates it
 * server-side (`assistant_topics` tool); this store only mirrors the
 * server's state — `currentId` changes through the same path whether the
 * user clicked a topic or the conversation switched by voice.
 */
export const useAssistantTopics = create<AssistantTopicsState>((set) => {
  const apply = (p: TopicsPayload) => set({ topics: p.topics, currentId: p.currentId, error: null })

  const onTopicsEvent = (ev: Event) => {
    const detail = (ev as CustomEvent<TopicsPayload>).detail
    if (detail && Array.isArray(detail.topics)) apply(detail)
  }

  return {
    topics: [],
    currentId: null,
    loading: false,
    error: null,

    load: async () => {
      if (!listening) {
        listening = true
        window.addEventListener('peckboard:voice-topics', onTopicsEvent)
      }
      set({ loading: true })
      try {
        // Get-or-create the voice session so its stream (which carries the
        // `voice-topics` events) can be subscribed before the snapshot.
        const info = await useVoiceStore.getState().ensureSession()
        if (info && subscribedTo !== info.session_id) {
          const ws = useWsStore.getState()
          if (subscribedTo) ws.unsubscribe(subscribedTo)
          ws.subscribe(info.session_id)
          subscribedTo = info.session_id
        }
        const res = await authedFetch('/api/voice/topics')
        if (!res.ok) {
          set({ error: await errorOf(res, "Couldn't load the Assistant's topics") })
          return
        }
        apply((await res.json()) as TopicsPayload)
      } catch {
        set({ error: "Couldn't reach the server to load the Assistant's topics." })
      } finally {
        set({ loading: false })
      }
    },

    focus: async (id) => {
      try {
        const res = await authedFetch('/api/voice/topics/focus', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ id }),
        })
        if (!res.ok) {
          set({ error: await errorOf(res, "Couldn't switch topics") })
          return
        }
        apply((await res.json()) as TopicsPayload)
      } catch {
        set({ error: "Couldn't reach the server to switch topics." })
      }
    },
  }
})

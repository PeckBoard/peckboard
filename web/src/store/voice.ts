import { create } from 'zustand'
import type { Event } from '../types/api'
import { authedFetch } from './auth'
import { useWsStore } from './ws'
import { getSpeechEngine } from '../voice/engine'
import { isRelayText, stripForSpeech, takeSentences } from '../voice/text'

/** Per-browser voice preferences. localStorage, not the DB — they describe
 *  this browser's speech engine (its voices, its microphone language). */
export interface VoicePrefs {
  /** `SpeechSynthesisVoice.voiceURI`; `''` = browser default. */
  voiceURI: string
  rate: number
  pitch: number
  /** Recognition language (BCP-47); `''` = browser default. */
  lang: string
  /** Start listening again once the assistant finishes speaking. */
  autoListen: boolean
}

export const VOICE_PREFS_KEY = 'peckboard_voice_prefs'

const DEFAULT_PREFS: VoicePrefs = {
  voiceURI: '',
  rate: 1,
  pitch: 1,
  lang: '',
  autoListen: true,
}

function loadPrefs(): VoicePrefs {
  try {
    const raw = localStorage.getItem(VOICE_PREFS_KEY)
    if (!raw) return DEFAULT_PREFS
    const parsed = JSON.parse(raw) as Partial<VoicePrefs>
    return {
      voiceURI: typeof parsed.voiceURI === 'string' ? parsed.voiceURI : DEFAULT_PREFS.voiceURI,
      rate: clamp(Number(parsed.rate), 0.5, 2, DEFAULT_PREFS.rate),
      pitch: clamp(Number(parsed.pitch), 0, 2, DEFAULT_PREFS.pitch),
      lang: typeof parsed.lang === 'string' ? parsed.lang : DEFAULT_PREFS.lang,
      autoListen:
        typeof parsed.autoListen === 'boolean' ? parsed.autoListen : DEFAULT_PREFS.autoListen,
    }
  } catch {
    return DEFAULT_PREFS
  }
}

function clamp(n: number, lo: number, hi: number, fallback: number): number {
  if (!Number.isFinite(n)) return fallback
  return Math.min(hi, Math.max(lo, n))
}

function savePrefs(prefs: VoicePrefs) {
  try {
    localStorage.setItem(VOICE_PREFS_KEY, JSON.stringify(prefs))
  } catch {
    /* storage unavailable — in-memory only */
  }
}

export type VoiceStatus = 'idle' | 'listening' | 'thinking' | 'speaking'

export interface PendingUtterance {
  tempId: string
  text: string
  ts: number
}

interface VoiceSessionInfo {
  session_id: string
  model: string
}

interface VoiceState {
  prefs: VoicePrefs
  setPrefs: (patch: Partial<VoicePrefs>) => void
  /** Whether this browser can do speech recognition at all. */
  recognitionSupported: boolean
  panelOpen: boolean
  sessionId: string | null
  model: string | null
  status: VoiceStatus
  /** Partial hypothesis while the user is still talking. */
  interim: string
  /** Raw events of the voice session, sorted by seq. The panel folds them
   *  into transcript lines (see `foldVoiceTranscript`). */
  events: Event[]
  pending: PendingUtterance[]
  error: string | null
  openPanel: () => Promise<void>
  closePanel: () => void
  /** Get-or-create the user's voice session; with `model`, also switch it. */
  ensureSession: (model?: string) => Promise<VoiceSessionInfo | null>
  setModel: (model: string) => Promise<void>
  /** Mic button: start listening, or stop if already listening. Pressing it
   *  while the assistant speaks cancels speech first (barge-in). */
  toggleMic: () => void
  stopSpeaking: () => void
  /** Send an utterance (spoken or typed) to the voice session. */
  sendText: (text: string) => Promise<void>
  /** Speak a sample sentence with the current prefs. */
  testVoice: () => void
  clearError: () => void
}

// ── Loop internals (module-level; not reactive state) ──────────────────
const engine = () => getSpeechEngine()
/** Sentences waiting to be spoken for the current turn. */
let speakQueue: string[] = []
/** Streamed text not yet split into a full sentence. */
let speakBuffer = ''
let speakingNow = false
/** The assistant's turn ended (`agent-end`); resume listening once the
 *  queue drains. */
let turnEnded = false
/** Bumped by cancel so an in-flight utterance's onEnd is ignored. */
let speakGen = 0
let liveListener: ((ev: Event) => void) | null = null
let historyRequestId = 0

function mergeEvents(existing: Event[], incoming: Event[]): Event[] {
  const bySeq = new Map<number, Event>()
  for (const e of existing) bySeq.set(e.seq, e)
  for (const e of incoming) if (!bySeq.has(e.seq)) bySeq.set(e.seq, e)
  return [...bySeq.values()].sort((a, b) => a.seq - b.seq)
}

export const useVoiceStore = create<VoiceState>((set, get) => {
  const enqueueSentences = (sentences: string[]) => {
    for (const s of sentences) {
      const spoken = stripForSpeech(s)
      if (spoken) speakQueue.push(spoken)
    }
  }

  const finishTurn = () => {
    turnEnded = false
    const { prefs, panelOpen, status } = get()
    if (status === 'listening') return
    if (prefs.autoListen && panelOpen) startListening()
    else set({ status: 'idle' })
  }

  const pump = () => {
    if (speakingNow) return
    const next = speakQueue.shift()
    if (next === undefined) {
      if (turnEnded) finishTurn()
      return
    }
    const gen = speakGen
    speakingNow = true
    set({ status: 'speaking' })
    const { prefs } = get()
    engine().speak(next, {
      voiceURI: prefs.voiceURI,
      rate: prefs.rate,
      pitch: prefs.pitch,
      onEnd: () => {
        if (gen !== speakGen) return
        speakingNow = false
        pump()
      },
    })
  }

  const cancelSpeech = () => {
    speakGen++
    speakQueue = []
    speakBuffer = ''
    speakingNow = false
    turnEnded = false
    engine().cancelSpeech()
  }

  const startListening = () => {
    if (!get().recognitionSupported) {
      set({ error: 'Speech recognition is not available in this browser.', status: 'idle' })
      return
    }
    set({ status: 'listening', interim: '', error: null })
    engine().startListening(
      { lang: get().prefs.lang },
      {
        onInterim: (text) => set({ interim: text }),
        onFinal: (text) => {
          set({ interim: '' })
          void get().sendText(text)
        },
        onEnd: () => {
          if (get().status === 'listening') set({ status: 'idle', interim: '' })
        },
        onError: (code) => {
          if (code === 'no-speech' || code === 'aborted') return
          const message =
            code === 'not-allowed' || code === 'service-not-allowed'
              ? 'Microphone access was denied. Allow the microphone for this site and try again.'
              : `Speech recognition error: ${code}.`
          set({ error: message, status: 'idle', interim: '' })
        },
      },
    )
  }

  const stopListening = () => {
    engine().stopListening()
    if (get().status === 'listening') set({ status: 'idle', interim: '' })
  }

  const onLiveEvent = (ev: Event) => {
    const { sessionId, pending } = get()
    if (!sessionId || ev.session_id !== sessionId) return
    set((s) => ({ events: mergeEvents(s.events, [ev]) }))
    switch (ev.kind) {
      case 'user': {
        const text = typeof ev.data.text === 'string' ? ev.data.text : ''
        if (!isRelayText(text)) {
          const idx = pending.findIndex((p) => p.text === text)
          if (idx >= 0) set({ pending: pending.filter((_, i) => i !== idx) })
        }
        break
      }
      case 'agent-start': {
        // A reply is coming — for our utterance or for a relayed update
        // that arrived while we were idle/listening. Either way the
        // assistant gets the floor.
        if (get().status === 'listening') engine().stopListening()
        speakQueue = []
        speakBuffer = ''
        turnEnded = false
        if (!speakingNow) set({ status: 'thinking', interim: '' })
        break
      }
      case 'agent-text': {
        const chunk = typeof ev.data.text === 'string' ? ev.data.text : ''
        speakBuffer += chunk
        const { sentences, rest } = takeSentences(speakBuffer)
        speakBuffer = rest
        enqueueSentences(sentences)
        pump()
        break
      }
      case 'agent-end': {
        if (speakBuffer.trim()) enqueueSentences([speakBuffer])
        speakBuffer = ''
        turnEnded = true
        pump()
        break
      }
      default:
        break
    }
  }

  const loadHistory = async (sessionId: string) => {
    const reqId = ++historyRequestId
    try {
      const res = await authedFetch(`/api/sessions/${sessionId}/events?limit=200`)
      if (!res.ok) return
      const events = (await res.json()) as Event[]
      if (reqId !== historyRequestId || get().sessionId !== sessionId) return
      set((s) => ({ events: mergeEvents(s.events, events) }))
    } catch {
      /* transcript history is best-effort; live events still flow */
    }
  }

  return {
    prefs: loadPrefs(),
    setPrefs: (patch) => {
      const prefs = { ...get().prefs, ...patch }
      savePrefs(prefs)
      set({ prefs })
    },
    recognitionSupported: engine().supportsRecognition(),
    panelOpen: false,
    sessionId: null,
    model: null,
    status: 'idle',
    interim: '',
    events: [],
    pending: [],
    error: null,

    ensureSession: async (model) => {
      try {
        const res = await authedFetch('/api/voice/session', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(model ? { model } : {}),
        })
        if (!res.ok) {
          const err = (await res.json().catch(() => null)) as { error?: string } | null
          set({ error: err?.error ?? `Couldn't open the voice session (${res.status}).` })
          return null
        }
        const info = (await res.json()) as VoiceSessionInfo
        set((s) => ({
          sessionId: info.session_id,
          model: info.model,
          // A different session id means a fresh transcript.
          events: s.sessionId === info.session_id ? s.events : [],
          pending: s.sessionId === info.session_id ? s.pending : [],
        }))
        return info
      } catch {
        set({ error: "Couldn't reach the server to open the voice session." })
        return null
      }
    },

    setModel: async (model) => {
      await get().ensureSession(model)
    },

    openPanel: async () => {
      if (get().panelOpen) return
      set({ panelOpen: true, error: null })
      const info = await get().ensureSession()
      if (!info || !get().panelOpen) return
      const ws = useWsStore.getState()
      if (!liveListener) {
        liveListener = onLiveEvent
        ws.addEventListener(liveListener)
      }
      ws.subscribe(info.session_id)
      void loadHistory(info.session_id)
    },

    closePanel: () => {
      const { sessionId, panelOpen } = get()
      if (!panelOpen) return
      stopListening()
      cancelSpeech()
      const ws = useWsStore.getState()
      if (liveListener) {
        ws.removeEventListener(liveListener)
        liveListener = null
      }
      if (sessionId) ws.unsubscribe(sessionId)
      set({ panelOpen: false, status: 'idle', interim: '' })
    },

    toggleMic: () => {
      const { status } = get()
      if (status === 'listening') {
        stopListening()
        return
      }
      // Barge-in: cut the assistant off and take the floor.
      cancelSpeech()
      startListening()
    },

    stopSpeaking: () => {
      cancelSpeech()
      if (get().status === 'speaking' || get().status === 'thinking') set({ status: 'idle' })
    },

    sendText: async (raw) => {
      const text = raw.trim()
      if (!text) return
      let { sessionId } = get()
      if (!sessionId) {
        const info = await get().ensureSession()
        if (!info) return
        sessionId = info.session_id
      }
      const tempId = `voice-${Date.now()}-${Math.random().toString(36).slice(2)}`
      set((s) => ({
        pending: [...s.pending, { tempId, text, ts: Date.now() }],
        status: 'thinking',
        interim: '',
        error: null,
      }))
      try {
        const res = await authedFetch(`/api/sessions/${sessionId}/message`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ text }),
        })
        if (!res.ok) {
          const err = (await res.json().catch(() => null)) as { error?: string } | null
          throw new Error(err?.error ?? `Send failed (${res.status}).`)
        }
      } catch (e) {
        set((s) => ({
          pending: s.pending.filter((p) => p.tempId !== tempId),
          status: s.status === 'thinking' ? 'idle' : s.status,
          error: e instanceof Error ? e.message : "Couldn't send that. Please try again.",
        }))
      }
    },

    testVoice: () => {
      cancelSpeech()
      const { prefs } = get()
      engine().speak('Hi, this is your Peckboard voice assistant.', {
        voiceURI: prefs.voiceURI,
        rate: prefs.rate,
        pitch: prefs.pitch,
        onEnd: () => {},
      })
    },

    clearError: () => set({ error: null }),
  }
})

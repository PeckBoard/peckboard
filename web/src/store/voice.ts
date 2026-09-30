import { create } from 'zustand'
import type { Event } from '../types/api'
import { authedFetch } from './auth'
import { useWsStore } from './ws'
import { getSpeechEngine } from '../voice/engine'
import {
  isLikelyEcho,
  isRelayText,
  speechWords,
  stripForSpeech,
  takeSpeakable,
} from '../voice/text'

/** Per-browser voice preferences. localStorage, not the DB — they describe
 *  this browser's speech engine (its voices, its microphone language). */
export interface VoicePrefs {
  /** `SpeechSynthesisVoice.voiceURI`; `''` = browser default. */
  voiceURI: string
  rate: number
  pitch: number
  /** Recognition language (BCP-47); `''` = browser default. */
  lang: string
  /** Open the always-on microphone as soon as the voice panel opens. */
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
  /** What the assistant is doing. `listening` = mic open and nothing else
   *  going on; the mic also stays open while thinking / speaking (see
   *  `micOn`), so the user can barge in. */
  status: VoiceStatus
  /** The always-on microphone is enabled (the user hasn't muted it). */
  micOn: boolean
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
  /** Mic button: mute / unmute the always-on microphone. */
  toggleMic: () => void
  /** Silence the rest of the current reply (the turn keeps running). */
  stopSpeaking: () => void
  /** Send an utterance (spoken or typed) to the voice session. */
  sendText: (text: string) => Promise<void>
  /** Speak a sample sentence with the current prefs. */
  testVoice: () => void
  clearError: () => void
  /** Call from a user gesture: lets browsers that gate audio play replies. */
  unlockSpeech: () => void
}

// ── Loop internals (module-level; not reactive state) ──────────────────────
const engine = () => getSpeechEngine()

// Speech output.
/** Chunks waiting to be spoken. */
let speakQueue: string[] = []
/** Streamed text not yet cut into a speakable chunk. */
let speakBuffer = ''
let speakingNow = false
/** Text of the utterance being spoken right now. */
let currentUtterance: string | null = null
/** Bumped by cancel so an in-flight utterance's onEnd is ignored. */
let speakGen = 0
/** Speaks a stalled tail of `speakBuffer` once the stream pauses. */
let flushTimer: ReturnType<typeof setTimeout> | null = null
/** A speech-output failure was already reported this panel session. */
let speakErrorShown = false
/** Recently spoken text, kept briefly so the mic hearing our own voice
 *  (speaker → microphone) isn't taken for the user talking. */
let echoPool: { text: string; until: number }[] = []

// The voice session's turn.
let agentRunning = false
/** The user sent something and no reply has finished yet. */
let awaitingReply = false
/** Barge-in / Stop speaking: say nothing more of the current turn. */
let muteTurn = false
/** Pending interrupt of the running turn; a send waits for it. */
let interruptInFlight: Promise<void> | null = null

// Always-on recognition.
let micWanted = false
let recActive = false
let recStartedAt = 0
let restartFailures = 0
let restartTimer: ReturnType<typeof setTimeout> | null = null

// Live stream plumbing.
let liveListener: ((ev: Event) => void) | null = null
let historyRequestId = 0
/** History has loaded; until then live events are held in `heldEvents`. */
let historyReady = false
let heldEvents: Event[] = []
/** Events at or below this seq were already there when the panel opened
 *  (history, or a WS resume replaying them) — shown, never spoken. */
let speakAfterSeq = Number.MAX_SAFE_INTEGER

/** How long after an utterance ends the mic may still deliver it back. */
const ECHO_TAIL_MS = 1500
/** Words the user must say over the assistant before it yields. */
const BARGE_IN_WORDS = 2

function mergeEvents(existing: Event[], incoming: Event[]): Event[] {
  const bySeq = new Map<number, Event>()
  for (const e of existing) bySeq.set(e.seq, e)
  for (const e of incoming) if (!bySeq.has(e.seq)) bySeq.set(e.seq, e)
  return [...bySeq.values()].sort((a, b) => a.seq - b.seq)
}

function speakErrorMessage(code: string): string {
  if (code === 'not-allowed') {
    return 'The browser blocked spoken replies. Click anywhere in the voice panel to allow sound.'
  }
  if (engine().getVoices().length === 0) {
    return (
      "This browser has no text-to-speech voices, so replies can't be read aloud. " +
      'On Linux, install speech-dispatcher (with espeak-ng) and restart the browser.'
    )
  }
  return `Spoken replies failed (${code}).`
}

export const useVoiceStore = create<VoiceState>((set, get) => {
  const refreshStatus = () => {
    const status: VoiceStatus = speakingNow
      ? 'speaking'
      : awaitingReply || agentRunning
        ? 'thinking'
        : micWanted
          ? 'listening'
          : 'idle'
    if (get().status !== status) set({ status })
  }

  // ── Speech output ──
  const rememberSpoken = (text: string) => {
    const now = Date.now()
    echoPool = echoPool.filter((e) => e.until > now)
    echoPool.push({ text, until: now + ECHO_TAIL_MS })
  }

  const echoTexts = (): string[] => {
    const now = Date.now()
    echoPool = echoPool.filter((e) => e.until > now)
    const texts = echoPool.map((e) => e.text)
    if (currentUtterance) texts.push(currentUtterance)
    return texts
  }

  const reportSpeakError = (code: string) => {
    if (speakErrorShown) return
    speakErrorShown = true
    set({ error: speakErrorMessage(code) })
  }

  const pump = () => {
    if (speakingNow) return
    const next = speakQueue.shift()
    if (next === undefined) {
      refreshStatus()
      return
    }
    const gen = speakGen
    speakingNow = true
    currentUtterance = next
    refreshStatus()
    const { prefs } = get()
    engine().speak(next, {
      voiceURI: prefs.voiceURI,
      rate: prefs.rate,
      pitch: prefs.pitch,
      onError: reportSpeakError,
      onEnd: () => {
        if (gen !== speakGen) return
        speakingNow = false
        currentUtterance = null
        rememberSpoken(next)
        pump()
      },
    })
  }

  const enqueue = (chunks: string[]) => {
    for (const c of chunks) {
      const spoken = stripForSpeech(c)
      if (spoken) speakQueue.push(spoken)
    }
    pump()
  }

  const clearFlushTimer = () => {
    if (flushTimer) clearTimeout(flushTimer)
    flushTimer = null
  }

  /** Speak whatever is buffered, finished sentence or not. */
  const flushBuffer = () => {
    clearFlushTimer()
    const rest = speakBuffer
    speakBuffer = ''
    if (rest.trim()) enqueue([rest])
  }

  /** The stream paused mid-sentence: speak the tail if it stays quiet —
   *  soon when it already ends in punctuation, later when mid-clause. */
  const scheduleFlush = () => {
    clearFlushTimer()
    if (!speakBuffer.trim()) return
    const delay = /[.!?…:;,]["')\]]*\s*$/.test(speakBuffer) ? 300 : 1500
    flushTimer = setTimeout(flushBuffer, delay)
  }

  const cancelSpeech = () => {
    speakGen++
    if (currentUtterance) rememberSpoken(currentUtterance)
    speakQueue = []
    speakBuffer = ''
    speakingNow = false
    currentUtterance = null
    clearFlushTimer()
    engine().cancelSpeech()
  }

  // ── Always-on recognition ──
  const clearRestart = () => {
    if (restartTimer) clearTimeout(restartTimer)
    restartTimer = null
  }

  const assistantAudible = () => speakingNow || speakQueue.length > 0

  /** Barge-in: the user talks over the assistant — stop speaking and
   *  interrupt its running turn so the new utterance is answered next. */
  const bargeIn = () => {
    cancelSpeech()
    const { sessionId } = get()
    if (agentRunning && sessionId && !interruptInFlight) {
      muteTurn = true
      interruptInFlight = authedFetch(`/api/sessions/${sessionId}/interrupt`, { method: 'POST' })
        .then(
          () => undefined,
          () => undefined,
        )
        .finally(() => {
          interruptInFlight = null
        })
    }
    refreshStatus()
  }

  const onHeard = (text: string, isFinal: boolean) => {
    const words = speechWords(text)
    const audible = assistantAudible()
    // Our own voice coming back through the mic: not the user.
    if (words.length === 0 || isLikelyEcho(text, echoTexts())) {
      if (isFinal || get().interim) set({ interim: '' })
      return
    }
    if (!isFinal) {
      set({ interim: text })
      if (audible && words.length >= BARGE_IN_WORDS) bargeIn()
      return
    }
    set({ interim: '' })
    // A lone word over the assistant's voice is more likely noise or a
    // misheard echo than the user taking the floor.
    if (audible && words.length < BARGE_IN_WORDS) return
    if (audible) bargeIn()
    void get().sendText(text)
  }

  const startRecognition = () => {
    clearRestart()
    if (!micWanted || !get().panelOpen || recActive) return
    recActive = true
    recStartedAt = Date.now()
    engine().startListening(
      { lang: get().prefs.lang, continuous: true },
      {
        onInterim: (text) => onHeard(text, false),
        onFinal: (text) => onHeard(text, true),
        onEnd: () => {
          recActive = false
          if (get().interim) set({ interim: '' })
          if (!micWanted || !get().panelOpen) return
          // Engines end sessions on their own (silence, a finished
          // utterance, network hiccups): reopen right away, backing off
          // only when sessions die immediately after starting.
          restartFailures = Date.now() - recStartedAt < 1000 ? restartFailures + 1 : 0
          const delay = Math.min(5000, 100 * 2 ** restartFailures)
          restartTimer = setTimeout(startRecognition, delay)
        },
        onError: (code) => {
          if (code === 'no-speech' || code === 'aborted') return
          if (
            code === 'not-allowed' ||
            code === 'service-not-allowed' ||
            code === 'audio-capture' ||
            code === 'unsupported'
          ) {
            micWanted = false
            set({
              micOn: false,
              interim: '',
              error:
                code === 'audio-capture'
                  ? 'No microphone was found. Connect one and turn the mic back on.'
                  : code === 'unsupported'
                    ? 'Speech recognition is not available in this browser.'
                    : 'Microphone access was denied. Allow the microphone for this site and try again.',
            })
            refreshStatus()
            return
          }
          console.warn('[voice] speech recognition error:', code)
        },
      },
    )
  }

  const stopRecognition = () => {
    clearRestart()
    recActive = false
    engine().stopListening()
    if (get().interim) set({ interim: '' })
  }

  const setMic = (on: boolean) => {
    micWanted = on
    set({ micOn: on })
    if (on) {
      restartFailures = 0
      startRecognition()
    } else {
      stopRecognition()
    }
    refreshStatus()
  }

  // ── Live events ──
  /** Drive the speech loop from one of the voice session's events. */
  const speakEvent = (ev: Event) => {
    switch (ev.kind) {
      case 'agent-start': {
        agentRunning = true
        muteTurn = false
        speakBuffer = ''
        refreshStatus()
        break
      }
      case 'agent-text': {
        // A built-in subagent's narration isn't the assistant talking.
        if (muteTurn || ev.data.parent_tool_use_id) break
        const chunk = typeof ev.data.text === 'string' ? ev.data.text : ''
        speakBuffer += chunk
        const { chunks, rest } = takeSpeakable(speakBuffer)
        speakBuffer = rest
        enqueue(chunks)
        scheduleFlush()
        break
      }
      case 'agent-tool-start':
      case 'question': {
        // Text before a tool call is complete — say it now rather than
        // after the tool returns.
        if (!muteTurn) flushBuffer()
        break
      }
      case 'agent-end': {
        if (!muteTurn) flushBuffer()
        else speakBuffer = ''
        agentRunning = false
        awaitingReply = false
        muteTurn = false
        pump()
        refreshStatus()
        break
      }
      default:
        break
    }
  }

  const onLiveEvent = (ev: Event) => {
    const { sessionId, pending } = get()
    if (!sessionId || ev.session_id !== sessionId) return
    set((s) => ({ events: mergeEvents(s.events, [ev]) }))
    if (ev.kind === 'user') {
      const text = typeof ev.data.text === 'string' ? ev.data.text : ''
      if (!isRelayText(text)) {
        const idx = pending.findIndex((p) => p.text === text)
        if (idx >= 0) set({ pending: pending.filter((_, i) => i !== idx) })
      }
    }
    if (!historyReady) {
      heldEvents.push(ev)
      return
    }
    if (ev.seq > speakAfterSeq) speakEvent(ev)
  }

  /** Load the transcript, then start speaking only what arrives after it. */
  const loadHistory = async (sessionId: string) => {
    const reqId = ++historyRequestId
    let maxSeq = -1
    try {
      const res = await authedFetch(`/api/sessions/${sessionId}/events?limit=200`)
      if (res.ok) {
        const events = (await res.json()) as Event[]
        if (reqId !== historyRequestId || get().sessionId !== sessionId) return
        for (const e of events) maxSeq = Math.max(maxSeq, e.seq)
        set((s) => ({ events: mergeEvents(s.events, events) }))
      }
    } catch {
      /* transcript history is best-effort; live events still flow */
    }
    if (reqId !== historyRequestId || !get().panelOpen) return
    speakAfterSeq = maxSeq
    historyReady = true
    const held = heldEvents
    heldEvents = []
    for (const ev of held) if (ev.seq > speakAfterSeq) speakEvent(ev)
  }

  const resetLoop = () => {
    cancelSpeech()
    agentRunning = false
    awaitingReply = false
    muteTurn = false
    historyReady = false
    heldEvents = []
    speakAfterSeq = Number.MAX_SAFE_INTEGER
    speakErrorShown = false
    echoPool = []
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
    micOn: false,
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
      // Still inside the click that opened the panel: unlock speech output
      // and open the mic now — both are gated on a user gesture.
      resetLoop()
      engine().unlockSynthesis()
      set({ panelOpen: true, error: null })
      const { prefs, recognitionSupported } = get()
      if (prefs.autoListen && recognitionSupported) setMic(true)
      else refreshStatus()
      const info = await get().ensureSession()
      if (!info || !get().panelOpen) return
      const ws = useWsStore.getState()
      if (!liveListener) {
        liveListener = onLiveEvent
        ws.addEventListener(liveListener)
      }
      ws.subscribe(info.session_id)
      await loadHistory(info.session_id)
    },

    closePanel: () => {
      const { sessionId, panelOpen } = get()
      if (!panelOpen) return
      micWanted = false
      stopRecognition()
      resetLoop()
      historyRequestId++
      const ws = useWsStore.getState()
      if (liveListener) {
        ws.removeEventListener(liveListener)
        liveListener = null
      }
      if (sessionId) ws.unsubscribe(sessionId)
      set({ panelOpen: false, micOn: false, status: 'idle', interim: '' })
    },

    toggleMic: () => {
      engine().unlockSynthesis()
      if (micWanted) {
        setMic(false)
        return
      }
      if (!get().recognitionSupported) {
        set({ error: 'Speech recognition is not available in this browser.' })
        return
      }
      set({ error: null })
      setMic(true)
    },

    stopSpeaking: () => {
      cancelSpeech()
      if (agentRunning) muteTurn = true
      refreshStatus()
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
      awaitingReply = true
      set((s) => ({
        pending: [...s.pending, { tempId, text, ts: Date.now() }],
        interim: '',
        error: null,
      }))
      refreshStatus()
      try {
        // After a barge-in, let the interrupt land first so this message
        // starts a fresh turn instead of queueing behind the old one.
        if (interruptInFlight) await interruptInFlight
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
        awaitingReply = false
        set((s) => ({
          pending: s.pending.filter((p) => p.tempId !== tempId),
          error: e instanceof Error ? e.message : "Couldn't send that. Please try again.",
        }))
        refreshStatus()
      }
    },

    testVoice: () => {
      engine().unlockSynthesis()
      cancelSpeech()
      speakErrorShown = false
      const { prefs } = get()
      engine().speak('Hi, this is your Peckboard voice assistant.', {
        voiceURI: prefs.voiceURI,
        rate: prefs.rate,
        pitch: prefs.pitch,
        onError: reportSpeakError,
        onEnd: () => {},
      })
    },

    clearError: () => set({ error: null }),

    unlockSpeech: () => engine().unlockSynthesis(),
  }
})

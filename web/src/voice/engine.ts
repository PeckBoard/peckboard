/**
 * Speech engine abstraction for the voice assistant.
 *
 * The voice loop (store/voice.ts) only ever talks to a `SpeechEngine`, so
 * the browser Web Speech API implementation below can be swapped for a
 * cloud STT/TTS engine later without touching the panel or the loop.
 */

export interface RecognitionCallbacks {
  /** Partial hypothesis while the user is still talking. */
  onInterim: (text: string) => void
  /** Final transcript for the utterance. */
  onFinal: (text: string) => void
  /** Recognition stopped (after a final result, silence, or `stop()`). */
  onEnd: () => void
  /** Recognition failed. `code` is the engine's error string (e.g.
   *  `not-allowed`, `no-speech`, `network`). */
  onError: (code: string) => void
}

export interface RecognitionOptions {
  /** BCP-47 tag, or `''` for the browser default. */
  lang: string
  /** Keep recognizing across pauses instead of stopping after one
   *  utterance. Engines still end sessions on their own (silence, network),
   *  so an always-on caller restarts on `onEnd`. */
  continuous?: boolean
}

export interface SpeakOptions {
  /** Engine-specific voice id (`SpeechSynthesisVoice.voiceURI`), `''` = default. */
  voiceURI: string
  rate: number
  pitch: number
  /** Called when the utterance finishes or is cancelled. */
  onEnd: () => void
  /** The engine failed to speak (`not-allowed`, `synthesis-failed`, …). */
  onError?: (code: string) => void
}

export interface VoiceOption {
  voiceURI: string
  name: string
  lang: string
  default: boolean
}

export interface SpeechEngine {
  supportsRecognition(): boolean
  supportsSynthesis(): boolean
  startListening(opts: RecognitionOptions, cb: RecognitionCallbacks): void
  stopListening(): void
  speak(text: string, opts: SpeakOptions): void
  /** Cancel the current utterance and anything the engine has queued. */
  cancelSpeech(): void
  /** Call from a user gesture (click / tap): unlocks speech output on
   *  engines that only allow it after user activation. */
  unlockSynthesis(): void
  getVoices(): VoiceOption[]
  /** Subscribe to voice-list changes (async population). Returns unsubscribe. */
  onVoicesChanged(cb: () => void): () => void
}

// Minimal typings for the (still prefixed) SpeechRecognition API — TS's
// lib.dom does not ship them.
interface SpeechRecognitionResultLike {
  isFinal: boolean
  0: { transcript: string }
  length: number
}
interface SpeechRecognitionEventLike {
  resultIndex: number
  results: ArrayLike<SpeechRecognitionResultLike>
}
interface SpeechRecognitionLike {
  lang: string
  continuous: boolean
  interimResults: boolean
  maxAlternatives: number
  onresult: ((ev: SpeechRecognitionEventLike) => void) | null
  onend: (() => void) | null
  onerror: ((ev: { error: string }) => void) | null
  start(): void
  stop(): void
  abort(): void
}
type SpeechRecognitionCtor = new () => SpeechRecognitionLike

function recognitionCtor(): SpeechRecognitionCtor | null {
  if (typeof window === 'undefined') return null
  const w = window as unknown as {
    SpeechRecognition?: SpeechRecognitionCtor
    webkitSpeechRecognition?: SpeechRecognitionCtor
  }
  return w.SpeechRecognition ?? w.webkitSpeechRecognition ?? null
}

function synthesis(): SpeechSynthesis | null {
  if (typeof window === 'undefined') return null
  return window.speechSynthesis ?? null
}

/** Pause between `cancel()` and the next `speak()` (see `speak`). */
const CANCEL_SETTLE_MS = 120

/** Browser Web Speech API engine: `SpeechRecognition` for STT and
 *  `speechSynthesis` for TTS. */
export class WebSpeechEngine implements SpeechEngine {
  private recognition: SpeechRecognitionLike | null = null
  /** Utterances in flight — referenced so Chrome can't GC them mid-speech. */
  private live = new Set<SpeechSynthesisUtterance>()
  private lastCancelAt = 0
  private cancelGen = 0
  private unlocked = false

  supportsRecognition(): boolean {
    return recognitionCtor() !== null
  }

  supportsSynthesis(): boolean {
    return synthesis() !== null && typeof SpeechSynthesisUtterance !== 'undefined'
  }

  startListening(opts: RecognitionOptions, cb: RecognitionCallbacks): void {
    const Ctor = recognitionCtor()
    if (!Ctor) {
      cb.onError('unsupported')
      cb.onEnd()
      return
    }
    this.stopListening()
    const rec = new Ctor()
    this.recognition = rec
    if (opts.lang) rec.lang = opts.lang
    rec.continuous = opts.continuous ?? false
    rec.interimResults = true
    rec.maxAlternatives = 1
    let finalText = ''
    rec.onresult = (ev) => {
      let interim = ''
      for (let i = ev.resultIndex; i < ev.results.length; i++) {
        const r = ev.results[i]
        const t = r[0]?.transcript ?? ''
        if (r.isFinal) finalText += t
        else interim += t
      }
      if (finalText.trim()) {
        const text = finalText.trim()
        finalText = ''
        cb.onFinal(text)
      } else if (interim) {
        cb.onInterim(interim)
      }
    }
    rec.onerror = (ev) => cb.onError(ev.error)
    rec.onend = () => {
      if (this.recognition === rec) this.recognition = null
      cb.onEnd()
    }
    try {
      rec.start()
    } catch (e) {
      this.recognition = null
      cb.onError(e instanceof Error ? e.message : 'start-failed')
      cb.onEnd()
    }
  }

  stopListening(): void {
    const rec = this.recognition
    if (!rec) return
    this.recognition = null
    // Detach first: a stop() triggers onend, which must not report a
    // user-initiated stop as "listening ended on its own".
    rec.onresult = null
    rec.onerror = null
    rec.onend = null
    try {
      rec.abort()
    } catch {
      /* already stopped */
    }
  }

  speak(text: string, opts: SpeakOptions): void {
    const synth = synthesis()
    if (!synth || typeof SpeechSynthesisUtterance === 'undefined') {
      opts.onError?.('unsupported')
      opts.onEnd()
      return
    }
    // Chrome drops an utterance queued in the same tick as `cancel()`
    // (cancel completes asynchronously and takes the new one with it).
    const sinceCancel = Date.now() - this.lastCancelAt
    if (sinceCancel < CANCEL_SETTLE_MS) {
      const gen = this.cancelGen
      setTimeout(() => {
        if (gen === this.cancelGen) this.speak(text, opts)
        else opts.onEnd()
      }, CANCEL_SETTLE_MS - sinceCancel)
      return
    }
    const u = new SpeechSynthesisUtterance(text)
    const voices = synth.getVoices()
    const voice = opts.voiceURI ? voices.find((v) => v.voiceURI === opts.voiceURI) : undefined
    if (voice) {
      u.voice = voice
      u.lang = voice.lang
    }
    u.rate = opts.rate
    u.pitch = opts.pitch
    let done = false
    let watchdog: ReturnType<typeof setTimeout> | null = null
    const finish = () => {
      if (done) return
      done = true
      if (watchdog) clearTimeout(watchdog)
      this.live.delete(u)
      opts.onEnd()
    }
    u.onend = finish
    u.onerror = (ev: Event) => {
      const code = (ev as { error?: string }).error ?? 'error'
      // `interrupted` / `canceled` are our own cancel(), not failures.
      if (code !== 'interrupted' && code !== 'canceled') {
        console.warn('[voice] speechSynthesis error:', code)
        // Blocked for lack of a user gesture: let the next gesture retry.
        if (code === 'not-allowed') this.unlocked = false
        opts.onError?.(code)
      }
      finish()
    }
    // Chrome garbage-collects an utterance nothing references and then
    // never fires its `end` — which stalled the speak queue forever. Hold
    // a reference until it finishes, and back that up with a watchdog in
    // case the engine goes silent without an event.
    this.live.add(u)
    const expectedMs = (text.length / 12 / Math.max(0.5, opts.rate)) * 1000
    watchdog = setTimeout(() => {
      console.warn('[voice] speechSynthesis never finished an utterance; skipping it')
      finish()
    }, expectedMs + 8000)
    // A paused engine (Chrome leaves it paused after some tab switches)
    // queues utterances silently until resumed.
    if (synth.paused) synth.resume()
    synth.speak(u)
  }

  cancelSpeech(): void {
    const synth = synthesis()
    if (!synth) return
    this.cancelGen++
    this.lastCancelAt = Date.now()
    try {
      synth.cancel()
    } catch {
      /* nothing queued */
    }
  }

  unlockSynthesis(): void {
    const synth = synthesis()
    if (!synth || this.unlocked || typeof SpeechSynthesisUtterance === 'undefined') return
    this.unlocked = true
    // Speaking (silently) inside a user gesture unlocks speech for the rest
    // of the page's life on engines that gate it behind activation
    // (Safari / iOS, Chrome on Android). Also nudges Chrome to load voices.
    try {
      const u = new SpeechSynthesisUtterance(' ')
      u.volume = 0
      synth.getVoices()
      if (synth.paused) synth.resume()
      synth.speak(u)
    } catch {
      /* best effort */
    }
  }

  getVoices(): VoiceOption[] {
    const synth = synthesis()
    if (!synth) return []
    return synth.getVoices().map((v) => ({
      voiceURI: v.voiceURI,
      name: v.name,
      lang: v.lang,
      default: v.default,
    }))
  }

  onVoicesChanged(cb: () => void): () => void {
    const synth = synthesis()
    if (!synth || typeof synth.addEventListener !== 'function') return () => {}
    synth.addEventListener('voiceschanged', cb)
    return () => synth.removeEventListener('voiceschanged', cb)
  }
}

let defaultEngine: SpeechEngine | null = null

/** The engine the app uses. Swap with `setSpeechEngine` (tests, cloud). */
export function getSpeechEngine(): SpeechEngine {
  if (!defaultEngine) defaultEngine = new WebSpeechEngine()
  return defaultEngine
}

export function setSpeechEngine(engine: SpeechEngine): void {
  defaultEngine = engine
}

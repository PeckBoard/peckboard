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
}

export interface SpeakOptions {
  /** Engine-specific voice id (`SpeechSynthesisVoice.voiceURI`), `''` = default. */
  voiceURI: string
  rate: number
  pitch: number
  /** Called when the utterance finishes or is cancelled. */
  onEnd: () => void
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

/** Browser Web Speech API engine: `SpeechRecognition` for STT and
 *  `speechSynthesis` for TTS. */
export class WebSpeechEngine implements SpeechEngine {
  private recognition: SpeechRecognitionLike | null = null

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
    rec.continuous = false
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
      opts.onEnd()
      return
    }
    const u = new SpeechSynthesisUtterance(text)
    if (opts.voiceURI) {
      const voice = synth.getVoices().find((v) => v.voiceURI === opts.voiceURI)
      if (voice) u.voice = voice
    }
    u.rate = opts.rate
    u.pitch = opts.pitch
    let done = false
    const finish = () => {
      if (done) return
      done = true
      opts.onEnd()
    }
    u.onend = finish
    u.onerror = finish
    synth.speak(u)
  }

  cancelSpeech(): void {
    const synth = synthesis()
    if (!synth) return
    try {
      synth.cancel()
    } catch {
      /* nothing queued */
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

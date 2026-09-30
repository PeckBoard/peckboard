/**
 * Speech engine abstraction for the voice assistant.
 *
 * The voice loop (store/voice.ts) only ever talks to a `SpeechEngine`, so
 * the browser Web Speech API implementation below can be swapped for a
 * cloud STT/TTS engine later without touching the panel or the loop.
 */
import { stripPronunciationHints } from './text'

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
  /** Optional hint: `text` will be spoken next, so engines that fetch
   *  audio (Kokoro) can start early. */
  prefetch?(text: string, opts: SpeakOptions): void
  /** Cancel the current utterance and anything the engine has queued.
   *  `reason` is logged. */
  cancelSpeech(reason?: string): void
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

/** Console trace for the speech loop — every speak / cancel / recognition
 *  transition, so a silent assistant can be diagnosed from DevTools. */
export function voiceLog(...args: unknown[]): void {
  console.info('[voice]', ...args)
}

/** Chromium exposes `navigator.userAgentData`; WebKit (incl. Chrome on iOS)
 *  and Firefox don't. */
function isChromium(): boolean {
  return typeof navigator !== 'undefined' && 'userAgentData' in navigator
}

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
    rec.onerror = (ev) => {
      voiceLog('recognition error:', ev.error)
      cb.onError(ev.error)
    }
    rec.onend = () => {
      voiceLog('recognition ended')
      if (this.recognition === rec) this.recognition = null
      cb.onEnd()
    }
    try {
      voiceLog('recognition start', { continuous: rec.continuous, lang: rec.lang || '(default)' })
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
      voiceLog('speak: speechSynthesis is not available in this browser')
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
        else {
          voiceLog('speak: dropped (cancelled before it started):', text)
          opts.onEnd()
        }
      }, CANCEL_SETTLE_MS - sinceCancel)
      return
    }
    // Nothing of ours is in flight but the engine says it is busy: a stale
    // utterance is wedged in Chrome's native queue, and everything queued
    // behind it would stay silent forever. Flush it first.
    if (this.live.size === 0 && (synth.speaking || synth.pending)) {
      this.cancelSpeech('stale engine queue')
      this.speak(text, opts)
      return
    }
    // Pronunciation hints are for Kokoro; the browser voice reads the words.
    const u = new SpeechSynthesisUtterance(stripPronunciationHints(text))
    const voices = synth.getVoices()
    const voice = opts.voiceURI ? voices.find((v) => v.voiceURI === opts.voiceURI) : undefined
    if (voice) {
      u.voice = voice
      u.lang = voice.lang
    }
    u.rate = opts.rate
    u.pitch = opts.pitch
    u.volume = 1
    const effective = voice ?? voices.find((v) => v.default)
    voiceLog('speak:', JSON.stringify(text), {
      voice: effective
        ? `${effective.name} (${effective.localService ? 'local' : 'network'})`
        : 'none',
      voicesAvailable: voices.length,
      requestedVoice: opts.voiceURI || '(default)',
      engine: { speaking: synth.speaking, pending: synth.pending, paused: synth.paused },
    })
    if (voices.length === 0) {
      voiceLog('speak: the browser reports NO voices yet — the utterance may fail or stay silent')
    }
    let done = false
    let started = false
    let watchdog: ReturnType<typeof setTimeout> | null = null
    let startWatchdog: ReturnType<typeof setTimeout> | null = null
    let keepAlive: ReturnType<typeof setInterval> | null = null
    const finish = () => {
      if (done) return
      done = true
      if (watchdog) clearTimeout(watchdog)
      if (startWatchdog) clearTimeout(startWatchdog)
      if (keepAlive) clearInterval(keepAlive)
      this.live.delete(u)
      opts.onEnd()
    }
    u.onstart = () => {
      started = true
      if (startWatchdog) clearTimeout(startWatchdog)
      voiceLog('speak: started', JSON.stringify(text))
      // Chrome's network voices stop mid-utterance after ~15s unless the
      // engine is nudged; local voices don't need (or like) it.
      if (effective && !effective.localService) {
        keepAlive = setInterval(() => {
          if (!synth.speaking) return
          synth.pause()
          synth.resume()
        }, 10_000)
      }
    }
    u.onend = () => {
      voiceLog('speak: ended', JSON.stringify(text))
      finish()
    }
    u.onerror = (ev: Event) => {
      const code = (ev as { error?: string }).error ?? 'error'
      // `interrupted` / `canceled` are our own cancel(), not failures.
      if (code !== 'interrupted' && code !== 'canceled') {
        console.warn('[voice] speechSynthesis error:', code, JSON.stringify(text))
        // Blocked for lack of a user gesture: let the next gesture retry.
        if (code === 'not-allowed') this.unlocked = false
        opts.onError?.(code)
      } else {
        voiceLog(`speak: ${code}`, JSON.stringify(text))
      }
      finish()
    }
    // Chrome garbage-collects an utterance nothing references and then
    // never fires its `end` — which stalled the speak queue forever. Hold
    // a reference until it finishes, and back that up with watchdogs in
    // case the engine goes silent without an event.
    this.live.add(u)
    startWatchdog = setTimeout(() => {
      // Engines that never fire `start` but are audibly busy are fine.
      if (started || done || synth.speaking) return
      console.warn(
        '[voice] speechSynthesis never started an utterance within 5s; resetting the engine',
        { speaking: synth.speaking, pending: synth.pending, paused: synth.paused },
      )
      this.cancelSpeech('utterance never started')
      finish()
    }, 5000)
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

  cancelSpeech(reason = 'unspecified'): void {
    const synth = synthesis()
    if (!synth) return
    this.cancelGen++
    const busy = synth.speaking || synth.pending || this.live.size > 0
    voiceLog(`cancel (${reason})`, busy ? '' : '— nothing was playing', {
      speaking: synth.speaking,
      pending: synth.pending,
      inFlight: this.live.size,
    })
    // Cancelling an idle engine is a no-op, but it would still make the
    // next speak() wait out the cancel-settle delay.
    if (!busy) return
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
    synth.getVoices()
    if (synth.paused) synth.resume()
    // Chromium (desktop and Android) only needs sticky user activation,
    // which the click calling this already grants — a throwaway utterance
    // there just risks wedging its queue. WebKit (Safari, and every iOS
    // browser including Chrome) needs a speak() inside the gesture.
    if (isChromium()) {
      voiceLog('unlock: Chromium — user activation is enough, no priming utterance')
      return
    }
    try {
      const u = new SpeechSynthesisUtterance(' ')
      u.volume = 0
      voiceLog('unlock: speaking a silent priming utterance')
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

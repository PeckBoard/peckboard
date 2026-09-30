/**
 * Kokoro speech engine: natural-sounding TTS synthesized server-side
 * (`POST /api/voice/tts` → WAV) and played through Web Audio.
 *
 * Voices are addressed as `kokoro:<id>` voice URIs, so one engine serves
 * both families: a `kokoro:` voice goes through the server, anything else
 * (and all recognition) is delegated to the browser `WebSpeechEngine`.
 * `onEnd` / `onError` fire exactly as the browser engine fires them, so
 * the store's speak queue, barge-in and echo filtering are unchanged.
 *
 * While the model is still downloading (503), or on any failure, the
 * utterance falls back to the browser voice; after repeated real failures
 * Kokoro is switched off for the rest of the page session.
 */

import { create } from 'zustand'
import { authedFetch } from '../store/auth'
import {
  WebSpeechEngine,
  setSpeechEngine,
  voiceLog,
  type RecognitionCallbacks,
  type RecognitionOptions,
  type SpeakOptions,
  type SpeechEngine,
  type VoiceOption,
} from './engine'

export const KOKORO_PREFIX = 'kokoro:'
export const DEFAULT_KOKORO_VOICE = 'kokoro:af_heart'

export function isKokoroVoice(voiceURI: string): boolean {
  return voiceURI.startsWith(KOKORO_PREFIX)
}

/** Real (non-503) failures before Kokoro is disabled for the session. */
/** Server-side cap on one request (`service::tts::MAX_TEXT_CHARS`). */
const MAX_TEXT_CHARS = 500
const MAX_FAILURES = 3

export interface KokoroStatus {
  state: 'unknown' | 'not_started' | 'downloading' | 'ready' | 'unavailable'
  progress: number
  error: string | null
}

/** Server-side model status, for the voice panel's first-use notice. */
export const useKokoroStatus = create<KokoroStatus>(() => ({
  state: 'unknown',
  progress: 0,
  error: null,
}))

let pollTimer: ReturnType<typeof setTimeout> | null = null

function applyStatus(raw: unknown) {
  const s = raw as { state?: KokoroStatus['state']; progress?: number; error?: string | null }
  if (!s || typeof s.state !== 'string') return
  useKokoroStatus.setState({ state: s.state, progress: s.progress ?? 0, error: s.error ?? null })
}

async function pollStatus() {
  pollTimer = null
  try {
    const res = await authedFetch('/api/voice/tts/status')
    if (!res.ok) return
    applyStatus(await res.json())
  } catch {
    return
  }
  if (useKokoroStatus.getState().state === 'downloading') pollTimer = setTimeout(pollStatus, 1000)
}

/** Ask the server to download/load the model (idempotent) and track it. */
export async function prepareKokoro(): Promise<void> {
  const { state } = useKokoroStatus.getState()
  if (state === 'ready' || (state === 'downloading' && pollTimer)) return
  try {
    const res = await authedFetch('/api/voice/tts/prepare', { method: 'POST' })
    if (!res.ok) {
      useKokoroStatus.setState({ state: 'unavailable', error: `HTTP ${res.status}` })
      return
    }
    applyStatus(await res.json())
    voiceLog('kokoro: prepare →', useKokoroStatus.getState().state)
  } catch (e) {
    voiceLog('kokoro: prepare failed', e)
    return
  }
  if (useKokoroStatus.getState().state === 'downloading' && !pollTimer) {
    pollTimer = setTimeout(pollStatus, 1000)
  }
}

class TtsHttpError extends Error {
  readonly status: number
  constructor(status: number) {
    super(`tts HTTP ${status}`)
    this.status = status
  }
}

interface Pending {
  key: string
  ctrl: AbortController
  audio: Promise<ArrayBuffer>
}

function langLabel(lang: string): string {
  return lang === 'en-GB' ? 'UK' : 'US'
}

export class KokoroEngine implements SpeechEngine {
  private readonly web: SpeechEngine
  private ctx: AudioContext | null = null
  private source: AudioBufferSourceNode | null = null
  /** Finishes the utterance currently playing (once). */
  private finishCurrent: (() => void) | null = null
  /** In-flight fetches: the one being spoken plus at most one prefetch. */
  private pending: Pending[] = []
  private gen = 0
  private failures = 0
  private disabled = false
  private voices: VoiceOption[] = []
  private voicesLoading = false
  private voicesLoadedAt = 0
  private listeners = new Set<() => void>()

  constructor(web: SpeechEngine = new WebSpeechEngine()) {
    this.web = web
  }

  supportsRecognition(): boolean {
    return this.web.supportsRecognition()
  }

  supportsSynthesis(): boolean {
    return this.web.supportsSynthesis() || typeof AudioContext !== 'undefined'
  }

  startListening(opts: RecognitionOptions, cb: RecognitionCallbacks): void {
    this.web.startListening(opts, cb)
  }

  stopListening(): void {
    this.web.stopListening()
  }

  /** Kokoro for a `kokoro:` voice, unless it failed repeatedly this session
   *  or the server reported it unavailable (a later `prepareKokoro` — e.g.
   *  reopening the panel — retries). */
  private useKokoro(voiceURI: string): boolean {
    return (
      isKokoroVoice(voiceURI) &&
      !this.disabled &&
      useKokoroStatus.getState().state !== 'unavailable' &&
      typeof AudioContext !== 'undefined'
    )
  }

  /** Browser-engine options for a fallback: a Kokoro URI means "default". */
  private webOpts(opts: SpeakOptions): SpeakOptions {
    return isKokoroVoice(opts.voiceURI) ? { ...opts, voiceURI: '' } : opts
  }

  private key(text: string, opts: SpeakOptions): string {
    return `${opts.voiceURI}|${opts.rate}|${text}`
  }

  private request(text: string, opts: SpeakOptions): Pending {
    const key = this.key(text, opts)
    const found = this.pending.find((p) => p.key === key)
    if (found) return found
    const ctrl = new AbortController()
    const audio = authedFetch('/api/voice/tts', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        text,
        voice: opts.voiceURI.slice(KOKORO_PREFIX.length),
        speed: opts.rate,
      }),
      signal: ctrl.signal,
    }).then((res) => {
      if (!res.ok) throw new TtsHttpError(res.status)
      return res.arrayBuffer()
    })
    // A prefetch nobody consumes must not surface as an unhandled rejection.
    audio.catch(() => {})
    const p = { key, ctrl, audio }
    this.pending.push(p)
    // Current + one prefetch; drop (abort) anything older.
    while (this.pending.length > 2) this.pending.shift()?.ctrl.abort()
    return p
  }

  /** Start fetching the next sentence while the current one plays. */
  prefetch(text: string, opts: SpeakOptions): void {
    if (!text || text.length > MAX_TEXT_CHARS || !this.useKokoro(opts.voiceURI)) return
    this.request(text, opts)
  }

  speak(text: string, opts: SpeakOptions): void {
    // The server caps a request; an over-long chunk is rare, so it just
    // uses the browser voice.
    if (text.length > MAX_TEXT_CHARS || !this.useKokoro(opts.voiceURI)) {
      this.web.speak(text, this.webOpts(opts))
      return
    }
    const gen = this.gen
    const req = this.request(text, opts)
    const ctx = this.ensureCtx()
    const started = performance.now()
    voiceLog('kokoro speak:', JSON.stringify(text), { voice: opts.voiceURI })
    let done = false
    const finish = () => {
      if (done) return
      done = true
      if (this.finishCurrent === finish) this.finishCurrent = null
      opts.onEnd()
    }
    this.finishCurrent = finish
    req.audio
      .then((buf) => {
        if (gen !== this.gen) throw new DOMException('cancelled', 'AbortError')
        return ctx.decodeAudioData(buf.slice(0))
      })
      .then(async (audio) => {
        this.pending = this.pending.filter((p) => p !== req)
        if (gen !== this.gen) return finish()
        if (ctx.state === 'suspended') {
          await Promise.race([ctx.resume(), new Promise((r) => setTimeout(r, 1000))])
          if (ctx.state === 'suspended') {
            console.warn('[voice] kokoro: AudioContext is suspended (no user gesture yet)')
            opts.onError?.('not-allowed')
            return finish()
          }
        }
        if (gen !== this.gen) return finish()
        this.failures = 0
        const src = ctx.createBufferSource()
        src.buffer = audio
        src.connect(ctx.destination)
        const watchdog = setTimeout(
          () => {
            console.warn('[voice] kokoro: playback never ended; skipping it')
            finish()
          },
          audio.duration * 1000 + 3000,
        )
        src.onended = () => {
          clearTimeout(watchdog)
          if (this.source === src) this.source = null
          voiceLog('kokoro speak: ended', JSON.stringify(text))
          finish()
        }
        this.source = src
        src.start()
        voiceLog('kokoro speak: started', JSON.stringify(text), {
          latencyMs: Math.round(performance.now() - started),
          seconds: Number(audio.duration.toFixed(2)),
        })
      })
      .catch((err: unknown) => {
        this.pending = this.pending.filter((p) => p !== req)
        if (done) return
        if (gen !== this.gen || (err instanceof DOMException && err.name === 'AbortError')) {
          voiceLog('kokoro speak: cancelled', JSON.stringify(text))
          return finish()
        }
        if (err instanceof TtsHttpError && err.status === 503) {
          voiceLog('kokoro: natural voice not ready yet — using the browser voice')
          void prepareKokoro()
        } else {
          this.failures++
          console.warn('[voice] kokoro failed; falling back to the browser voice:', err)
          if (this.failures >= MAX_FAILURES) {
            this.disabled = true
            console.warn('[voice] kokoro: disabled for this session after repeated failures')
          }
        }
        // Hand the utterance to the browser voice; its onEnd ends ours.
        this.finishCurrent = null
        this.web.speak(text, { ...this.webOpts(opts), onEnd: finish })
      })
  }

  cancelSpeech(reason = 'unspecified'): void {
    this.gen++
    const busy = this.source !== null || this.pending.length > 0
    if (busy) voiceLog(`kokoro cancel (${reason})`)
    for (const p of this.pending) p.ctrl.abort()
    this.pending = []
    const src = this.source
    this.source = null
    if (src) {
      try {
        src.stop()
      } catch {
        /* not started */
      }
    }
    // An utterance waiting on its fetch/decode ends now, like the browser
    // engine's `interrupted`.
    this.finishCurrent?.()
    this.web.cancelSpeech(reason)
  }

  private ensureCtx(): AudioContext {
    if (!this.ctx || this.ctx.state === 'closed') this.ctx = new AudioContext()
    return this.ctx
  }

  unlockSynthesis(): void {
    this.web.unlockSynthesis()
    if (typeof AudioContext === 'undefined') return
    const ctx = this.ensureCtx()
    if (ctx.state === 'suspended') {
      voiceLog('unlock: resuming the AudioContext')
      void ctx.resume()
    }
  }

  private loadVoices() {
    // Retry at most once a minute (e.g. first call before login).
    if (this.voicesLoading || Date.now() - this.voicesLoadedAt < 60_000) return
    this.voicesLoading = true
    this.voicesLoadedAt = Date.now()
    authedFetch('/api/voice/tts/voices')
      .then(async (res) => {
        if (!res.ok) return
        const body = (await res.json()) as {
          default: string
          voices: { id: string; name: string; lang: string }[]
        }
        this.voices = body.voices.map((v) => ({
          voiceURI: KOKORO_PREFIX + v.id,
          name: `Kokoro — ${v.name} (${langLabel(v.lang)})`,
          lang: v.lang,
          default: v.id === body.default,
        }))
        for (const cb of this.listeners) cb()
      })
      .catch(() => {})
      .finally(() => {
        this.voicesLoading = false
      })
  }

  getVoices(): VoiceOption[] {
    if (this.voices.length === 0) this.loadVoices()
    return [...this.voices, ...this.web.getVoices()]
  }

  onVoicesChanged(cb: () => void): () => void {
    this.listeners.add(cb)
    const off = this.web.onVoicesChanged(cb)
    return () => {
      this.listeners.delete(cb)
      off()
    }
  }
}

let installed = false

/** Make the Kokoro engine the app's speech engine (idempotent). */
export function installKokoroEngine(): void {
  if (installed) return
  installed = true
  setSpeechEngine(new KokoroEngine())
}

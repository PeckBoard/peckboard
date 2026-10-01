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

type PlayResult = 'started' | 'cancelled' | 'not-allowed'

/** iPhone/iPad — every iOS browser is WebKit. iPadOS reports a Mac UA, so
 *  touch support tells them apart. */
export function isIOS(): boolean {
  if (typeof navigator === 'undefined') return false
  const ua = navigator.userAgent
  return /iPhone|iPad|iPod/.test(ua) || (/Macintosh/.test(ua) && navigator.maxTouchPoints > 1)
}

/** iOS plays Kokoro through a media element instead of Web Audio (see
 *  `playWithElement`). */
function playsViaMediaElement(): boolean {
  return isIOS() && typeof Audio !== 'undefined'
}

function audioContextCtor(): typeof AudioContext | null {
  if (typeof window === 'undefined') return null
  return (
    window.AudioContext ??
    (window as unknown as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext ??
    null
  )
}

function canPlayKokoro(): boolean {
  return playsViaMediaElement() || audioContextCtor() !== null
}

/** Promise-returning decode; older WebKit only has the callback form. */
function decode(ctx: AudioContext, buf: ArrayBuffer): Promise<AudioBuffer> {
  return new Promise((resolve, reject) => {
    const p = ctx.decodeAudioData(buf, resolve, reject) as Promise<AudioBuffer> | undefined
    p?.then(resolve, reject)
  })
}

/** Duration of a PCM WAV from its header (byte rate at offset 28). */
export function wavSeconds(buf: ArrayBuffer): number {
  if (buf.byteLength < 44) return 0
  const byteRate = new DataView(buf).getUint32(28, true)
  return byteRate > 0 ? (buf.byteLength - 44) / byteRate : 0
}

/** A mono 16-bit PCM WAV of `pcm` at `rate` Hz. */
export function wavBlob(pcm: Int16Array, rate: number): Blob {
  const b = new DataView(new ArrayBuffer(44 + pcm.length * 2))
  const str = (o: number, s: string) => {
    for (let i = 0; i < s.length; i++) b.setUint8(o + i, s.charCodeAt(i))
  }
  str(0, 'RIFF')
  b.setUint32(4, 36 + pcm.length * 2, true)
  str(8, 'WAVEfmt ')
  b.setUint32(16, 16, true)
  b.setUint16(20, 1, true)
  b.setUint16(22, 1, true)
  b.setUint32(24, rate, true)
  b.setUint32(28, rate * 2, true)
  b.setUint16(32, 2, true)
  b.setUint16(34, 16, true)
  str(36, 'data')
  b.setUint32(40, pcm.length * 2, true)
  for (let i = 0; i < pcm.length; i++) b.setInt16(44 + i * 2, pcm[i], true)
  return new Blob([b.buffer], { type: 'audio/wav' })
}

let silentUrl: string | null = null

/** A 0.05 s silent 24 kHz WAV, for unlocking a media element in a gesture. */
function silentWavUrl(): string {
  silentUrl ??= URL.createObjectURL(wavBlob(new Int16Array(1200), 24000))
  return silentUrl
}

const KOKORO_SPEED_KEY = 'peckboard.voice.kokoroSpeed'

/** Kokoro speech speed (the model's `speed`): the Settings → Voice slider
 *  range and default. A touch under 1× keeps short replies ("Got it.")
 *  distinct over a Bluetooth headset. */
export const KOKORO_SPEED_MIN = 0.8
export const KOKORO_SPEED_MAX = 1.2
export const DEFAULT_KOKORO_SPEED = 0.95

function clampSpeed(n: number): number {
  if (!Number.isFinite(n)) return DEFAULT_KOKORO_SPEED
  return Math.min(KOKORO_SPEED_MAX, Math.max(KOKORO_SPEED_MIN, n))
}

/** Per-device Kokoro choices (localStorage). */
export const useKokoroDevicePrefs = create<{ speed: number }>(() => {
  let speed = DEFAULT_KOKORO_SPEED
  try {
    const raw = localStorage.getItem(KOKORO_SPEED_KEY)
    if (raw !== null) speed = clampSpeed(Number(raw))
  } catch {
    /* storage unavailable */
  }
  return { speed }
})

export function setKokoroSpeed(speed: number): void {
  const s = clampSpeed(speed)
  try {
    localStorage.setItem(KOKORO_SPEED_KEY, String(s))
  } catch {
    /* storage unavailable */
  }
  useKokoroDevicePrefs.setState({ speed: s })
}

/**
 * Bluetooth headsets (AirPods) idle on digital silence and take a few
 * hundred ms to wake on the next sound, swallowing the first syllable
 * ("Got it" → "it"). Kokoro clips open with ~350 ms of near-zero samples
 * (≤ -80 dBFS), too quiet to wake the link. So every clip carries an
 * inaudible noise floor (~-67 dBFS) from its first sample, and the first
 * clip of a burst gets extra pre-roll in front.
 */
export const FIRST_CLIP_PREROLL_MS = 200
export const NEXT_CLIP_PREROLL_MS = 0
/** Idle time after which the next clip counts as the first of a burst. */
const BURST_GAP_MS = 2000
/** Peak of the wake noise, in 16-bit LSBs. */
const WAKE_NOISE_LSB = 24

/**
 * `wav` (16-bit mono PCM) with `prerollMs` of silence prepended and the
 * wake noise floor added throughout (faded in over 10 ms). Anything else
 * comes back as an unchanged copy (decoding detaches its input).
 */
export function withWakePreroll(wav: ArrayBuffer, prerollMs: number): ArrayBuffer {
  const v = new DataView(wav)
  const pcm16Mono =
    wav.byteLength >= 44 &&
    v.getUint32(0) === 0x52494646 && // RIFF
    v.getUint32(36) === 0x64617461 && // data
    v.getUint16(20, true) === 1 &&
    v.getUint16(22, true) === 1 &&
    v.getUint16(34, true) === 16
  if (!pcm16Mono) return wav.slice(0)
  const rate = v.getUint32(24, true)
  const n = Math.min(v.getUint32(40, true), wav.byteLength - 44) >> 1
  const pad = Math.round((rate * Math.max(0, prerollMs)) / 1000)
  const total = pad + n
  const out = new ArrayBuffer(44 + total * 2)
  new Uint8Array(out).set(new Uint8Array(wav, 0, 44))
  const o = new DataView(out)
  o.setUint32(4, 36 + total * 2, true)
  o.setUint32(40, total * 2, true)
  const fade = Math.max(1, Math.round(rate * 0.01))
  let seed = 0x2545f491
  for (let i = 0; i < total; i++) {
    seed ^= seed << 13
    seed ^= seed >>> 17
    seed ^= seed << 5
    const noise = ((seed >>> 0) / 0xffffffff - 0.5) * 2 * WAKE_NOISE_LSB * Math.min(1, i / fade)
    const s = i < pad ? 0 : v.getInt16(44 + (i - pad) * 2, true)
    o.setInt16(44 + i * 2, Math.max(-32768, Math.min(32767, Math.round(s + noise))), true)
  }
  return out
}

let previewEl: HTMLAudioElement | null = null
let previewCtx: AudioContext | null = null

/**
 * For a one-off preview (Settings → Pronunciations → Play). Call it
 * synchronously inside the click so the gesture unlocks playback; it
 * returns the function that plays the fetched WAV, via the same path as
 * the assistant (media element on iOS, Web Audio elsewhere).
 */
export function preparePreviewPlayback(): (wav: ArrayBuffer) => Promise<void> {
  if (playsViaMediaElement()) {
    if (!previewEl) previewEl = new Audio()
    const el = previewEl
    el.src = silentWavUrl()
    void el.play().catch(() => {})
    return async (wav) => {
      const url = URL.createObjectURL(
        new Blob([withWakePreroll(wav, FIRST_CLIP_PREROLL_MS)], { type: 'audio/wav' }),
      )
      el.onended = el.onerror = () => URL.revokeObjectURL(url)
      el.src = url
      await el.play()
    }
  }
  const Ctor = audioContextCtor()
  if (!Ctor) return () => Promise.reject(new Error('This browser cannot play audio.'))
  if (!previewCtx || previewCtx.state === 'closed') previewCtx = new Ctor()
  const ctx = previewCtx
  if (ctx.state !== 'running') void ctx.resume()
  return async (wav) => {
    const audio = await decode(ctx, withWakePreroll(wav, FIRST_CLIP_PREROLL_MS))
    const src = ctx.createBufferSource()
    src.buffer = audio
    src.connect(ctx.destination)
    src.start()
  }
}

export class KokoroEngine implements SpeechEngine {
  private readonly web: SpeechEngine
  private ctx: AudioContext | null = null
  /** Set by a `devicechange`: rebuild the context before the next utterance. */
  private ctxStale = false
  private watchingDevices = false
  /** iOS playback element, reused so its gesture unlock sticks. */
  private el: HTMLAudioElement | null = null
  private elUnlocked = false
  /** iOS element for the thinking cue and filler (see `thinkingElement`). */
  private thinkEl: HTMLAudioElement | null = null
  private thinkElUnlocked = false
  /** Stops whatever is playing right now (either path), silently. */
  private stopCurrent: (() => void) | null = null
  /** Stops for every Web Audio clip started and not yet ended: cancel
   *  silences all of them, not only the current one. */
  private sources = new Set<() => void>()
  /** Finishes the utterance currently playing (once). */
  private finishCurrent: (() => void) | null = null
  /** In-flight fetches: the one being spoken plus at most one prefetch. */
  private pending: Pending[] = []
  private gen = 0
  private failures = 0
  /** When the last utterance ended (`performance.now()`), for burst pre-roll. */
  private lastEnd = -Infinity
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
    return this.web.supportsSynthesis() || canPlayKokoro()
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
      canPlayKokoro()
    )
  }

  /** Browser-engine options for a fallback: a Kokoro URI means "default". */
  private webOpts(opts: SpeakOptions): SpeakOptions {
    return isKokoroVoice(opts.voiceURI) ? { ...opts, voiceURI: '' } : opts
  }

  private key(text: string, opts: SpeakOptions): string {
    return `${opts.voiceURI}|${useKokoroDevicePrefs.getState().speed}|${text}`
  }

  /** A private copy of `buf` ready to play: wake noise, plus the burst's
   *  pre-roll when nothing has played for a while (see
   *  `FIRST_CLIP_PREROLL_MS`). */
  private wakeWav(buf: ArrayBuffer): ArrayBuffer {
    const first = performance.now() - this.lastEnd > BURST_GAP_MS
    const prerollMs = first ? FIRST_CLIP_PREROLL_MS : NEXT_CLIP_PREROLL_MS
    voiceLog('kokoro: wake pre-roll', { prerollMs, first })
    return withWakePreroll(buf, prerollMs)
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
        // Kokoro has its own speed (Settings → Voice); `opts.rate` is the
        // browser voice's, used on fallback.
        speed: useKokoroDevicePrefs.getState().speed,
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
    const started = performance.now()
    voiceLog('kokoro speak:', JSON.stringify(text), { voice: opts.voiceURI })
    let done = false
    const finish = () => {
      if (done) return
      done = true
      this.lastEnd = performance.now()
      if (this.finishCurrent === finish) this.finishCurrent = null
      opts.onEnd()
    }
    this.finishCurrent = finish
    const played = playsViaMediaElement()
      ? req.audio.then((buf) => {
          this.pending = this.pending.filter((p) => p !== req)
          if (gen !== this.gen) throw new DOMException('cancelled', 'AbortError')
          return this.playWithElement(this.wakeWav(buf), text, finish)
        })
      : this.playWithWebAudio(req, gen, text, finish)
    played
      .then((ok) => {
        if (ok === 'not-allowed') {
          opts.onError?.('not-allowed')
          return finish()
        }
        if (ok === 'cancelled') return finish()
        this.failures = 0
        opts.onStart?.()
        voiceLog('kokoro speak: started', JSON.stringify(text), {
          latencyMs: Math.round(performance.now() - started),
          path: playsViaMediaElement() ? 'media-element' : 'web-audio',
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

  /** Decode + play through Web Audio (desktop). Resolves once started. */
  private async playWithWebAudio(
    req: Pending,
    gen: number,
    text: string,
    finish: () => void,
  ): Promise<PlayResult> {
    const ctx = this.ensureCtx()
    const buf = await req.audio
    if (gen !== this.gen) throw new DOMException('cancelled', 'AbortError')
    // decodeAudioData detaches its input (and prefetch entries share the
    // promise), so always decode a private copy — `wakeWav` makes one.
    const audio = await decode(ctx, this.wakeWav(buf))
    this.pending = this.pending.filter((p) => p !== req)
    if (gen !== this.gen) return 'cancelled'
    // iOS reports `interrupted` (call, Siri, route change) as well as
    // `suspended`; both need a resume.
    if (ctx.state !== 'running') {
      await Promise.race([ctx.resume(), new Promise((r) => setTimeout(r, 1000))])
      if ((ctx.state as string) !== 'running') {
        console.warn(`[voice] kokoro: AudioContext is ${ctx.state} (no user gesture yet)`)
        return 'not-allowed'
      }
    }
    if (gen !== this.gen) return 'cancelled'
    const src = ctx.createBufferSource()
    src.buffer = audio
    src.connect(ctx.destination)
    let watchdog: ReturnType<typeof setTimeout> | undefined = undefined
    const stop = () => {
      clearTimeout(watchdog)
      this.sources.delete(stop)
      if (this.stopCurrent === stop) this.stopCurrent = null
      src.onended = null
      try {
        src.stop()
      } catch {
        /* not started */
      }
    }
    // A stalled output (a Bluetooth route change freezes the context) never
    // ends the clip: skip it, and silence it too — left scheduled, it would
    // play whenever the output recovers, after later sentences or a barge-in.
    watchdog = setTimeout(
      () => {
        console.warn('[voice] kokoro: playback never ended; skipping it')
        stop()
        finish()
      },
      audio.duration * 1000 + 3000,
    )
    src.onended = () => {
      clearTimeout(watchdog)
      this.sources.delete(stop)
      if (this.stopCurrent === stop) this.stopCurrent = null
      voiceLog('kokoro speak: ended', JSON.stringify(text))
      finish()
    }
    this.stopCurrent = stop
    this.sources.add(stop)
    src.start()
    return 'started'
  }

  /**
   * Play the WAV through one reused `<audio>` element (iOS). WebKit's Web
   * Audio output on iOS shares the audio session with the always-on speech
   * recognizer, and when recognition flips the session to play-and-record
   * the context keeps rendering at its original rate → crackle/static. The
   * media element goes through AVFoundation, which follows session and
   * route changes and resamples correctly.
   */
  private async playWithElement(
    buf: ArrayBuffer,
    text: string,
    finish: () => void,
  ): Promise<PlayResult> {
    const el = this.audioEl()
    const url = URL.createObjectURL(new Blob([buf], { type: 'audio/wav' }))
    let watchdog: ReturnType<typeof setTimeout> | undefined = undefined
    const release = () => {
      clearTimeout(watchdog)
      el.onended = null
      el.onerror = null
      URL.revokeObjectURL(url)
    }
    const stop = () => {
      release()
      el.pause()
      el.removeAttribute('src')
      el.load()
    }
    this.stopCurrent = stop
    el.onended = () => {
      release()
      if (this.stopCurrent === stop) this.stopCurrent = null
      voiceLog('kokoro speak: ended', JSON.stringify(text))
      finish()
    }
    let started = false
    el.onerror = () => {
      console.warn('[voice] kokoro: <audio> playback error', el.error?.code)
      // Before playback, play() rejects too and that path falls back to
      // the browser voice; only a mid-playback error ends the utterance.
      if (!started) return
      stop()
      if (this.stopCurrent === stop) this.stopCurrent = null
      finish()
    }
    el.src = url
    try {
      await el.play()
    } catch (err) {
      const cancelled = this.stopCurrent !== stop
      if (!cancelled) this.stopCurrent = null
      stop()
      if (cancelled) return 'cancelled'
      if (err instanceof DOMException && err.name === 'NotAllowedError') {
        console.warn('[voice] kokoro: <audio> play() blocked (no user gesture yet)')
        return 'not-allowed'
      }
      throw err
    }
    if (this.stopCurrent !== stop) return 'cancelled'
    started = true
    watchdog = setTimeout(
      () => {
        console.warn('[voice] kokoro: playback never ended; skipping it')
        if (this.stopCurrent === stop) this.stopCurrent = null
        stop()
        finish()
      },
      wavSeconds(buf) * 1000 + 3000,
    )
    return 'started'
  }

  cancelSpeech(reason = 'unspecified'): void {
    this.gen++
    const busy = this.stopCurrent !== null || this.pending.length > 0 || this.sources.size > 0
    if (busy) voiceLog(`kokoro cancel (${reason})`)
    for (const p of this.pending) p.ctrl.abort()
    this.pending = []
    const stop = this.stopCurrent
    this.stopCurrent = null
    stop?.()
    for (const s of [...this.sources]) s()
    // An utterance waiting on its fetch/decode ends now, like the browser
    // engine's `interrupted`.
    this.finishCurrent?.()
    this.web.cancelSpeech(reason)
  }

  private ensureCtx(): AudioContext {
    // A route change (headphones/Bluetooth) can leave the old context
    // rendering at the wrong hardware rate — rebuild it between utterances.
    if (this.ctx && this.ctxStale && !this.stopCurrent) {
      void this.ctx.close().catch(() => {})
      this.ctx = null
    }
    if (!this.ctx || this.ctx.state === 'closed') {
      const Ctor = audioContextCtor()
      if (!Ctor) throw new Error('no Web Audio')
      this.ctx = new Ctor()
      this.ctxStale = false
      if (!this.watchingDevices && navigator.mediaDevices?.addEventListener) {
        this.watchingDevices = true
        navigator.mediaDevices.addEventListener('devicechange', () => {
          this.ctxStale = true
        })
      }
    }
    return this.ctx
  }

  private audioEl(): HTMLAudioElement {
    if (!this.el) {
      this.el = new Audio()
      this.el.preload = 'auto'
    }
    return this.el
  }

  audioContext(): AudioContext | null {
    if (playsViaMediaElement() || !this.ctx || this.ctx.state === 'closed') return null
    return this.ctx
  }

  thinkingElement(): HTMLAudioElement | null {
    return playsViaMediaElement() ? this.thinkEl : null
  }

  unlockSynthesis(): void {
    this.web.unlockSynthesis()
    if (playsViaMediaElement()) {
      // iOS only lets a media element play() later once it has played
      // inside a gesture; prime the one element every utterance reuses,
      // and the one the thinking cue and filler share.
      if (!this.thinkEl) this.thinkEl = new Audio()
      const think = this.thinkEl
      if (!this.thinkElUnlocked && think.paused) {
        think.src = silentWavUrl()
        think.play().then(
          () => {
            this.thinkElUnlocked = true
          },
          () => {},
        )
      }
      if (this.elUnlocked || this.stopCurrent) return
      const el = this.audioEl()
      el.src = silentWavUrl()
      el.play().then(
        () => {
          this.elUnlocked = true
          voiceLog('unlock: media element unlocked')
        },
        () => {},
      )
      return
    }
    if (!audioContextCtor()) return
    const ctx = this.ensureCtx()
    if (ctx.state !== 'running') {
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

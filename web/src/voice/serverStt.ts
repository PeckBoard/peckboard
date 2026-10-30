/**
 * Speech recognition through the external speech server (VoiceStudio,
 * Settings → Assistant → Speech Server) instead of the browser's
 * `webkitSpeechRecognition` — which Android WebViews and Firefox lack.
 *
 * The microphone is captured with echo cancellation, downsampled to 16 kHz
 * mono PCM16 in an AudioWorklet (`/pcm16-worklet.js`; ScriptProcessor where
 * AudioWorklet is missing) and streamed in 100 ms frames over
 * `/api/voice/stt`, which Peckboard's server relays to the speech server
 * (see src/ws/voice_stt.rs). Results reach the store through the same
 * `RecognitionCallbacks` the browser engine fires, so barge-in, end of
 * turn and restart-on-`onEnd` work unchanged:
 *
 * - `partial` → `onInterim`;
 * - `final` (`utterance`) → `onFinal`. Streaming models endpoint every
 *   utterance themselves; the whole-session `summary` that follows repeats
 *   them and is dropped;
 * - a model with no utterance finals only sends its `summary` after the
 *   audio ends: once its partial has stopped changing the session asks for
 *   it (`end`), delivers it as the final and ends — the store restarts,
 *   exactly as after a one-shot browser recognition.
 *
 * A denied microphone reports `not-allowed` (as the browser engine does);
 * a socket or server failure reports `network` then `onEnd`, so the
 * store's restart backoff applies. The capture outlives one session by a
 * few seconds so those restarts don't reopen the microphone each time.
 */

import { create } from 'zustand'
import { getToken } from '../store/auth'
import {
  phantomFinalReason,
  voiceLog,
  type HeardMeta,
  type RecognitionCallbacks,
  type RecognitionOptions,
} from './engine'

export type RecognitionSource = 'browser' | 'server'

const RECOGNITION_KEY = 'peckboard.voice.recognition'
const STT_PATH = '/api/voice/stt'
const WORKLET_URL = '/pcm16-worklet.js'
const STT_RATE = 16_000
/** 100 ms of 16 kHz audio per frame. */
const FRAME_SAMPLES = 1600
/** Frames held while the server connects (3 s), so the first words count. */
const MAX_QUEUED_FRAMES = 30
/** Stop sending (drop frames) when the socket backs up this far. */
const MAX_BUFFERED_BYTES = 512 * 1024
/** With no utterance finals: how long a partial must stay unchanged
 *  before the session asks the server for its final transcript. */
export const STABLE_PARTIAL_MS = 900
/** After asking for the final transcript, how long to wait for it. */
const END_TIMEOUT_MS = 10_000
/** Keep the microphone open this long after a session ends on its own. */
const RELEASE_CAPTURE_MS = 4000
/** The server must say `ready` this soon after the socket opens. */
const READY_TIMEOUT_MS = 8000
/** Mic RMS above this counts as speech for the stall watchdog. */
const VOICED_RMS = 0.02
/** Speech-level frames (100 ms each) sent with no server reply… */
const STALL_VOICED_FRAMES = 80
/** …and this long since the server's last message: its recognizer hung. */
const STALL_SILENCE_MS = 15_000
/** Error code of a session the stall watchdog ended (benign: reconnect). */
export const STALLED = 'stalled'

function loadSource(): RecognitionSource {
  try {
    return localStorage.getItem(RECOGNITION_KEY) === 'server' ? 'server' : 'browser'
  } catch {
    return 'browser'
  }
}

/** Per-browser recognition choice, plus whether a speech server is saved
 *  (`null` = not known yet — assume it is until the server says not). */
export const useSpeechRecognitionPrefs = create<{
  source: RecognitionSource
  serverConfigured: boolean | null
}>(() => ({ source: loadSource(), serverConfigured: null }))

export function setRecognitionSource(source: RecognitionSource): void {
  try {
    localStorage.setItem(RECOGNITION_KEY, source)
  } catch {
    /* storage unavailable */
  }
  useSpeechRecognitionPrefs.setState({ source })
}

export function setSpeechServerConfigured(configured: boolean): void {
  if (useSpeechRecognitionPrefs.getState().serverConfigured !== configured) {
    useSpeechRecognitionPrefs.setState({ serverConfigured: configured })
  }
}

type AudioContextCtor = typeof AudioContext

function audioContextCtor(): AudioContextCtor | null {
  if (typeof window === 'undefined') return null
  const w = window as unknown as {
    AudioContext?: AudioContextCtor
    webkitAudioContext?: AudioContextCtor
  }
  return w.AudioContext ?? w.webkitAudioContext ?? null
}

/** This browser can capture and stream microphone audio. */
export function supportsServerRecognition(): boolean {
  return (
    typeof WebSocket !== 'undefined' &&
    typeof navigator !== 'undefined' &&
    typeof navigator.mediaDevices?.getUserMedia === 'function' &&
    audioContextCtor() !== null
  )
}

/** Recognition should go to the speech server right now. */
export function wantsServerRecognition(): boolean {
  const { source, serverConfigured } = useSpeechRecognitionPrefs.getState()
  return source === 'server' && serverConfigured !== false && supportsServerRecognition()
}

/** Main-thread twin of the worklet's downsampler (ScriptProcessor path). */
export class Pcm16Downsampler {
  private readonly ratio: number
  private readonly frameSamples: number
  private readonly onFrame: (frame: ArrayBuffer) => void
  private buf: Int16Array
  private n = 0
  private acc = 0
  private accN = 0
  private pos = 0

  constructor(inRate: number, frameSamples: number, onFrame: (frame: ArrayBuffer) => void) {
    this.ratio = Math.max(1, inRate / STT_RATE)
    this.frameSamples = frameSamples
    this.onFrame = onFrame
    this.buf = new Int16Array(frameSamples)
  }

  push(input: Float32Array): void {
    for (let i = 0; i < input.length; i++) {
      this.acc += input[i]
      this.accN++
      this.pos += 1
      if (this.pos < this.ratio) continue
      this.pos -= this.ratio
      const s = this.acc / this.accN
      this.acc = 0
      this.accN = 0
      this.buf[this.n++] = Math.max(-32768, Math.min(32767, Math.round(s * 32767)))
      if (this.n === this.frameSamples) {
        this.onFrame(this.buf.buffer as ArrayBuffer)
        this.buf = new Int16Array(this.frameSamples)
        this.n = 0
      }
    }
  }
}

/** RMS (0..1) of the newest microphone frame, and when it arrived. */
let micRms = 0
let micAt = -Infinity
/** A frame older than this no longer counts as the live level. */
const MIC_LEVEL_STALE_MS = 400

function noteMicFrame(frame: ArrayBuffer): void {
  const pcm = new Int16Array(frame)
  let sum = 0
  for (let i = 0; i < pcm.length; i++) {
    const v = pcm[i] / 32768
    sum += v * v
  }
  micRms = pcm.length > 0 ? Math.sqrt(sum / pcm.length) : 0
  micAt = performance.now()
}

/** The microphone's raw level (RMS, 0..1) while server recognition is
 *  capturing, from the same PCM frames it streams; 0 otherwise. */
export function serverMicLevel(): number {
  return performance.now() - micAt < MIC_LEVEL_STALE_MS ? micRms : 0
}

interface Capture {
  /** Where frames go; the active session swaps itself in and out. */
  onFrame: ((frame: ArrayBuffer) => void) | null
  /** The microphone went away (unplugged, revoked). */
  onLost: (() => void) | null
  close(): void
}

async function openCapture(): Promise<Capture> {
  const stream = await navigator.mediaDevices.getUserMedia({
    audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true },
  })
  const Ctor = audioContextCtor()
  let ctx: AudioContext | null = null
  try {
    if (!Ctor) throw new Error('no Web Audio')
    ctx = new Ctor()
    if (ctx.state !== 'running') await ctx.resume().catch(() => {})
    const audio = ctx
    const cap: Capture = {
      onFrame: null,
      onLost: null,
      close: () => {
        cap.onFrame = null
        cap.onLost = null
        micAt = -Infinity
        for (const t of stream.getTracks()) t.stop()
        void audio.close().catch(() => {})
      },
    }
    const deliver = (frame: ArrayBuffer) => {
      noteMicFrame(frame)
      cap.onFrame?.(frame)
    }
    const source = audio.createMediaStreamSource(stream)
    let node: AudioNode
    if (audio.audioWorklet && typeof AudioWorkletNode !== 'undefined') {
      await audio.audioWorklet.addModule(WORKLET_URL)
      const w = new AudioWorkletNode(audio, 'pcm16-downsampler', {
        processorOptions: { outRate: STT_RATE, frameSamples: FRAME_SAMPLES },
      })
      w.port.onmessage = (ev: MessageEvent<ArrayBuffer>) => deliver(ev.data)
      node = w
    } else {
      const sp = audio.createScriptProcessor(4096, 1, 1)
      const ds = new Pcm16Downsampler(audio.sampleRate, FRAME_SAMPLES, deliver)
      sp.onaudioprocess = (ev) => ds.push(ev.inputBuffer.getChannelData(0))
      node = sp
    }
    // Processing nodes only run when pulled by the destination; a muted
    // gain keeps the microphone out of the speakers.
    const mute = audio.createGain()
    mute.gain.value = 0
    source.connect(node)
    node.connect(mute)
    mute.connect(audio.destination)
    for (const t of stream.getAudioTracks()) t.addEventListener('ended', () => cap.onLost?.())
    voiceLog('server recognition: microphone open', { sampleRate: audio.sampleRate })
    return cap
  } catch (e) {
    for (const t of stream.getTracks()) t.stop()
    void ctx?.close().catch(() => {})
    throw e
  }
}

let capture: Promise<Capture> | null = null
let releaseTimer: ReturnType<typeof setTimeout> | null = null

function acquireCapture(): Promise<Capture> {
  if (releaseTimer) clearTimeout(releaseTimer)
  releaseTimer = null
  if (!capture) {
    const p = openCapture()
    capture = p
    p.then(
      (cap) => {
        cap.onLost = () => {
          if (capture === p) capture = null
          cap.close()
          current?.finish('audio-capture')
        }
      },
      () => {
        if (capture === p) capture = null
      },
    )
  }
  return capture
}

function releaseCapture(): void {
  if (releaseTimer) clearTimeout(releaseTimer)
  releaseTimer = null
  const p = capture
  capture = null
  p?.then(
    (cap) => cap.close(),
    () => {},
  )
}

/** `getUserMedia` failure → the browser engine's error code. */
function micErrorCode(e: unknown): string {
  const name = e instanceof DOMException || e instanceof Error ? e.name : ''
  if (name === 'NotAllowedError' || name === 'SecurityError') return 'not-allowed'
  return 'audio-capture'
}

interface Session {
  cb: RecognitionCallbacks
  ws: WebSocket
  continuous: boolean
  startedAt: number
  ready: boolean
  queue: ArrayBuffer[]
  /** Callbacks detached: `onEnd` fired or the caller stopped it. */
  done: boolean
  /** `end` sent: the server's last finals and close are expected. */
  ending: boolean
  sawUtterance: boolean
  heardInterim: boolean
  lastPartial: string
  timer: ReturnType<typeof setTimeout> | null
  /** Gives up when the server never says `ready`. */
  readyTimer: ReturnType<typeof setTimeout> | null
  /** `performance.now()` of the server's last message (stall watchdog). */
  lastServerAt: number
  /** Speech-level frames sent since the server's last message. */
  voicedSinceServer: number
  sendFrame: (frame: ArrayBuffer) => void
  finish: (error?: string) => void
}

let current: Session | null = null

function sttUrl(): string {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:'
  return `${proto}//${location.host}${STT_PATH}`
}

/** Stop delivering for `s` and drop its socket; the capture stays. */
function detach(s: Session): void {
  s.done = true
  if (s.timer) clearTimeout(s.timer)
  s.timer = null
  if (s.readyTimer) clearTimeout(s.readyTimer)
  s.readyTimer = null
  if (current === s) current = null
  void capture?.then(
    (cap) => {
      if (cap.onFrame === s.sendFrame) cap.onFrame = null
    },
    () => {},
  )
  try {
    s.ws.close()
  } catch {
    /* already closed */
  }
}

function askForFinal(s: Session): void {
  if (s.done || s.ending) return
  s.ending = true
  if (s.timer) clearTimeout(s.timer)
  try {
    s.ws.send(JSON.stringify({ type: 'end' }))
  } catch {
    s.finish('network')
    return
  }
  s.timer = setTimeout(() => s.finish('network'), END_TIMEOUT_MS)
}

function onFinalText(s: Session, text: string, kind: string): void {
  if (kind === 'utterance') {
    s.sawUtterance = true
    if (s.timer && !s.ending) clearTimeout(s.timer)
  } else if (s.sawUtterance) {
    if (text) voiceLog('server recognition: dropped the session summary (utterances already heard)')
    return
  }
  if (!text) return
  const meta: HeardMeta = { confidence: 0, hadInterim: s.heardInterim }
  s.heardInterim = false
  s.lastPartial = ''
  const drop = phantomFinalReason(text, meta, Date.now() - s.startedAt)
  if (drop) {
    voiceLog(`dropped utterance (${drop}):`, JSON.stringify(text), meta)
    return
  }
  voiceLog('recognized final (speech server):', JSON.stringify(text), { kind })
  s.cb.onFinal(text, meta)
  if (!s.continuous && !s.done) askForFinal(s)
}

function onServerFrame(s: Session, raw: string): void {
  let msg: { type?: string; text?: string; final_kind?: string; error?: string; code?: string }
  try {
    msg = JSON.parse(raw)
  } catch {
    return
  }
  const text = (msg.text ?? '').trim()
  switch (msg.type) {
    case 'ready':
      s.ready = true
      for (const f of s.queue.splice(0)) s.sendFrame(f)
      return
    case 'partial':
      if (!text || text === s.lastPartial || s.ending) return
      s.lastPartial = text
      s.heardInterim = true
      s.cb.onInterim(text, { confidence: 0, hadInterim: true })
      if (!s.sawUtterance) {
        if (s.timer) clearTimeout(s.timer)
        s.timer = setTimeout(() => askForFinal(s), STABLE_PARTIAL_MS)
      }
      return
    case 'final':
      onFinalText(s, text, msg.final_kind === 'utterance' ? 'utterance' : 'summary')
      return
    case 'error':
      if (msg.code === 'not-configured') setSpeechServerConfigured(false)
      console.warn('[voice] speech server recognition:', msg.error ?? 'error')
      // A setup problem (no speech-to-text model, no server, a rejected
      // key) fails every reconnect the same way: report it as such so the
      // caller stops instead of flapping the mic on and off.
      if (msg.code === 'stt-unavailable' || msg.code === 'not-configured') {
        sttProblem = msg.error ?? 'the speech server cannot recognize speech'
        s.finish(STT_UNAVAILABLE)
        return
      }
      s.finish('network')
      return
  }
}

/** Error code a recognition session ends with on a setup problem. */
export const STT_UNAVAILABLE = 'stt-unavailable'
let sttProblem = ''

/** The speech server's own words for the last setup problem. */
export function serverSttProblem(): string {
  return sttProblem
}

export function startServerListening(opts: RecognitionOptions, cb: RecognitionCallbacks): void {
  if (current) detach(current)
  let ws: WebSocket
  try {
    ws = new WebSocket(sttUrl())
  } catch {
    cb.onError('network')
    cb.onEnd()
    return
  }
  ws.binaryType = 'arraybuffer'
  const s: Session = {
    cb,
    ws,
    continuous: opts.continuous ?? false,
    startedAt: Date.now(),
    ready: false,
    queue: [],
    done: false,
    ending: false,
    sawUtterance: false,
    heardInterim: false,
    lastPartial: '',
    timer: null,
    readyTimer: null,
    lastServerAt: performance.now(),
    voicedSinceServer: 0,
    sendFrame: (frame) => {
      if (s.done || s.ending) return
      if (!s.ready) {
        s.queue.push(frame)
        if (s.queue.length > MAX_QUEUED_FRAMES) s.queue.shift()
        return
      }
      if (ws.readyState === WebSocket.OPEN && ws.bufferedAmount < MAX_BUFFERED_BYTES) ws.send(frame)
      // Stall watchdog: the server answers speech with partials. Seconds of
      // speech sent with no word back means its recognizer hung (the socket
      // can stay open) — end the session so the caller opens a fresh one
      // instead of sitting on a dead mic that still reads "Listening".
      if (micRms >= VOICED_RMS) s.voicedSinceServer++
      if (
        s.voicedSinceServer >= STALL_VOICED_FRAMES &&
        performance.now() - s.lastServerAt > STALL_SILENCE_MS
      ) {
        voiceLog('server recognition stalled: speech sent, no reply — restarting', {
          voicedFrames: s.voicedSinceServer,
          silentMs: Math.round(performance.now() - s.lastServerAt),
        })
        // Benign: a noisy room can look like this too, so it must never
        // count toward the caller's give-up limit — it just reconnects.
        s.finish(STALLED)
      }
    },
    finish: (error) => {
      if (s.done) return
      detach(s)
      if (releaseTimer) clearTimeout(releaseTimer)
      releaseTimer = capture ? setTimeout(releaseCapture, RELEASE_CAPTURE_MS) : null
      if (error) {
        voiceLog('recognition error (speech server):', error)
        cb.onError(error)
      }
      voiceLog('recognition ended (speech server)')
      cb.onEnd()
    },
  }
  current = s
  // A server that never says `ready` would buffer the mic forever while the
  // UI shows "Listening": give up and let the caller restart.
  s.readyTimer = setTimeout(() => {
    if (s.done || s.ready) return
    voiceLog(`server recognition: no "ready" within ${READY_TIMEOUT_MS} ms — restarting`)
    s.finish('network')
  }, READY_TIMEOUT_MS)
  ws.onopen = () => {
    if (!s.done) ws.send(JSON.stringify({ type: 'auth', token: getToken() ?? '' }))
  }
  ws.onmessage = (ev: MessageEvent) => {
    if (s.done || typeof ev.data !== 'string') return
    // Any word from the server means it is alive: reset the stall count.
    s.lastServerAt = performance.now()
    s.voicedSinceServer = 0
    onServerFrame(s, ev.data)
  }
  ws.onclose = () => s.finish(s.ending ? undefined : 'network')
  voiceLog('recognition start (speech server)', { continuous: s.continuous })
  acquireCapture().then(
    (cap) => {
      if (!s.done) cap.onFrame = s.sendFrame
    },
    (e: unknown) => {
      console.warn('[voice] speech server recognition: microphone unavailable:', e)
      s.finish(micErrorCode(e))
    },
  )
}

/** Stop listening (caller-initiated: no `onEnd`) and close the microphone. */
export function stopServerListening(): void {
  const s = current
  if (s) detach(s)
  releaseCapture()
}

import { authedFetch } from '../store/auth'

/**
 * A step-by-step record of what this browser or web view does when asked for
 * the microphone. Permission failures in embedded web views can be silent, and
 * can't be reproduced from anywhere but that machine, so this reports what
 * actually happened instead of leaving it to be guessed at.
 */

const MIC_TIMEOUT_MS = 20_000
const RECOGNITION_WATCH_MS = 8_000
const LEVEL_WATCH_MS = 1_000

type RecognitionEvents = {
  lang: string
  continuous: boolean
  interimResults: boolean
  start(): void
  abort(): void
  onstart: ((ev: unknown) => void) | null
  onaudiostart: ((ev: unknown) => void) | null
  onsoundstart: ((ev: unknown) => void) | null
  onspeechstart: ((ev: unknown) => void) | null
  onresult: ((ev: unknown) => void) | null
  onnomatch: ((ev: unknown) => void) | null
  onend: ((ev: unknown) => void) | null
  onerror: ((ev: { error?: string; message?: string }) => void) | null
}

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms))

function timeout<T>(work: Promise<T>, ms: number): Promise<T> {
  return Promise.race([
    work,
    new Promise<T>((_, reject) => {
      setTimeout(() => reject(new Error(`no answer after ${ms / 1000} s`)), ms)
    }),
  ])
}

const describe = (e: unknown) =>
  e instanceof Error ? `${e.name}: ${e.message}` : `unknown error: ${String(e)}`

/** The loudest level on `stream` for a moment; silence can mean a blocked mic. */
async function peakLevel(stream: MediaStream): Promise<number> {
  const ctx = new AudioContext()
  try {
    await ctx.resume()
    const analyser = ctx.createAnalyser()
    ctx.createMediaStreamSource(stream).connect(analyser)
    const samples = new Float32Array(analyser.fftSize)
    let peak = 0
    const until = Date.now() + LEVEL_WATCH_MS
    while (Date.now() < until) {
      analyser.getFloatTimeDomainData(samples)
      for (const s of samples) peak = Math.max(peak, Math.abs(s))
      await sleep(50)
    }
    return peak
  } finally {
    void ctx.close()
  }
}

async function testCapture(add: (line: string) => void) {
  const devices = navigator.mediaDevices
  if (!devices?.getUserMedia) {
    add('getUserMedia: not available in this web view')
    return
  }
  add('getUserMedia: asking…')
  try {
    const stream = await timeout(devices.getUserMedia({ audio: true }), MIC_TIMEOUT_MS)
    const track = stream.getAudioTracks()[0]
    add(`getUserMedia: OK, "${track?.label ?? '?'}", ${track?.readyState}, muted=${track?.muted}`)
    try {
      add(`mic level: peak ${(await peakLevel(stream)).toFixed(4)} over 1 s`)
    } catch (e) {
      add(`mic level: ${describe(e)}`)
    }
    for (const t of stream.getTracks()) t.stop()
  } catch (e) {
    add(`getUserMedia: FAILED, ${describe(e)}`)
  }
  try {
    const inputs = (await devices.enumerateDevices()).filter((d) => d.kind === 'audioinput')
    add(`inputs: ${inputs.length} [${inputs.map((d) => d.label || '(no name)').join(', ')}]`)
  } catch (e) {
    add(`inputs: ${describe(e)}`)
  }
}

async function testRecognition(add: (line: string) => void, lang: string) {
  const w = window as unknown as Record<string, unknown>
  const name = w.SpeechRecognition ? 'SpeechRecognition' : 'webkitSpeechRecognition'
  const Ctor = w[name] as (new () => RecognitionEvents) | undefined
  if (!Ctor) {
    add('speech recognition: not available in this web view')
    return
  }
  const started = Date.now()
  let events = 0
  const note = (event: string) => {
    events++
    add(`speech recognition: ${event} at ${Date.now() - started} ms`)
  }
  const rec = new Ctor()
  if (lang) rec.lang = lang
  rec.continuous = true
  rec.interimResults = true
  rec.onstart = () => note('start')
  rec.onaudiostart = () => note('audiostart')
  rec.onsoundstart = () => note('soundstart')
  rec.onspeechstart = () => note('speechstart')
  rec.onresult = () => note('result')
  rec.onnomatch = () => note('nomatch')
  rec.onend = () => note('end')
  rec.onerror = (ev) => note(`ERROR ${ev.error ?? '?'} ${ev.message ?? ''}`.trim())
  try {
    rec.start()
    add(`speech recognition: ${name}.start() called, lang=${rec.lang || '(default)'}`)
    await sleep(RECOGNITION_WATCH_MS)
    if (events === 0) add(`speech recognition: NO EVENTS in ${RECOGNITION_WATCH_MS / 1000} s`)
  } catch (e) {
    add(`speech recognition: start() threw, ${describe(e)}`)
  } finally {
    rec.onend = null
    rec.onerror = null
    try {
      rec.abort()
    } catch {
      /* already stopped */
    }
  }
}

/** Run every check, calling `onLine` as each result comes in. */
export async function runMicDiagnostics(lang: string, onLine: (line: string) => void) {
  const lines: string[] = []
  const add = (line: string) => {
    lines.push(line)
    onLine(line)
  }
  add(`page: ${location.protocol}//${location.host}, secure=${window.isSecureContext}`)
  add(`agent: ${navigator.userAgent}`)
  try {
    const state = await navigator.permissions.query({ name: 'microphone' as PermissionName })
    add(`permission before: ${state.state}`)
  } catch (e) {
    add(`permission before: unknown (${describe(e)})`)
  }
  await testCapture(add)
  await testRecognition(add, lang)
  // Best-effort, so whoever is debugging this can read it on the server.
  await authedFetch('/api/voice/mic-report', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ report: lines.join('\n') }),
  }).catch(() => undefined)
}

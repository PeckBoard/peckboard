/**
 * "Thinking" feedback while the voice assistant waits for its reply:
 *
 * - a soft looping cue (a quiet low sine pulse, Web Audio, no asset) from
 *   the moment an utterance is sent until the first reply audio plays;
 * - one short spoken filler ("One sec.") when no reply audio has started
 *   `FILLER_DELAY_MS` after the send.
 *
 * Both play on the speech engine's already gesture-unlocked AudioContext
 * (see `SpeechEngine.audioContext`) — this module never creates one. The
 * fillers are synthesized by Kokoro ahead of time (`prepareFillers`), so
 * playing one has no synthesis delay. Neither is a message: nothing here
 * touches the transcript or the server session.
 */

import { authedFetch } from '../store/auth'
import { voiceLog } from './engine'
import {
  FIRST_CLIP_PREROLL_MS,
  KOKORO_PREFIX,
  isKokoroVoice,
  useKokoroDevicePrefs,
  withWakePreroll,
} from './kokoro'

/** Time between cue pulses. */
export const CUE_PERIOD_MS = 1200
/** Peak gain of a cue pulse — barely there. */
const CUE_GAIN = 0.05
const CUE_FREQ_HZ = 196
const CUE_ATTACK_S = 0.04
const CUE_RELEASE_S = 0.22
/** Fade when the cue is stopped mid-pulse. */
const CUE_STOP_FADE_S = 0.04
/** No reply audio this long after the send: say a filler. */
export const FILLER_DELAY_MS = 2000
/** Fade when a filler is cut off (barge-in, Stop, panel close). */
export const FILLER_STOP_FADE_MS = 40

export interface Filler {
  /** Plain words: the echo filter's view of what was said. */
  text: string
  /** Sent to Kokoro: every word carries its misaki phonemes. */
  spoken: string
}

/** Phonemes from misaki (`service::tts::kokoro::phonemize_with`). */
export const FILLERS: Filler[] = [
  { text: 'One sec.', spoken: '[One](/wˈʌn/) [sec](/sˈɛk/).' },
  {
    text: 'Let me think about that.',
    spoken: '[Let](/lˈɛt/) [me](/mˌiː/) [think](/θˈɪŋk/) [about](/ɐbˌWt/) [that](/ðæt/).',
  },
  {
    text: 'Give me a second.',
    spoken: '[Give](/ɡˈɪv/) [me](/mˌiː/) [a](/ɐ/) [second](/sˈɛkənd/).',
  },
  { text: 'One moment.', spoken: '[One](/wˈʌn/) [moment](/mˈOmənt/).' },
  { text: 'Let me check.', spoken: '[Let](/lˈɛt/) [me](/mˌiː/) [check](/ʧˈɛk/).' },
]

// ── Cue ──

let cue: { osc: OscillatorNode; gain: GainNode; timer: ReturnType<typeof setInterval> } | null =
  null

/** Start the looping cue (no-op if it's already on or `ctx` isn't running). */
export function startThinkingCue(ctx: AudioContext): void {
  if (cue || ctx.state !== 'running') return
  const osc = ctx.createOscillator()
  osc.type = 'sine'
  osc.frequency.value = CUE_FREQ_HZ
  const gain = ctx.createGain()
  gain.gain.value = 0
  osc.connect(gain).connect(ctx.destination)
  const pulse = () => {
    const t = ctx.currentTime
    gain.gain.cancelScheduledValues(t)
    gain.gain.setValueAtTime(0, t)
    gain.gain.linearRampToValueAtTime(CUE_GAIN, t + CUE_ATTACK_S)
    gain.gain.linearRampToValueAtTime(0, t + CUE_ATTACK_S + CUE_RELEASE_S)
  }
  osc.start()
  pulse()
  cue = { osc, gain, timer: setInterval(pulse, CUE_PERIOD_MS) }
  voiceLog('thinking cue: on')
}

export function stopThinkingCue(reason: string): void {
  if (!cue) return
  const { osc, gain, timer } = cue
  cue = null
  clearInterval(timer)
  const t = osc.context.currentTime
  gain.gain.cancelScheduledValues(t)
  gain.gain.setValueAtTime(gain.gain.value, t)
  gain.gain.linearRampToValueAtTime(0, t + CUE_STOP_FADE_S)
  try {
    osc.stop(t + CUE_STOP_FADE_S + 0.01)
  } catch {
    /* already stopped */
  }
  voiceLog(`thinking cue: off (${reason})`)
}

// ── Fillers ──

/** Synthesized fillers for one voice + speed (`key`). */
let cacheKey = ''
const cache = new Map<string, AudioBuffer>()
let preparing: Promise<void> | null = null
/** Last filler said, across turns: never the same one twice in a row. */
let lastFiller: string | null = null

function currentKey(voiceURI: string): string {
  return `${voiceURI}|${useKokoroDevicePrefs.getState().speed}`
}

/**
 * Synthesize (and decode) every filler for `voiceURI` at the current
 * Kokoro speed, unless already cached. Idempotent; a voice/speed change
 * drops the old set. Failures (Kokoro not ready: 503) leave the filler
 * missing — the next call retries.
 */
export function prepareFillers(voiceURI: string, ctx: AudioContext | null): void {
  if (!isKokoroVoice(voiceURI) || !ctx) return
  const key = currentKey(voiceURI)
  if (key !== cacheKey) {
    cacheKey = key
    cache.clear()
  }
  if (preparing || cache.size === FILLERS.length) return
  const speed = useKokoroDevicePrefs.getState().speed
  preparing = (async () => {
    // One at a time: the fillers mustn't crowd out a reply's synthesis.
    for (const f of FILLERS) {
      if (cacheKey !== key) return
      if (cache.has(f.text)) continue
      try {
        const res = await authedFetch('/api/voice/tts', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            text: f.spoken,
            voice: voiceURI.slice(KOKORO_PREFIX.length),
            speed,
          }),
        })
        if (!res.ok) {
          voiceLog(`thinking filler: synthesis HTTP ${res.status}; skipping fillers for now`)
          return
        }
        const wav = withWakePreroll(await res.arrayBuffer(), FIRST_CLIP_PREROLL_MS)
        const audio = await ctx.decodeAudioData(wav)
        if (cacheKey === key) cache.set(f.text, audio)
      } catch (e) {
        voiceLog('thinking filler: synthesis failed', e)
        return
      }
    }
  })().finally(() => {
    preparing = null
  })
}

/** A cached filler for `voiceURI`, never the one said last. */
export function pickFiller(voiceURI: string): { filler: Filler; audio: AudioBuffer } | null {
  if (currentKey(voiceURI) !== cacheKey) return null
  const ready = FILLERS.filter((f) => cache.has(f.text) && f.text !== lastFiller)
  if (ready.length === 0) return null
  const filler = ready[Math.floor(Math.random() * ready.length)]
  return { filler, audio: cache.get(filler.text)! }
}

export interface FillerPlayback {
  filler: Filler
  /** Cut it off (fade, then silence); `onEnd` still fires once. */
  stop: (fadeMs?: number) => void
}

/** Play a filler; `onEnd` fires once when it ends or is stopped. */
export function playFiller(
  ctx: AudioContext,
  filler: Filler,
  audio: AudioBuffer,
  onEnd: () => void,
): FillerPlayback {
  lastFiller = filler.text
  const src = ctx.createBufferSource()
  src.buffer = audio
  const gain = ctx.createGain()
  src.connect(gain).connect(ctx.destination)
  let done = false
  // A stalled output never ends the clip: don't hold the reply forever.
  const watchdog = setTimeout(() => stop(0), audio.duration * 1000 + 3000)
  const finish = () => {
    if (done) return
    done = true
    clearTimeout(watchdog)
    src.onended = null
    onEnd()
  }
  const stop = (fadeMs = FILLER_STOP_FADE_MS) => {
    if (done) return
    const t = ctx.currentTime
    const fade = Math.max(0, fadeMs) / 1000
    gain.gain.cancelScheduledValues(t)
    gain.gain.setValueAtTime(gain.gain.value, t)
    gain.gain.linearRampToValueAtTime(0, t + fade)
    try {
      src.stop(t + fade)
    } catch {
      /* not started */
    }
    finish()
  }
  src.onended = finish
  src.start()
  voiceLog('thinking filler:', JSON.stringify(filler.text))
  return { filler, stop }
}

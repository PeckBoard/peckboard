/**
 * Live audio levels and the orb state for the full-page Assistant
 * (`voice/assistantContract.ts`). Both level functions run every animation
 * frame: no allocation, never throw.
 *
 * - `output()` — the assistant's voice. Kokoro / speech-server clips on
 *   desktop play through Web Audio: read the pass-through analyser
 *   `KokoroEngine` puts in front of the destination. The iOS `<audio>`
 *   path is left untapped (routing it through Web Audio would bring back
 *   the crackle the element exists to avoid), and browser speechSynthesis
 *   voices can't be tapped at all: both get a synthesised speech-like
 *   envelope while the voice store says `speaking`.
 * - `input()` — the user's mic. With server recognition, the RMS of the
 *   PCM frames it already streams (`serverMicLevel`). Browser recognition
 *   exposes no audio, so interim results drive decaying pulses instead of
 *   opening a second microphone.
 */

import { useVoiceStore } from '../store/voice'
import type { AudioLevels, OrbState } from './assistantContract'
import { getSpeechEngine } from './engine'
import { KokoroEngine } from './kokoro'
import { serverMicLevel, wantsServerRecognition } from './serverStt'

/** One-pole smoothing time constants (ms): fast attack, slower release. */
const ATTACK_MS = 45
const RELEASE_MS = 180
/** Analyser RMS that maps to a full-scale level (speech rarely peaks). */
const OUTPUT_FULL_RMS = 0.25
/** Mic RMS that maps to full scale (after the browser's auto gain). */
const INPUT_FULL_RMS = 0.18
/** How fast an interim-result pulse fades (per second, multiplicative). */
const PULSE_DECAY_PER_S = 0.04

const clamp01 = (n: number) => (n < 0 ? 0 : n > 1 ? 1 : Number.isFinite(n) ? n : 0)

/** Smooth `target` into `prev` over `dt` ms. */
function smooth(prev: number, target: number, dt: number): number {
  const tau = target > prev ? ATTACK_MS : RELEASE_MS
  const k = 1 - Math.exp(-dt / tau)
  return prev + (target - prev) * k
}

// ── Output ──

let outLevel = 0
let outAt = 0
let samples: Float32Array<ArrayBuffer> = new Float32Array(0)

function analyserRms(a: AnalyserNode): number {
  if (samples.length !== a.fftSize) samples = new Float32Array(a.fftSize)
  a.getFloatTimeDomainData(samples)
  let sum = 0
  for (let i = 0; i < samples.length; i++) sum += samples[i] * samples[i]
  return Math.sqrt(sum / samples.length)
}

/** A plausible speech envelope at time `t` (ms): syllable-rate bumps
 *  riding a slower phrase contour. */
function syntheticSpeech(t: number): number {
  const s = t / 1000
  const syllable = Math.abs(Math.sin(s * Math.PI * 4.3))
  const phrase = 0.55 + 0.45 * Math.sin(s * Math.PI * 0.9 + 1.3)
  const flutter = 0.08 * Math.sin(s * Math.PI * 17.0)
  return 0.18 + 0.62 * syllable * phrase + flutter
}

function readOutput(): number {
  const now = performance.now()
  const dt = outAt === 0 ? 16 : Math.min(250, now - outAt)
  outAt = now
  let target = 0
  if (useVoiceStore.getState().status === 'speaking') {
    const engine = getSpeechEngine()
    const analyser = engine instanceof KokoroEngine ? engine.outputAnalyser() : null
    target = analyser ? analyserRms(analyser) / OUTPUT_FULL_RMS : syntheticSpeech(now)
  }
  outLevel = clamp01(smooth(outLevel, clamp01(target), dt))
  return outLevel
}

// ── Input ──

let inLevel = 0
let inAt = 0
/** Browser-recognition activity: bumped by each new hypothesis, decays. */
let pulse = 0
let pulseAt = 0
let watchingHypotheses = false

function watchHypotheses() {
  if (watchingHypotheses) return
  watchingHypotheses = true
  useVoiceStore.subscribe((s, prev) => {
    if (s.interim !== prev.interim && s.interim !== '') {
      // Longer hypotheses = more speech: a bigger bump, never past 1.
      pulse = Math.min(1, Math.max(pulse, 0.55) + 0.25)
      pulseAt = performance.now()
    } else if (s.heard !== prev.heard && s.heard !== '') {
      pulse = Math.max(pulse, 0.7)
      pulseAt = performance.now()
    }
  })
}

function readInput(): number {
  watchHypotheses()
  const now = performance.now()
  const dt = inAt === 0 ? 16 : Math.min(250, now - inAt)
  inAt = now
  let target = 0
  if (useVoiceStore.getState().micOn) {
    if (wantsServerRecognition()) {
      target = serverMicLevel() / INPUT_FULL_RMS
    } else if (pulse > 0) {
      const p = pulse * Math.pow(PULSE_DECAY_PER_S, (now - pulseAt) / 1000)
      // A little texture so a held pulse doesn't look like a flat bar.
      target = p * (0.8 + 0.2 * Math.abs(Math.sin(now / 70)))
      if (p < 0.01) pulse = 0
    }
  }
  inLevel = clamp01(smooth(inLevel, clamp01(target), dt))
  return inLevel
}

/** The orb's audio levels, each 0..1 and smoothed. */
export const audioLevels: AudioLevels = {
  input: () => {
    try {
      return readInput()
    } catch {
      return 0
    }
  },
  output: () => {
    try {
      return readOutput()
    } catch {
      return 0
    }
  },
}

/** The voice store's state machine as the orb shows it: `offline` until
 *  a conversation is open with its session. */
export function useOrbState(): OrbState {
  return useVoiceStore((s) => (!s.panelOpen || !s.sessionId ? 'offline' : s.status))
}

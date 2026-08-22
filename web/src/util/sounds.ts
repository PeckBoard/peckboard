// Per-browser notification sounds. localStorage, not the DB — same class
// of state as theme/density (AGENTS.md). Synthesized over Web Audio so
// there are no asset files to license or embed.

export const SOUNDS_KEY = 'peckboard_sounds'

export const SOUND_KINDS = [
  'question',
  'runComplete',
  'accountLimit',
  'runStart',
  'toolUsed',
  'messageSent',
  'queueProcessed',
] as const

export type SoundKind = (typeof SOUND_KINDS)[number]

export type SoundPrefs = {
  /** Master mute. When false, no event plays (Preview still works). */
  enabled: boolean
} & Record<SoundKind, boolean>

/** Frequent / redundant chimes start OFF. Attention-worthy ones start ON. */
export const DEFAULT_SOUND_PREFS: SoundPrefs = {
  enabled: true,
  question: true,
  runComplete: true,
  accountLimit: true,
  runStart: false,
  toolUsed: false,
  messageSent: false,
  queueProcessed: false,
}

export const SOUND_META: Record<SoundKind, { label: string; hint: string }> = {
  question: {
    label: 'Question',
    hint: 'An agent or worker needs an answer, or a sudo / env-unlock prompt appeared.',
  },
  runComplete: {
    label: 'Run complete',
    hint: 'A turn finished successfully.',
  },
  accountLimit: {
    label: 'Account at limit',
    hint: 'A provider budget, plan quota, or project spend cap was hit.',
  },
  runStart: {
    label: 'Run start',
    hint: 'An agent turn began. Off by default — workers fire this often.',
  },
  toolUsed: {
    label: 'Tool used',
    hint: 'A tool call finished. Off by default — a single turn can fire dozens.',
  },
  messageSent: {
    label: 'Message sent',
    hint: 'Your send was accepted. Off by default — you already clicked Send.',
  },
  queueProcessed: {
    label: 'Queued message sent',
    hint: 'A parked message left the queue and went to the agent.',
  },
}

type Note = { f: number; at: number; dur: number; gain: number }

/** Distinct pentatonic motifs. Short, quiet, no alarm tones. */
const MOTIFS: Record<SoundKind, Note[]> = {
  // Soft doorbell: rising fifth.
  question: [
    { f: 523.25, at: 0, dur: 0.22, gain: 0.09 },
    { f: 783.99, at: 0.14, dur: 0.28, gain: 0.08 },
  ],
  // Major arpeggio up — "done".
  runComplete: [
    { f: 392.0, at: 0, dur: 0.16, gain: 0.08 },
    { f: 523.25, at: 0.1, dur: 0.16, gain: 0.08 },
    { f: 659.25, at: 0.2, dur: 0.28, gain: 0.09 },
  ],
  // Gentle falling fifth — warning, not a klaxon.
  accountLimit: [
    { f: 440.0, at: 0, dur: 0.22, gain: 0.09 },
    { f: 329.63, at: 0.16, dur: 0.32, gain: 0.08 },
  ],
  // Tiny uptick.
  runStart: [{ f: 523.25, at: 0, dur: 0.08, gain: 0.05 }],
  // Quiet high tick.
  toolUsed: [{ f: 987.77, at: 0, dur: 0.045, gain: 0.04 }],
  // Soft pop.
  messageSent: [{ f: 587.33, at: 0, dur: 0.06, gain: 0.05 }],
  // Two-note "next".
  queueProcessed: [
    { f: 392.0, at: 0, dur: 0.09, gain: 0.06 },
    { f: 493.88, at: 0.08, dur: 0.12, gain: 0.06 },
  ],
}

const COOLDOWN_MS = 400
const LIVE_WINDOW_MS = 8_000

let ctx: AudioContext | null = null
let unlocked = false
const lastPlayed = new Map<SoundKind, number>()

export function loadSoundPrefs(): SoundPrefs {
  try {
    const raw = localStorage.getItem(SOUNDS_KEY)
    if (!raw) return { ...DEFAULT_SOUND_PREFS }
    const parsed = JSON.parse(raw) as Partial<SoundPrefs>
    const prefs = { ...DEFAULT_SOUND_PREFS }
    if (typeof parsed.enabled === 'boolean') prefs.enabled = parsed.enabled
    for (const kind of SOUND_KINDS) {
      if (typeof parsed[kind] === 'boolean') prefs[kind] = parsed[kind]
    }
    return prefs
  } catch {
    return { ...DEFAULT_SOUND_PREFS }
  }
}

export function saveSoundPrefs(prefs: SoundPrefs): void {
  try {
    localStorage.setItem(SOUNDS_KEY, JSON.stringify(prefs))
  } catch {
    /* private mode */
  }
}

export function isSoundEnabled(kind: SoundKind, prefs: SoundPrefs = loadSoundPrefs()): boolean {
  return prefs.enabled && prefs[kind]
}

/** Skip replayed history after a WS resume. ts is unix millis (or seconds). */
export function isLiveEventTs(ts: number, now = Date.now()): boolean {
  if (!Number.isFinite(ts) || ts <= 0) return true
  const ms = ts < 1e12 ? ts * 1000 : ts
  return now - ms < LIVE_WINDOW_MS
}

function getCtx(): AudioContext | null {
  const AC =
    window.AudioContext ||
    (window as unknown as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext
  if (!AC) return null
  if (!ctx) ctx = new AC()
  return ctx
}

/** Browsers block audio until a user gesture. Login is a click; this is backup. */
export function unlockSounds(): void {
  unlocked = true
  const c = getCtx()
  if (c && c.state === 'suspended') void c.resume()
}

function emitPlayed(kind: SoundKind): void {
  window.dispatchEvent(new CustomEvent('peckboard:sound', { detail: { kind } }))
}

function playMotif(kind: SoundKind): void {
  const c = getCtx()
  if (!c) {
    emitPlayed(kind)
    return
  }
  if (c.state === 'suspended') void c.resume()
  const t0 = c.currentTime + 0.01
  const filter = c.createBiquadFilter()
  filter.type = 'lowpass'
  filter.frequency.value = 2400
  filter.connect(c.destination)
  for (const n of MOTIFS[kind]) {
    const osc = c.createOscillator()
    const g = c.createGain()
    osc.type = 'sine'
    osc.frequency.value = n.f
    const start = t0 + n.at
    g.gain.setValueAtTime(0.0001, start)
    g.gain.exponentialRampToValueAtTime(n.gain, start + 0.012)
    g.gain.exponentialRampToValueAtTime(0.0001, start + n.dur)
    osc.connect(g)
    g.connect(filter)
    osc.start(start)
    osc.stop(start + n.dur + 0.03)
  }
  emitPlayed(kind)
}

/** Preview ignores mute/toggles so the user can hear a sample. */
export function previewSound(kind: SoundKind): void {
  unlockSounds()
  playMotif(kind)
}

export function playSound(kind: SoundKind): void {
  if (!unlocked && typeof document !== 'undefined' && document.hasFocus()) {
    unlockSounds()
  }
  if (!isSoundEnabled(kind)) return
  const now = Date.now()
  const prev = lastPlayed.get(kind) ?? 0
  if (now - prev < COOLDOWN_MS) return
  lastPlayed.set(kind, now)
  playMotif(kind)
}

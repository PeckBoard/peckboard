/**
 * One-at-a-time media playback, app-wide.
 *
 * Every inline player (shared-file audio / video today, embeds later)
 * registers here. Starting one pauses all the others, Assistant speech
 * pauses all of them (`pauseAllMedia`), and the always-on Assistant mic
 * ignores what it hears while any of them is audible (`mediaAudible`) — it
 * would otherwise transcribe the clip.
 */
import { useEffect, useState, type RefObject } from 'react'

export interface MediaHandle {
  /** Call when this player starts (or resumes) playing. Pauses the rest. */
  playing(): void
  /** Call when it pauses, ends, or errors. */
  stopped(): void
  unregister(): void
  /** Swap the function that pauses this player. */
  setPause(pause: () => void): void
}

interface Entry {
  pause: () => void
}

const playingNow = new Set<Entry>()
const listeners = new Set<(playing: boolean) => void>()
let lastStoppedAt = 0
/** Speaker tail after a clip stops that the mic may still pick up. */
const AUDIBLE_TAIL_MS = 700

function notify() {
  const on = playingNow.size > 0
  for (const fn of listeners) fn(on)
}

/** Register a player; `pause` must stop it (and should lead to `stopped`). */
export function registerMedia(pause: () => void): MediaHandle {
  const entry: Entry = { pause }
  const stop = () => {
    if (!playingNow.delete(entry)) return
    lastStoppedAt = Date.now()
    notify()
  }
  return {
    playing() {
      for (const other of [...playingNow]) {
        if (other === entry) continue
        playingNow.delete(other)
        other.pause()
      }
      if (playingNow.has(entry)) return
      playingNow.add(entry)
      notify()
    },
    stopped: stop,
    unregister: stop,
    setPause(fn) {
      entry.pause = fn
    },
  }
}

/** Pause every registered player (e.g. the Assistant starts speaking). */
export function pauseAllMedia(): void {
  if (playingNow.size === 0) return
  for (const e of [...playingNow]) {
    playingNow.delete(e)
    e.pause()
  }
  lastStoppedAt = Date.now()
  notify()
}

/** Whether any registered player is playing right now. */
export function isMediaPlaying(): boolean {
  return playingNow.size > 0
}

/** Playing now, or stopped so recently its sound may still reach the mic. */
export function mediaAudible(): boolean {
  return playingNow.size > 0 || Date.now() - lastStoppedAt < AUDIBLE_TAIL_MS
}

export function subscribeMediaPlaying(fn: (playing: boolean) => void): () => void {
  listeners.add(fn)
  return () => listeners.delete(fn)
}

/**
 * Register a custom player (an iframe embed, a Web Audio graph) for the
 * component's lifetime. Returns a stable handle: call `playing()` /
 * `stopped()` from the player's own events. `pause` may change per render.
 */
export function useMediaPlayback(pause: () => void): MediaHandle {
  const [handle] = useState(() => registerMedia(() => {}))
  useEffect(() => handle.setPause(pause), [handle, pause])
  useEffect(() => () => handle.unregister(), [handle])
  return handle
}

/** Wire a native `<audio>` / `<video>` element into the coordinator. */
export function useMediaElementPlayback(
  ref: RefObject<HTMLMediaElement | null>,
  // Re-attach when the element is swapped (e.g. a re-minted `src`).
  key?: unknown,
): void {
  useEffect(() => {
    const el = ref.current
    if (!el) return
    const h = registerMedia(() => el.pause())
    const onPlay = () => h.playing()
    const onStop = () => h.stopped()
    el.addEventListener('play', onPlay)
    el.addEventListener('pause', onStop)
    el.addEventListener('ended', onStop)
    el.addEventListener('error', onStop)
    el.addEventListener('emptied', onStop)
    if (!el.paused) h.playing()
    return () => {
      el.removeEventListener('play', onPlay)
      el.removeEventListener('pause', onStop)
      el.removeEventListener('ended', onStop)
      el.removeEventListener('error', onStop)
      el.removeEventListener('emptied', onStop)
      h.unregister()
    }
  }, [ref, key])
}

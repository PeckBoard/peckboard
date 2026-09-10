/**
 * Unlock windows for encrypted environment variables.
 *
 * The values are the wire form of the server's `UnlockDuration`
 * (`src/service/env_vars.rs`) — an unknown value is a 400 there, so the two
 * lists have to stay in sync.
 */
export type UnlockDuration = '15m' | '1h' | '4h' | '8h' | '24h' | 'until-lock'

export const UNLOCK_DURATIONS: { value: UnlockDuration; label: string }[] = [
  { value: '15m', label: '15 minutes' },
  { value: '1h', label: '1 hour' },
  { value: '4h', label: '4 hours' },
  { value: '8h', label: '8 hours' },
  { value: '24h', label: '24 hours' },
  { value: 'until-lock', label: 'Until I lock' },
]

export const DEFAULT_UNLOCK_DURATION: UnlockDuration = '1h'

const STORAGE_KEY = 'peckboard:env-unlock-duration'

/**
 * The window the user picked last time, so the choice carries across prompts
 * instead of resetting to the default on every dialog.
 */
export function loadUnlockDuration(): UnlockDuration {
  try {
    const stored = localStorage.getItem(STORAGE_KEY)
    if (stored && UNLOCK_DURATIONS.some((d) => d.value === stored)) {
      return stored as UnlockDuration
    }
  } catch {
    // Blocked storage (private mode): fall through to the default.
  }
  return DEFAULT_UNLOCK_DURATION
}

export function saveUnlockDuration(duration: UnlockDuration): void {
  try {
    localStorage.setItem(STORAGE_KEY, duration)
  } catch {
    // Non-fatal — the picker just won't remember the choice.
  }
}

/**
 * `2h 5m` / `42m` / `30s`. Coarse above a minute so the countdown doesn't
 * redraw a new string every second for hours on end.
 */
export function formatRemaining(secs: number): string {
  const total = Math.max(0, Math.floor(secs))
  if (total < 60) return `${total}s`
  const mins = Math.floor(total / 60)
  if (mins < 60) return `${mins}m`
  const hours = Math.floor(mins / 60)
  const rem = mins % 60
  return rem === 0 ? `${hours}h` : `${hours}h ${rem}m`
}

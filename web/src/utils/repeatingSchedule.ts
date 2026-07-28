import type { RepeatingScheduleKind } from '../types/api'

const WEEKDAYS = ['Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday', 'Sunday']
/** Mirrors `MIN_INTERVAL_MINUTES` in `src/repeating.rs`. */
export const MIN_INTERVAL_MINUTES = 1
/** One year of minutes — the editor's upper bound. */
export const MAX_INTERVAL_MINUTES = 525600

/**
 * The unmet requirement for a schedule draft, or `''` when it is valid.
 *
 * The editor deliberately does NOT clamp an out-of-range interval up to the
 * minimum: silently rewriting 0 to 1 turns "don't repeat" into "run every
 * minute". Callers use this to state the problem and block submit instead.
 */
export function scheduleProblem(
  kind: RepeatingScheduleKind,
  value: Record<string, number>,
): string {
  if (kind !== 'interval') return ''
  const minutes = value.minutes
  if (!Number.isFinite(minutes) || minutes < MIN_INTERVAL_MINUTES) {
    return `Every (minutes) must be at least ${MIN_INTERVAL_MINUTES}`
  }
  if (minutes > MAX_INTERVAL_MINUTES) {
    return `Every (minutes) must be at most ${MAX_INTERVAL_MINUTES}`
  }
  return ''
}

/** Render a one-line human description of a schedule for list/detail views. */
export function describeSchedule(kind: RepeatingScheduleKind, valueJson: string): string {
  let parsed: Record<string, number>
  try {
    parsed = JSON.parse(valueJson)
  } catch {
    return 'Invalid schedule'
  }
  switch (kind) {
    case 'interval': {
      const m = parsed.minutes ?? 0
      if (m % 1440 === 0) return `Every ${m / 1440} day${m === 1440 ? '' : 's'}`
      if (m % 60 === 0) return `Every ${m / 60} hour${m === 60 ? '' : 's'}`
      return `Every ${m} minute${m === 1 ? '' : 's'}`
    }
    case 'daily': {
      const h = String(parsed.hour ?? 0).padStart(2, '0')
      const min = String(parsed.minute ?? 0).padStart(2, '0')
      return `Daily at ${h}:${min} UTC`
    }
    case 'weekly': {
      const h = String(parsed.hour ?? 0).padStart(2, '0')
      const min = String(parsed.minute ?? 0).padStart(2, '0')
      const day = WEEKDAYS[parsed.weekday ?? 0] ?? '?'
      return `${day}s at ${h}:${min} UTC`
    }
  }
}

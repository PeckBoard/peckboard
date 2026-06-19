import type { RepeatingScheduleKind } from '../types/api'

const WEEKDAYS = ['Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday', 'Sunday']

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

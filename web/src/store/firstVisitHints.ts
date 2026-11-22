import { create } from 'zustand'

const STORAGE_KEY = 'peckboard_first_visit_hints_dismissed'

/** Storage id for one user's dismissal of one hint. */
export function hintId(userId: string, key: string): string {
  return `${userId}:${key}`
}

function readDismissed(): string[] {
  try {
    const raw = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? '[]')
    return Array.isArray(raw) ? raw.filter((v): v is string => typeof v === 'string') : []
  } catch {
    return []
  }
}

interface FirstVisitHintsState {
  /** `userId:key` of every hint dismissed on this browser. */
  dismissed: string[]
  dismiss: (userId: string, key: string) => void
}

/** Per-user, per-browser dismissals of the one-time page hints
 *  (`FirstVisitHint`). Kept in localStorage like the Getting Started card:
 *  a hint is a nudge, not data worth a server round-trip. */
export const useFirstVisitHintsStore = create<FirstVisitHintsState>((set, get) => ({
  dismissed: readDismissed(),
  dismiss: (userId, key) => {
    const id = hintId(userId, key)
    if (get().dismissed.includes(id)) return
    const dismissed = [...get().dismissed, id]
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(dismissed))
    } catch {
      /* private mode: hidden for this page load only */
    }
    set({ dismissed })
  },
}))

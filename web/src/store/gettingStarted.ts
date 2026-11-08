import { create } from 'zustand'

const DISMISSED_KEY = 'peckboard_getting_started_dismissed'

function readDismissed(): boolean {
  try {
    return localStorage.getItem(DISMISSED_KEY) === '1'
  } catch {
    return false
  }
}

interface GettingStartedState {
  /** Hidden by the user on this browser (persisted in localStorage). */
  dismissed: boolean
  /** Re-opened from the user menu: shown even when every step is done. */
  forced: boolean
  dismiss: () => void
  show: () => void
}

/** Per-browser visibility of the Getting Started checklist. No server state:
 *  the ticks come from real data, so only "I've hidden it" needs remembering. */
export const useGettingStartedStore = create<GettingStartedState>((set) => ({
  dismissed: readDismissed(),
  forced: false,
  dismiss: () => {
    try {
      localStorage.setItem(DISMISSED_KEY, '1')
    } catch {
      /* private mode: hidden for this page load only */
    }
    set({ dismissed: true, forced: false })
  },
  show: () => {
    try {
      localStorage.removeItem(DISMISSED_KEY)
    } catch {
      /* ignore */
    }
    set({ dismissed: false, forced: true })
  },
}))

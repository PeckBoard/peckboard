import { create } from 'zustand'

/** Per-install "don't ask again" for the backlog → work confirmation.
 *  Lives in localStorage, not the DB: it's a per-browser preference, and
 *  AGENTS.md says that class of state stays out of migrations. */
export const SKIP_BACKLOG_CONFIRM_KEY = 'peckboard_skip_backlog_confirm'

function readSkipBacklogConfirm(): boolean {
  try {
    return localStorage.getItem(SKIP_BACKLOG_CONFIRM_KEY) === '1'
  } catch {
    // Private-mode / storage-disabled browsers: fall back to asking.
    return false
  }
}

interface UiState {
  connected: boolean
  sidebarOpen: boolean
  /** True once the user ticked "don't ask again" on the backlog → work
   *  confirmation. Re-enabled from Settings → Appearance → Confirmations. */
  skipBacklogConfirm: boolean
  setConnected: (connected: boolean) => void
  toggleSidebar: () => void
  setSidebarOpen: (open: boolean) => void
  setSkipBacklogConfirm: (skip: boolean) => void
}

export const useUiStore = create<UiState>((set) => ({
  connected: false,
  sidebarOpen: true,
  skipBacklogConfirm: readSkipBacklogConfirm(),
  setConnected: (connected) => set({ connected }),
  toggleSidebar: () => set((s) => ({ sidebarOpen: !s.sidebarOpen })),
  setSidebarOpen: (open) => set({ sidebarOpen: open }),
  setSkipBacklogConfirm: (skip) => {
    try {
      if (skip) localStorage.setItem(SKIP_BACKLOG_CONFIRM_KEY, '1')
      else localStorage.removeItem(SKIP_BACKLOG_CONFIRM_KEY)
    } catch {
      // Storage unavailable — keep the in-memory value for this tab.
    }
    set({ skipBacklogConfirm: skip })
  },
}))

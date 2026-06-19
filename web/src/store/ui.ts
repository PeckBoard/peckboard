import { create } from 'zustand'

interface UiState {
  connected: boolean
  sidebarOpen: boolean
  setConnected: (connected: boolean) => void
  toggleSidebar: () => void
  setSidebarOpen: (open: boolean) => void
}

export const useUiStore = create<UiState>((set) => ({
  connected: false,
  sidebarOpen: true,
  setConnected: (connected) => set({ connected }),
  toggleSidebar: () => set((s) => ({ sidebarOpen: !s.sidebarOpen })),
  setSidebarOpen: (open) => set({ sidebarOpen: open }),
}))

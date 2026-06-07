import { create } from 'zustand'
import { authedFetch } from './auth'

export type TabType = 'session' | 'project'

export interface Tab {
  itemType: TabType
  itemId: string
  /** ISO timestamp; updated server-side every time the tab is opened.
   *  Frontend computes the unread badge by comparing this against the
   *  source's own activity timestamp (session.last_activity, etc.). */
  lastActive: string
}

interface TabsState {
  tabs: Tab[]
  loaded: boolean
  fetchTabs: () => Promise<void>
  /** Open or re-activate a tab. Optimistic: updates local order
   *  immediately, then write-through to the server. Idempotent. */
  openTab: (itemType: TabType, itemId: string) => Promise<void>
  /** Remove a tab from the strip. Does not delete the underlying
   *  session/project. */
  closeTab: (itemType: TabType, itemId: string) => Promise<void>
}

interface ApiTab {
  item_type: TabType
  item_id: string
  last_active: string
}

function fromApi(t: ApiTab): Tab {
  return { itemType: t.item_type, itemId: t.item_id, lastActive: t.last_active }
}

/** MRU sort: most recent `lastActive` first. */
function sortByMru(tabs: Tab[]): Tab[] {
  return [...tabs].sort((a, b) => (a.lastActive < b.lastActive ? 1 : -1))
}

export const useTabsStore = create<TabsState>((set, get) => ({
  tabs: [],
  loaded: false,

  fetchTabs: async () => {
    try {
      const res = await authedFetch('/api/me/tabs')
      if (!res.ok) return
      const data = (await res.json()) as ApiTab[]
      set({ tabs: sortByMru(data.map(fromApi)), loaded: true })
    } catch {
      // Network errors are non-fatal here — the tab strip still renders
      // whatever it has from the last successful fetch.
    }
  },

  openTab: async (itemType, itemId) => {
    // Optimistic: bump (or insert) this tab to the top with `now`.
    const now = new Date().toISOString()
    const existing = get().tabs.filter((t) => !(t.itemType === itemType && t.itemId === itemId))
    set({ tabs: [{ itemType, itemId, lastActive: now }, ...existing] })

    try {
      const res = await authedFetch('/api/me/tabs', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ item_type: itemType, item_id: itemId }),
      })
      if (res.ok) {
        const tab = fromApi((await res.json()) as ApiTab)
        // Replace the optimistic entry with the server-canonical one
        // (server-side timestamp wins so cross-device ordering matches).
        set((s) => ({
          tabs: sortByMru([
            tab,
            ...s.tabs.filter((t) => !(t.itemType === tab.itemType && t.itemId === tab.itemId)),
          ]),
        }))
      }
    } catch {
      // Leave the optimistic update in place; next focus refetch will
      // reconcile.
    }
  },

  closeTab: async (itemType, itemId) => {
    // Optimistic remove.
    set((s) => ({
      tabs: s.tabs.filter((t) => !(t.itemType === itemType && t.itemId === itemId)),
    }))
    try {
      await authedFetch(`/api/me/tabs/${itemType}/${itemId}`, { method: 'DELETE' })
    } catch {
      // ditto — focus refetch reconciles
    }
  },
}))

/** Set up cross-device sync: re-fetch when the window regains focus,
 *  and on a 60s timer as a fallback for never-focused tabs. */
export function startTabsAutoSync(): () => void {
  const refresh = () => useTabsStore.getState().fetchTabs()
  const onVis = () => {
    if (document.visibilityState === 'visible') refresh()
  }
  window.addEventListener('focus', refresh)
  document.addEventListener('visibilitychange', onVis)
  const interval = window.setInterval(refresh, 60_000)
  return () => {
    window.removeEventListener('focus', refresh)
    document.removeEventListener('visibilitychange', onVis)
    window.clearInterval(interval)
  }
}

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
  /** Open or re-activate a tab. New tabs are prepended to the strip;
   *  re-activating an existing tab does NOT change its position (tabs
   *  behave like browser tabs — clicking one selects it but doesn't
   *  shuffle the strip). Idempotent. */
  openTab: (itemType: TabType, itemId: string) => Promise<void>
  /** Remove a tab from the strip. Does not delete the underlying
   *  session/project. */
  closeTab: (itemType: TabType, itemId: string) => Promise<void>
  /** Remove a tab in response to its underlying item being deleted.
   *  Same as closeTab but kept as a separate entry point for clarity
   *  at call sites that own session/project deletion. */
  removeTabsForItem: (itemType: TabType, itemId: string) => void
}

interface ApiTab {
  item_type: TabType
  item_id: string
  last_active: string
}

function fromApi(t: ApiTab): Tab {
  return { itemType: t.item_type, itemId: t.item_id, lastActive: t.last_active }
}

/** Initial sort for tabs freshly loaded from the server: most recent
 *  `lastActive` first, so a brand-new device sees the user's MRU
 *  ordering. Local order is then preserved as the user works — clicking
 *  an existing tab no longer reshuffles it. */
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
      const incoming = data.map(fromApi)
      // First load: take server order verbatim (MRU). Subsequent
      // refetches (focus / 60s poll): preserve the local order the
      // user is looking at — add any new tabs at the end, drop any
      // tabs the server says are gone. Reshuffling the strip out
      // from under the user is jarring.
      set((s) => {
        if (!s.loaded) {
          return { tabs: sortByMru(incoming), loaded: true }
        }
        const incomingKey = (t: Tab) => `${t.itemType}:${t.itemId}`
        const incomingSet = new Set(incoming.map(incomingKey))
        const incomingMap = new Map(incoming.map((t) => [incomingKey(t), t]))
        const kept = s.tabs
          .filter((t) => incomingSet.has(incomingKey(t)))
          .map((t) => incomingMap.get(incomingKey(t)) ?? t)
        const keptKeys = new Set(kept.map(incomingKey))
        const added = incoming.filter((t) => !keptKeys.has(incomingKey(t)))
        return { tabs: [...kept, ...added], loaded: true }
      })
    } catch {
      // Network errors are non-fatal here — the tab strip still renders
      // whatever it has from the last successful fetch.
    }
  },

  openTab: async (itemType, itemId) => {
    const existing = get().tabs.find((t) => t.itemType === itemType && t.itemId === itemId)
    if (existing) {
      // Tab already open — selecting it should not move it. Don't
      // re-sort, don't even refresh `last_active` (we used to,
      // which then made the strip reshuffle on the next refetch).
      return
    }
    // New tab: prepend to the strip optimistically, then write through.
    const now = new Date().toISOString()
    set((s) => ({ tabs: [{ itemType, itemId, lastActive: now }, ...s.tabs] }))

    try {
      const res = await authedFetch('/api/me/tabs', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ item_type: itemType, item_id: itemId }),
      })
      if (res.ok) {
        const tab = fromApi((await res.json()) as ApiTab)
        // Replace the optimistic entry in place. If a concurrent
        // `fetchTabs` happened to wipe it before our POST returned
        // (race during initial auth), re-insert at the front so the
        // tab isn't silently lost.
        set((s) => {
          const exists = s.tabs.some((t) => t.itemType === tab.itemType && t.itemId === tab.itemId)
          return exists
            ? {
                tabs: s.tabs.map((t) =>
                  t.itemType === tab.itemType && t.itemId === tab.itemId ? tab : t,
                ),
              }
            : { tabs: [tab, ...s.tabs] }
        })
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

  removeTabsForItem: (itemType, itemId) => {
    set((s) => ({
      tabs: s.tabs.filter((t) => !(t.itemType === itemType && t.itemId === itemId)),
    }))
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

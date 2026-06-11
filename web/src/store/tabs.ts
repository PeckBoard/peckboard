import { create } from 'zustand'
import { authedFetch } from './auth'

export type TabType = 'session' | 'project' | 'report' | 'repeating_task'

export interface Tab {
  itemType: TabType
  itemId: string
  /** ISO timestamp; updated server-side every time the tab is opened.
   *  Frontend computes the unread badge by comparing this against the
   *  source's own activity timestamp (session.last_activity, etc.). */
  lastActive: string
  /** Denormalized name of the underlying item (session/project name,
   *  repeating task name, or report title). The server resolves this
   *  on the way out — see /api/me/tabs in src/routes/me.rs. Worker
   *  sessions (`is_worker=true`) are not in the regular sessions list,
   *  so the strip relied on this field to label them; without it the
   *  chip rendered as a generic "Session" and the cleanup loop closed
   *  the tab as soon as the sessions list loaded. */
  name: string
  /** Denormalized `sessions.is_worker` for session tabs (always false
   *  for project / report / repeating-task tabs). Tabs for worker
   *  sessions hide the "Delete session" context-menu entry — worker
   *  sessions are owned by their card and the backend refuses
   *  DELETE /api/sessions/:id for them. */
  isWorker: boolean
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
  name?: string
  is_worker?: boolean
}

function fromApi(t: ApiTab): Tab {
  return {
    itemType: t.item_type,
    itemId: t.item_id,
    lastActive: t.last_active,
    name: t.name ?? '',
    isWorker: t.is_worker ?? false,
  }
}

/** Initial sort for tabs freshly loaded from the server: most recent
 *  `lastActive` first, so a brand-new device sees the user's MRU
 *  ordering. Local order is then preserved as the user works — clicking
 *  an existing tab no longer reshuffles it. */
function sortByMru(tabs: Tab[]): Tab[] {
  return [...tabs].sort((a, b) => (a.lastActive < b.lastActive ? 1 : -1))
}

/** Build a stable key for a (type, id) pair so we can intern it in a
 *  Set. Used by the tombstone logic below to avoid re-inserting a tab
 *  whose underlying item was deleted while its openTab POST was in
 *  flight. */
function tabKey(itemType: TabType, itemId: string): string {
  return `${itemType}:${itemId}`
}

/** Items whose tab was just removed by `removeTabsForItem` while an
 *  `openTab` POST for the same id is still in flight. Without this set,
 *  the POST response handler unconditionally re-inserts the tab — which
 *  resurrects a chip for a session that was just deleted on another
 *  device. The set is process-global (not in zustand state) because it's
 *  a transient race guard, not user-visible state. */
const recentTombstones = new Set<string>()

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
    // Empty `name` is fine — the upsert response (or the next refetch)
    // backfills it, and TabBar falls back to a sensible default.
    const now = new Date().toISOString()
    set((s) => ({
      tabs: [{ itemType, itemId, lastActive: now, name: '', isWorker: false }, ...s.tabs],
    }))

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
        // tab isn't silently lost. EXCEPT when the wipe came from an
        // explicit `removeTabsForItem` (the session was deleted on
        // another device while our POST was in flight) — the
        // tombstone tells us not to resurrect a chip for an item
        // that no longer exists.
        const key = tabKey(tab.itemType, tab.itemId)
        const tombstoned = recentTombstones.has(key)
        if (tombstoned) recentTombstones.delete(key)
        set((s) => {
          const exists = s.tabs.some((t) => t.itemType === tab.itemType && t.itemId === tab.itemId)
          if (exists) {
            return {
              tabs: s.tabs.map((t) =>
                t.itemType === tab.itemType && t.itemId === tab.itemId ? tab : t,
              ),
            }
          }
          // Disappeared from the strip while we were waiting on the
          // POST. If it was tombstoned (deleted-elsewhere), leave it
          // out; otherwise restore (initial-auth fetchTabs race).
          return tombstoned ? {} : { tabs: [tab, ...s.tabs] }
        })
      } else {
        // Server refused (404 = referenced item is gone, 4xx more
        // generally). Roll the optimistic add back so the strip
        // doesn't leak a phantom chip for an item the server won't
        // store.
        set((s) => ({
          tabs: s.tabs.filter((t) => !(t.itemType === itemType && t.itemId === itemId)),
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

  removeTabsForItem: (itemType, itemId) => {
    // Tombstone the id so an in-flight `openTab` POST response can't
    // re-insert the chip after we drop it here. The set is cleared as
    // soon as the response is processed or the next openTab succeeds.
    recentTombstones.add(tabKey(itemType, itemId))
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

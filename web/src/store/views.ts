import { create } from 'zustand'
import { authedFetch } from './auth'
import { sanitizeLayout, type LayoutNode } from '../lib/layoutTree'

/** A saved multi-session View as listed by `GET /api/me/views`. */
export interface ViewSummary {
  id: string
  name: string
  created_at: string
  updated_at: string
}

/** Full view shape (`GET/POST/PUT /api/me/views[/:id]`). */
export interface SavedView extends ViewSummary {
  layout: LayoutNode | null
}

async function errorOf(res: Response, fallback: string): Promise<Error> {
  const data = await res.json().catch(() => null)
  return new Error((data && typeof data.error === 'string' && data.error) || fallback)
}

function toSaved(raw: SavedView): SavedView {
  return { ...raw, layout: sanitizeLayout(raw.layout) }
}

interface ViewsState {
  views: ViewSummary[]
  loaded: boolean
  error: string
  fetchViews: () => Promise<void>
  getView: (id: string) => Promise<SavedView>
  createView: (name: string, layout: LayoutNode | null) => Promise<SavedView>
  updateView: (
    id: string,
    patch: { name?: string; layout?: LayoutNode | null },
  ) => Promise<SavedView>
  deleteView: (id: string) => Promise<void>
}

/** Keep the list row in sync with a full view the server just returned. */
function upsert(views: ViewSummary[], v: SavedView): ViewSummary[] {
  const row: ViewSummary = {
    id: v.id,
    name: v.name,
    created_at: v.created_at,
    updated_at: v.updated_at,
  }
  return views.some((x) => x.id === v.id)
    ? views.map((x) => (x.id === v.id ? row : x))
    : [row, ...views]
}

export const useViewsStore = create<ViewsState>((set) => ({
  views: [],
  loaded: false,
  error: '',

  fetchViews: async () => {
    try {
      const res = await authedFetch('/api/me/views')
      if (!res.ok) throw await errorOf(res, 'Failed to load views')
      const data = await res.json()
      const list: ViewSummary[] = Array.isArray(data) ? data : (data.views ?? [])
      set({ views: list, loaded: true, error: '' })
    } catch (err) {
      set({ loaded: true, error: err instanceof Error ? err.message : 'Failed to load views' })
    }
  },

  getView: async (id) => {
    const res = await authedFetch(`/api/me/views/${encodeURIComponent(id)}`)
    if (!res.ok)
      throw await errorOf(res, res.status === 404 ? 'View not found' : 'Failed to load view')
    return toSaved(await res.json())
  },

  createView: async (name, layout) => {
    const res = await authedFetch('/api/me/views', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name, layout }),
    })
    if (!res.ok) throw await errorOf(res, 'Failed to create view')
    const v = toSaved(await res.json())
    set((s) => ({ views: upsert(s.views, v) }))
    return v
  },

  updateView: async (id, patch) => {
    const res = await authedFetch(`/api/me/views/${encodeURIComponent(id)}`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(patch),
    })
    if (!res.ok) throw await errorOf(res, 'Failed to save view')
    const v = toSaved(await res.json())
    set((s) => ({ views: upsert(s.views, v) }))
    return v
  },

  deleteView: async (id) => {
    const res = await authedFetch(`/api/me/views/${encodeURIComponent(id)}`, { method: 'DELETE' })
    if (!res.ok && res.status !== 404) throw await errorOf(res, 'Failed to delete view')
    set((s) => ({ views: s.views.filter((v) => v.id !== id) }))
  },
}))

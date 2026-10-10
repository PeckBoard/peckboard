import { create } from 'zustand'
import { authedFetch } from './auth'
import { clampRect, type GridItem } from '../lib/widgetGrid'

/** Display identity of a terminal a view references. `closed` = soft-closed:
 *  the shell is gone but its pane can reopen one on the same host. */
export interface ViewTerminalMeta {
  name: string
  host_label: string
  plugin_id: string
  host_id: string
  closed: boolean
}
export function terminalMeta(t: {
  name: string
  host_label: string
  plugin_id: string
  host_id: string
}): ViewTerminalMeta {
  return {
    name: t.name,
    host_label: t.host_label,
    plugin_id: t.plugin_id,
    host_id: t.host_id,
    closed: false,
  }
}

/** A saved multi-session View as listed by `GET /api/me/views`. */
export interface ViewSummary {
  id: string
  name: string
  created_at: string
  updated_at: string
}

export type WidgetKind = 'session' | 'terminal' | 'project'

/** One widget on a view's 12-column grid. The ref key matches `kind`; a
 *  null ref (empty, or its target was deleted) shows a picker. */
export interface ViewWidget extends GridItem {
  kind: WidgetKind
  sessionId?: string | null
  terminalId?: string | null
  projectId?: string | null
}

/** Display identity of a project a view references. */
export interface ViewProjectMeta {
  name: string
}

/** The ref a widget of `kind` points at, or null. */
export function widgetRef(w: ViewWidget): string | null {
  return (
    (w.kind === 'session' ? w.sessionId : w.kind === 'terminal' ? w.terminalId : w.projectId) ??
    null
  )
}

/** Copy of `w` pointing at `ref` (which may switch its kind). Only the
 *  matching ref key is set — the server rejects two refs. */
export function withRef(w: ViewWidget, kind: WidgetKind, ref: string | null): ViewWidget {
  const { id, x, y, w: width, h } = w
  const base = { id, x, y, w: width, h, kind }
  if (kind === 'session') return { ...base, sessionId: ref }
  if (kind === 'terminal') return { ...base, terminalId: ref }
  return { ...base, projectId: ref }
}

/** Full view shape (`GET/POST/PUT /api/me/views[/:id]`). `terminals` /
 *  `projects` map each referenced terminal / project to its display
 *  identity. */
export interface SavedView extends ViewSummary {
  widgets: ViewWidget[]
  terminals?: Record<string, ViewTerminalMeta>
  projects?: Record<string, ViewProjectMeta>
}

async function errorOf(res: Response, fallback: string): Promise<Error> {
  const data = await res.json().catch(() => null)
  return new Error((data && typeof data.error === 'string' && data.error) || fallback)
}

const KINDS: WidgetKind[] = ['session', 'terminal', 'project']

/** Drop malformed widgets and clamp rects to the contract bounds. */
function sanitizeWidgets(raw: unknown): ViewWidget[] {
  if (!Array.isArray(raw)) return []
  const seen = new Set<string>()
  const out: ViewWidget[] = []
  for (const r of raw as Partial<ViewWidget>[]) {
    if (!r || typeof r.id !== 'string' || seen.has(r.id) || !KINDS.includes(r.kind!)) continue
    seen.add(r.id)
    const key =
      r.kind === 'session' ? 'sessionId' : r.kind === 'terminal' ? 'terminalId' : 'projectId'
    const ref = typeof r[key] === 'string' ? (r[key] as string) : null
    out.push(
      withRef(
        {
          id: r.id,
          kind: r.kind!,
          ...clampRect({ x: r.x ?? 0, y: r.y ?? 0, w: r.w ?? 6, h: r.h ?? 8 }),
        },
        r.kind!,
        ref,
      ),
    )
  }
  return out
}

function toSaved(raw: SavedView): SavedView {
  return { ...raw, widgets: sanitizeWidgets(raw.widgets) }
}

interface ViewsState {
  views: ViewSummary[]
  loaded: boolean
  error: string
  fetchViews: () => Promise<void>
  getView: (id: string) => Promise<SavedView>
  createView: (name: string, widgets: ViewWidget[]) => Promise<SavedView>
  updateView: (id: string, patch: { name?: string; widgets?: ViewWidget[] }) => Promise<SavedView>
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

  createView: async (name, widgets) => {
    const res = await authedFetch('/api/me/views', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name, widgets }),
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

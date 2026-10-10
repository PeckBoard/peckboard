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

export type WidgetKind =
  | 'session'
  | 'terminal'
  | 'project'
  | 'note'
  | 'report'
  | 'background'
  | 'repeating'
  | 'todos'
  | 'attention'
  | 'review_queue'
  | 'review_quality'
  | 'workers'
  | 'worktrees'
  | 'dependencies'
  | 'ssh_activity'
  | 'ssh_hosts'

/** One widget on a view's 12-column grid. The ref key matches `kind`; a
 *  null ref (empty, or its target was deleted) shows a picker. For the
 *  project-scoped info kinds `projectId` is an optional scope (null = all
 *  projects). See `tmp-widgets2-contract.md`. */
export interface ViewWidget extends GridItem {
  kind: WidgetKind
  sessionId?: string | null
  terminalId?: string | null
  projectId?: string | null
  /** `dependencies`: optional root card. */
  cardId?: string | null
  /** `note`: markdown body. */
  body?: string | null
  /** `report`: `"<folder>/<file>"`; null = latest report. */
  reportRef?: string | null
  /** `ssh_activity`: ssh-fleet host id; null = all hosts. */
  hostRef?: string | null
}

/** Display identity of a project a view references. */
export interface ViewProjectMeta {
  name: string
}
export type WidgetField =
  | 'sessionId'
  | 'terminalId'
  | 'projectId'
  | 'cardId'
  | 'body'
  | 'reportRef'
  | 'hostRef'

/** Ref fields each kind may carry; the server 400s any other field. The
 *  first entry is the kind's primary ref (see `widgetRef`). */
export const KIND_FIELDS: Record<WidgetKind, readonly WidgetField[]> = {
  session: ['sessionId'],
  terminal: ['terminalId'],
  project: ['projectId'],
  note: ['body'],
  report: ['reportRef'],
  background: [],
  repeating: [],
  todos: ['projectId'],
  attention: ['projectId'],
  review_queue: ['projectId'],
  review_quality: ['projectId'],
  workers: ['projectId'],
  worktrees: ['projectId'],
  dependencies: ['projectId', 'cardId'],
  ssh_activity: ['hostRef'],
  ssh_hosts: [],
}

/** Max note body, matching the server's bound. */
export const NOTE_MAX = 20_000
/** Max `hostRef`, matching the server's bound. */
const HOST_REF_MAX = 200

/** The primary ref a widget points at (session / terminal / project or
 *  scope project / report / host / note body), or null. */
export function widgetRef(w: ViewWidget): string | null {
  const key = KIND_FIELDS[w.kind]?.[0]
  return (key && w[key]) || null
}

/** Clamp string fields to their server bounds. */
function bound(w: ViewWidget): ViewWidget {
  if (typeof w.body === 'string') w.body = w.body.slice(0, NOTE_MAX)
  if (typeof w.hostRef === 'string') w.hostRef = w.hostRef.slice(0, HOST_REF_MAX)
  return w
}

/** Copy of `w` as `kind` with its primary ref set to `ref` (which may
 *  switch its kind). Only the kind's own fields are set — the server
 *  rejects foreign ones; secondary fields (a dependencies root card) are
 *  kept only while kind and primary ref stay the same. */
export function withRef(w: ViewWidget, kind: WidgetKind, ref: string | null): ViewWidget {
  const { id, x, y, w: width, h } = w
  const out: ViewWidget = { id, x, y, w: width, h, kind }
  const [primary, ...rest] = KIND_FIELDS[kind]
  if (primary) out[primary] = ref
  const same = w.kind === kind && (!primary || (w[primary] ?? null) === ref)
  for (const f of rest) out[f] = same ? (w[f] ?? null) : null
  return bound(out)
}

/** Copy of `w` with `patch` applied to the fields its kind allows. A new
 *  scope project clears a dependencies root card picked inside the old one. */
export function patchWidget(
  w: ViewWidget,
  patch: Partial<Pick<ViewWidget, WidgetField>>,
): ViewWidget {
  const out = { ...w }
  for (const f of KIND_FIELDS[w.kind]) if (f in patch) out[f] = patch[f] ?? null
  if (
    w.kind === 'dependencies' &&
    'projectId' in patch &&
    (patch.projectId ?? null) !== (w.projectId ?? null) &&
    !('cardId' in patch)
  )
    out.cardId = null
  return bound(out)
}

/** Full view shape (`GET/POST/PUT /api/me/views[/:id]`). `terminals` /
 *  `projects` map each referenced terminal / project to its display
 *  identity. */
export interface SavedView extends ViewSummary {
  widgets: ViewWidget[]
  terminals?: Record<string, ViewTerminalMeta>
  projects?: Record<string, ViewProjectMeta>
  /** Titles of cards referenced by `cardId`s. */
  cards?: Record<string, { title: string }>
}

async function errorOf(res: Response, fallback: string): Promise<Error> {
  const data = await res.json().catch(() => null)
  return new Error((data && typeof data.error === 'string' && data.error) || fallback)
}

const KINDS = Object.keys(KIND_FIELDS) as WidgetKind[]

/** Drop malformed widgets and clamp rects / strings to the contract bounds. */
function sanitizeWidgets(raw: unknown): ViewWidget[] {
  if (!Array.isArray(raw)) return []
  const seen = new Set<string>()
  const out: ViewWidget[] = []
  for (const r of raw as Partial<ViewWidget>[]) {
    if (!r || typeof r.id !== 'string' || seen.has(r.id) || !KINDS.includes(r.kind!)) continue
    seen.add(r.id)
    const w: ViewWidget = {
      id: r.id,
      kind: r.kind!,
      ...clampRect({ x: r.x ?? 0, y: r.y ?? 0, w: r.w ?? 6, h: r.h ?? 8 }),
    }
    for (const f of KIND_FIELDS[w.kind]) w[f] = typeof r[f] === 'string' ? (r[f] as string) : null
    out.push(bound(w))
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

import { create } from 'zustand'

/** A navigation an agent asked for: the per-user `ui-navigate` WS frame,
 *  sent by the `ui_open` MCP tool and the voice assistant's `show_view`
 *  (and the view tools' `show`). The server has already resolved names to
 *  ids and picked which of the user's tabs should follow. */
export type UiNavTarget =
  | { target: 'session'; id: string }
  | { target: 'project'; id: string }
  | { target: 'folder'; id: string }
  | { target: 'view'; id: string }
  | { target: 'card'; id: string; projectId: string }
  | { target: 'settings'; sub: string | null }
  | { target: 'plugin_page'; plugin: string; item: string; query: string }
  | { target: 'page'; page: UiPage }

/** A parsed navigation plus who asked and what to call it in the notice. */
export interface UiNavigate {
  nav: UiNavTarget
  /** Display name of what was opened ("Stashify", "Settings"). */
  label: string
  /** The session that asked, for "‹source› opened ‹page›". */
  sourceName: string
}

/** Top-level pages `target: page` may open (server `PAGES`), mapped to
 *  App's `View` names. */
export const UI_PAGE_VIEWS = {
  sessions: 'sessions',
  projects: 'projects',
  folders: 'folders',
  views: 'views',
  settings: 'settings',
  reports: 'reports',
  repeating_tasks: 'repeatingTasks',
  terminals: 'terminals',
  review: 'docReview',
  usage: 'usage',
  agents: 'agents',
} as const

export type UiPage = keyof typeof UI_PAGE_VIEWS

const PAGE_LABELS: Record<UiPage, string> = {
  sessions: 'Sessions',
  projects: 'Projects',
  folders: 'Folders',
  views: 'Views',
  settings: 'Settings',
  reports: 'Reports',
  repeating_tasks: 'Repeating Tasks',
  terminals: 'Terminals',
  review: 'Document Review',
  usage: 'Usage',
  agents: 'Agents',
}

const SLUG = /^[A-Za-z0-9._-]{1,100}$/

/** Validate the frame payload; `null` for anything malformed. */
export function parseUiNavigate(data: unknown): UiNavigate | null {
  if (!data || typeof data !== 'object') return null
  const d = data as Record<string, unknown>
  const id = typeof d.id === 'string' && d.id ? d.id : null
  const name = typeof d.name === 'string' && d.name ? d.name : null
  const source = (d.source ?? {}) as Record<string, unknown>
  const sourceName = typeof source.name === 'string' && source.name ? source.name : 'An agent'
  const nav = ((): UiNavTarget | null => {
    switch (d.target) {
      case 'session':
      case 'project':
      case 'folder':
      case 'view':
        return id ? { target: d.target, id } : null
      case 'card':
        return id && typeof d.project_id === 'string' && d.project_id
          ? { target: 'card', id, projectId: d.project_id }
          : null
      case 'settings':
        return {
          target: 'settings',
          sub: typeof d.sub === 'string' && SLUG.test(d.sub) ? d.sub : null,
        }
      case 'plugin_page':
        return typeof d.plugin === 'string' &&
          SLUG.test(d.plugin) &&
          typeof d.item === 'string' &&
          SLUG.test(d.item)
          ? {
              target: 'plugin_page',
              plugin: d.plugin,
              item: d.item,
              query: typeof d.query === 'string' ? d.query : '',
            }
          : null
      case 'page':
        return typeof d.page === 'string' && d.page in UI_PAGE_VIEWS
          ? { target: 'page', page: d.page as UiPage }
          : null
      default:
        return null
    }
  })()
  if (!nav) return null
  const label =
    nav.target === 'page'
      ? PAGE_LABELS[nav.page]
      : nav.target === 'settings'
        ? nav.sub
          ? `Settings → ${nav.sub}`
          : 'Settings'
        : nav.target === 'plugin_page'
          ? `${nav.plugin} ${nav.item}`
          : (name ?? nav.target)
  return { nav, label, sourceName }
}

/** A card the assistant asked to show. The board it lives on mounts (and
 *  loads its cards) after the navigation, so KanbanBoard claims the card
 *  from here once its cards are in. */
interface VoiceNavState {
  pendingCard: { projectId: string; cardId: string } | null
  requestCard: (projectId: string, cardId: string) => void
  /** The pending card id for `projectId`, cleared on read. */
  takePendingCard: (projectId: string) => string | null
}

export const useVoiceNavStore = create<VoiceNavState>((set, get) => ({
  pendingCard: null,
  requestCard: (projectId, cardId) => set({ pendingCard: { projectId, cardId } }),
  takePendingCard: (projectId) => {
    const p = get().pendingCard
    if (!p || p.projectId !== projectId) return null
    set({ pendingCard: null })
    return p.cardId
  },
}))

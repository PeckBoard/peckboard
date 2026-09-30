import { create } from 'zustand'

/** A navigation the voice assistant asked for (the `voice-navigate` WS
 *  event, emitted by the `show_view` MCP tool). The server has already
 *  resolved spoken names to ids. */
export type VoiceNavTarget =
  | { target: 'session'; id: string }
  | { target: 'project'; id: string }
  | { target: 'folder'; id: string }
  | { target: 'card'; id: string; projectId: string }
  | { target: 'page'; page: VoicePage }

/** Top-level pages `show_view` may open (server `PAGES`), mapped to App's
 *  `View` names. */
export const VOICE_PAGE_VIEWS = {
  sessions: 'sessions',
  projects: 'projects',
  folders: 'folders',
  settings: 'settings',
  reports: 'reports',
  repeating_tasks: 'repeatingTasks',
  usage: 'usage',
  agents: 'agents',
} as const

export type VoicePage = keyof typeof VOICE_PAGE_VIEWS

/** Validate the event payload; `null` for anything malformed. */
export function parseVoiceNavigate(data: unknown): VoiceNavTarget | null {
  if (!data || typeof data !== 'object') return null
  const d = data as Record<string, unknown>
  const id = typeof d.id === 'string' && d.id ? d.id : null
  switch (d.target) {
    case 'session':
    case 'project':
    case 'folder':
      return id ? { target: d.target, id } : null
    case 'card':
      return id && typeof d.project_id === 'string' && d.project_id
        ? { target: 'card', id, projectId: d.project_id }
        : null
    case 'page':
      return typeof d.page === 'string' && d.page in VOICE_PAGE_VIEWS
        ? { target: 'page', page: d.page as VoicePage }
        : null
    default:
      return null
  }
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

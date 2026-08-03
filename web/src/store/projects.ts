import { create } from 'zustand'
import type { Project, Card } from '../types/api'
import { authedFetch } from './auth'
import { useTabsStore } from './tabs'

/** `POST /api/projects` rejects a body missing `name`, `folder_id`, or
 *  `workflow` before the handler runs (no serde defaults), so encode the
 *  requirement in the type. */
export type CreateProjectInput = Partial<Project> & Pick<Project, 'name' | 'folder_id' | 'workflow'>
export interface CardReport {
  folder: string
  file: string
  title: string
  date: string
}

export interface PendingQuestionItem {
  question: string
  header?: string
  multiSelect?: boolean
  options?: string[]
  optionObjects?: { label: string; description?: string }[]
}

export interface PendingQuestion {
  eventId: string
  sessionId: string
  ts: number
  questions: PendingQuestionItem[]
  cardId: string | null
  cardTitle: string | null
  cardDescription: string | null
}

interface ProjectsState {
  projects: Project[]
  /** True once `fetchProjects` has completed successfully at least
   *  once. See the matching flag in `useSessionsStore` for why this
   *  matters — without it, the empty initial state is indistinguishable
   *  from "no projects exist". */
  projectsLoaded: boolean
  /** Non-empty when the last `fetchProjects` failed. An empty list is a claim
   *  ("you have no projects") and a failed request cannot make it — the sidebar
   *  shows an error + Retry instead. */
  projectsError: string
  activeProjectId: string | null
  cards: Card[]
  /** Non-empty when the last `fetchCards` failed — same reasoning as
   *  `projectsError`, for the board's per-step "No cards in …" placeholders. */
  cardsError: string
  /** Project id whose cards are in `cards`, set once a `fetchCards` for it
   *  has succeeded. The board's “No cards yet” state is a claim about a
   *  known-empty project — without this it would flash while the first
   *  fetch is still in flight. */
  cardsLoadedProjectId: string | null
  cardReportsByCard: Record<string, CardReport[]>
  pendingQuestionsByProject: Record<string, PendingQuestion[]>
  fetchProjects: () => Promise<void>
  createProject: (data: CreateProjectInput) => Promise<Project>
  updateProject: (id: string, data: Partial<Project>) => Promise<Project>
  deleteProject: (id: string) => Promise<void>
  setActiveProject: (id: string | null) => void
  fetchCards: (projectId: string) => Promise<void>
  createCard: (projectId: string, data: Partial<Card>) => Promise<Card>
  updateCard: (projectId: string, cardId: string, data: Partial<Card>) => Promise<Card>
  deleteCard: (projectId: string, cardId: string) => Promise<void>
  fetchCardReports: (projectId: string, cardId: string) => Promise<void>
  clearCardReports: (cardId: string) => void
  fetchPendingQuestions: (projectId: string) => Promise<void>
}

/** Monotonic token for `fetchCards`. A response is only committed while its
 *  request is still the newest one — otherwise switching projects while a slow
 *  fetch is in flight lets the old project's cards land on the new project's
 *  board (`cards` is global and the board doesn't filter by `project_id`), and
 *  card actions then POST to `/api/projects/<new>/cards/<old-card-id>` and 404.
 *  Same pattern as `sessionSearchRequestId` in `store/sessions.ts`. */
let cardsRequestId = 0

export const useProjectsStore = create<ProjectsState>((set) => ({
  projects: [],
  projectsLoaded: false,
  activeProjectId: null,
  cards: [],
  cardReportsByCard: {},
  pendingQuestionsByProject: {},
  projectsError: '',
  cardsError: '',
  cardsLoadedProjectId: null,

  fetchProjects: async () => {
    try {
      const res = await authedFetch('/api/projects')
      if (!res.ok) {
        set({ projectsError: 'Couldn’t load projects.' })
        return
      }
      const projects: Project[] = await res.json()
      set({ projects, projectsLoaded: true, projectsError: '' })
    } catch {
      set({ projectsError: 'Couldn’t load projects.' })
    }
  },

  createProject: async (data: CreateProjectInput) => {
    const res = await authedFetch('/api/projects', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(data),
    })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to create project' }))
      throw new Error(err.error || 'Failed to create project')
    }
    const project: Project = await res.json()
    set((s) => ({ projects: [...s.projects, project] }))
    return project
  },

  updateProject: async (id: string, data: Partial<Project>) => {
    const res = await authedFetch(`/api/projects/${id}`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(data),
    })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to update project' }))
      throw new Error(err.error || 'Failed to update project')
    }
    const project: Project = await res.json()
    set((s) => ({ projects: s.projects.map((p) => (p.id === id ? project : p)) }))
    return project
  },

  deleteProject: async (id: string) => {
    const res = await authedFetch(`/api/projects/${id}`, { method: 'DELETE' })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to delete project' }))
      throw new Error(err.error || 'Failed to delete project')
    }
    set((s) => ({
      projects: s.projects.filter((p) => p.id !== id),
      activeProjectId: s.activeProjectId === id ? null : s.activeProjectId,
      cards: s.activeProjectId === id ? [] : s.cards,
    }))
    // Same reason as deleteSession: drop the orphan tab so it doesn't
    // render with the "Project" fallback label.
    useTabsStore.getState().removeTabsForItem('project', id)
  },

  setActiveProject: (id: string | null) => {
    const current = useProjectsStore.getState().activeProjectId
    // Don't clear cards if re-selecting the same project
    if (id === current) return
    // Invalidate any in-flight `fetchCards` for the project we're leaving,
    // even if no new fetch has started yet.
    cardsRequestId++
    set({ activeProjectId: id, cards: [], cardsError: '', cardsLoadedProjectId: null })
  },

  fetchCards: async (projectId: string) => {
    const requestId = ++cardsRequestId
    try {
      const res = await authedFetch(`/api/projects/${projectId}/cards`)
      if (requestId !== cardsRequestId) return
      if (!res.ok) {
        set({ cardsError: 'Couldn’t load cards.' })
        return
      }
      const cards: Card[] = await res.json()
      if (requestId !== cardsRequestId) return
      set({ cards, cardsError: '', cardsLoadedProjectId: projectId })
    } catch {
      if (requestId === cardsRequestId) set({ cardsError: 'Couldn’t load cards.' })
    }
  },

  createCard: async (projectId: string, data: Partial<Card>) => {
    const res = await authedFetch(`/api/projects/${projectId}/cards`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(data),
    })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to create card' }))
      throw new Error(err.error || 'Failed to create card')
    }
    const card: Card = await res.json()
    // Upsert to avoid duplicates with WebSocket card-update broadcast
    set((s) => {
      const exists = s.cards.some((c) => c.id === card.id)
      if (exists) return { cards: s.cards.map((c) => (c.id === card.id ? card : c)) }
      return { cards: [...s.cards, card] }
    })
    return card
  },

  updateCard: async (projectId: string, cardId: string, data: Partial<Card>) => {
    const res = await authedFetch(`/api/projects/${projectId}/cards/${cardId}`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(data),
    })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to update card' }))
      throw new Error(err.error || 'Failed to update card')
    }
    const card: Card = await res.json()
    set((s) => ({ cards: s.cards.map((c) => (c.id === cardId ? card : c)) }))
    return card
  },

  deleteCard: async (projectId: string, cardId: string) => {
    const res = await authedFetch(`/api/projects/${projectId}/cards/${cardId}`, {
      method: 'DELETE',
    })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to delete card' }))
      throw new Error(err.error || 'Failed to delete card')
    }
    set((s) => {
      const { [cardId]: _drop, ...remaining } = s.cardReportsByCard
      void _drop
      return {
        cards: s.cards.filter((c) => c.id !== cardId),
        cardReportsByCard: remaining,
      }
    })
  },

  fetchCardReports: async (projectId: string, cardId: string) => {
    try {
      const res = await authedFetch(`/api/projects/${projectId}/cards/${cardId}/reports`)
      if (!res.ok) {
        set((s) => ({
          cardReportsByCard: { ...s.cardReportsByCard, [cardId]: [] },
        }))
        return
      }
      const data = await res.json()
      const reports: CardReport[] = data?.reports ?? []
      set((s) => ({
        cardReportsByCard: { ...s.cardReportsByCard, [cardId]: reports },
      }))
    } catch {
      set((s) => ({
        cardReportsByCard: { ...s.cardReportsByCard, [cardId]: [] },
      }))
    }
  },

  clearCardReports: (cardId: string) => {
    set((s) => {
      const { [cardId]: _drop, ...remaining } = s.cardReportsByCard
      void _drop
      return { cardReportsByCard: remaining }
    })
  },

  fetchPendingQuestions: async (projectId: string) => {
    try {
      const res = await authedFetch(`/api/projects/${projectId}/pending-questions`)
      if (!res.ok) return
      const data = await res.json()
      const questions: PendingQuestion[] = data?.questions ?? []
      set((s) => ({
        pendingQuestionsByProject: { ...s.pendingQuestionsByProject, [projectId]: questions },
      }))
    } catch {
      /* ignore */
    }
  },
}))

import { create } from 'zustand'
import type { Project, Card } from '../types/api'
import { authedFetch } from './auth'
import { useTabsStore } from './tabs'

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
  activeProjectId: string | null
  cards: Card[]
  cardReportsByCard: Record<string, CardReport[]>
  pendingQuestionsByProject: Record<string, PendingQuestion[]>
  fetchProjects: () => Promise<void>
  createProject: (data: Partial<Project>) => Promise<Project>
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

export const useProjectsStore = create<ProjectsState>((set) => ({
  projects: [],
  projectsLoaded: false,
  activeProjectId: null,
  cards: [],
  cardReportsByCard: {},
  pendingQuestionsByProject: {},

  fetchProjects: async () => {
    const res = await authedFetch('/api/projects')
    if (res.ok) {
      const projects: Project[] = await res.json()
      set({ projects, projectsLoaded: true })
    }
  },

  createProject: async (data: Partial<Project>) => {
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
    set({ activeProjectId: id, cards: [] })
  },

  fetchCards: async (projectId: string) => {
    const res = await authedFetch(`/api/projects/${projectId}/cards`)
    if (res.ok) {
      const cards: Card[] = await res.json()
      set({ cards })
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

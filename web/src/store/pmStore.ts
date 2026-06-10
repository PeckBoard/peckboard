import { create } from 'zustand'
import type { PmDecision, PmPendingQuestion } from '../types/api'
import { authedFetch } from './auth'
import { useWsStore } from './ws'

// Stable empty sentinels so zustand selectors over the keyed maps stay
// referentially stable (same reason as EMPTY_EVENTS in chat/events.ts).
export const EMPTY_PM_DECISIONS: PmDecision[] = []
export const EMPTY_PM_QUESTIONS: PmPendingQuestion[] = []

interface PmState {
  decisionsByProject: Record<string, PmDecision[]>
  pendingQuestionsByProject: Record<string, PmPendingQuestion[]>
  /** Server-reported pending count per project. Kept separately from the
   *  questions list because the `pm-decisions-changed` broadcast carries
   *  only the count — it can be current even before the lists are fetched. */
  pendingCountByProject: Record<string, number>
  loading: boolean
  fetchPmState: (projectId: string) => Promise<void>
  answerQuestion: (projectId: string, questionId: string, answer: string) => Promise<PmDecision>
  editDecision: (
    projectId: string,
    decisionId: string,
    payload: { question?: string; answer: string },
  ) => Promise<PmDecision>
  /** Apply an incoming `pm-decisions-changed` broadcast: trust the count
   *  from the payload immediately, refetch the lists if we hold them. */
  applyPmChange: (projectId: string, pendingCount: number) => void
  hasPendingQuestions: (projectId: string) => boolean
  pendingCount: (projectId: string) => number
}

function upsertDecision(decisions: PmDecision[], decision: PmDecision): PmDecision[] {
  const idx = decisions.findIndex((d) => d.id === decision.id)
  if (idx === -1) return [decision, ...decisions]
  const next = decisions.slice()
  next[idx] = decision
  return next
}

export const usePmStore = create<PmState>((set, get) => ({
  decisionsByProject: {},
  pendingQuestionsByProject: {},
  pendingCountByProject: {},
  loading: false,

  fetchPmState: async (projectId: string) => {
    // `pm-decisions-changed` is broadcast with session_id = project id and
    // is NOT in the ws handler's global whitelist — the server only
    // delivers it when some client is subscribed to that id. Subscribing
    // here (idempotent, auto-resumed on reconnect) is what turns the
    // broadcasts on for this project.
    useWsStore.getState().subscribe(projectId)
    set({ loading: true })
    try {
      const [decisionsRes, questionsRes] = await Promise.all([
        authedFetch(`/api/projects/${projectId}/pm/decisions`),
        authedFetch(`/api/projects/${projectId}/pm/questions`),
      ])
      if (decisionsRes.ok) {
        const data = await decisionsRes.json()
        const decisions: PmDecision[] = data?.decisions ?? []
        set((s) => ({
          decisionsByProject: { ...s.decisionsByProject, [projectId]: decisions },
          pendingCountByProject: {
            ...s.pendingCountByProject,
            [projectId]: data?.pending_count ?? 0,
          },
        }))
      }
      if (questionsRes.ok) {
        const data = await questionsRes.json()
        const questions: PmPendingQuestion[] = data?.questions ?? []
        set((s) => ({
          pendingQuestionsByProject: { ...s.pendingQuestionsByProject, [projectId]: questions },
        }))
      }
    } finally {
      set({ loading: false })
    }
  },

  answerQuestion: async (projectId: string, questionId: string, answer: string) => {
    const res = await authedFetch(`/api/projects/${projectId}/pm/questions/${questionId}/answer`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ answer }),
    })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to answer question' }))
      throw new Error(err.error || 'Failed to answer question')
    }
    const data = await res.json()
    const decision: PmDecision = data.decision
    set((s) => ({
      pendingQuestionsByProject: {
        ...s.pendingQuestionsByProject,
        [projectId]: (s.pendingQuestionsByProject[projectId] ?? []).filter(
          (q) => q.id !== questionId,
        ),
      },
      // Upsert to dedupe against the refetch the WS broadcast triggers.
      decisionsByProject: {
        ...s.decisionsByProject,
        [projectId]: upsertDecision(s.decisionsByProject[projectId] ?? [], decision),
      },
      pendingCountByProject: {
        ...s.pendingCountByProject,
        [projectId]: data.pending_count ?? Math.max(0, get().pendingCount(projectId) - 1),
      },
    }))
    return decision
  },

  editDecision: async (
    projectId: string,
    decisionId: string,
    payload: { question?: string; answer: string },
  ) => {
    const res = await authedFetch(`/api/projects/${projectId}/pm/decisions/${decisionId}`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload),
    })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Failed to edit decision' }))
      throw new Error(err.error || 'Failed to edit decision')
    }
    const data = await res.json()
    const decision: PmDecision = data.decision
    const supersededId: string = data.superseded_decision_id ?? decisionId
    set((s) => ({
      decisionsByProject: {
        ...s.decisionsByProject,
        [projectId]: upsertDecision(
          (s.decisionsByProject[projectId] ?? []).filter((d) => d.id !== supersededId),
          decision,
        ),
      },
    }))
    return decision
  },

  applyPmChange: (projectId: string, pendingCount: number) => {
    set((s) => ({
      pendingCountByProject: { ...s.pendingCountByProject, [projectId]: pendingCount },
    }))
    // Only refetch lists we already hold — projects whose PM state was
    // never opened just track the count from the broadcast.
    if (projectId in get().decisionsByProject) {
      get()
        .fetchPmState(projectId)
        .catch(() => {})
    }
  },

  hasPendingQuestions: (projectId: string) => get().pendingCount(projectId) > 0,

  pendingCount: (projectId: string) => {
    const { pendingCountByProject, pendingQuestionsByProject } = get()
    return pendingCountByProject[projectId] ?? pendingQuestionsByProject[projectId]?.length ?? 0
  },
}))

import { create } from 'zustand'
import type { PmDecision } from '../types/api'
import { authedFetch } from './auth'
import { useWsStore } from './ws'

// Stable empty sentinels so zustand selectors over the keyed maps stay
// referentially stable (same reason as EMPTY_EVENTS in chat/events.ts).
export const EMPTY_PM_DECISIONS: PmDecision[] = []
export const EMPTY_PM_QUESTIONS: PmDecision[] = []

interface PmState {
  /** Decided/superseded decisions per project (the experts plugin's
   *  `decisions` list). */
  decisionsByProject: Record<string, PmDecision[]>
  /** Pending decisions per project (the plugin's `pending` list): each is
   *  a `PmDecision` with `status: 'pending'` and `decision: null`, whose
   *  `title` is the question text awaiting a user answer. */
  pendingQuestionsByProject: Record<string, PmDecision[]>
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
    payload: { title?: string; decision: string },
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
      const res = await authedFetch(
        `/api/plugin-ui/pm/decisions?project_id=${encodeURIComponent(projectId)}`,
      )
      if (res.ok) {
        const data = await res.json()
        const decisions: PmDecision[] = data?.decisions ?? []
        const pending: PmDecision[] = data?.pending ?? []
        set((s) => ({
          decisionsByProject: { ...s.decisionsByProject, [projectId]: decisions },
          pendingQuestionsByProject: { ...s.pendingQuestionsByProject, [projectId]: pending },
          pendingCountByProject: {
            ...s.pendingCountByProject,
            [projectId]: pending.length,
          },
        }))
      }
    } finally {
      set({ loading: false })
    }
  },

  answerQuestion: async (projectId: string, questionId: string, answer: string) => {
    const res = await authedFetch('/api/plugin-ui/pm/answer', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ project_id: projectId, question_id: questionId, answer }),
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
    payload: { title?: string; decision: string },
  ) => {
    const res = await authedFetch(`/api/plugin-ui/pm/decisions/${decisionId}`, {
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
    set((s) => ({
      decisionsByProject: {
        ...s.decisionsByProject,
        // Replace the edited decision in place; the plugin returns the
        // resulting (possibly superseding) decision under the same id.
        [projectId]: upsertDecision(
          (s.decisionsByProject[projectId] ?? []).filter((d) => d.id !== decisionId),
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
